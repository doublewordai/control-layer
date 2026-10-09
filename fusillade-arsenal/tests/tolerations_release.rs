//! Scheduling-tolerations release: the cutoff computation (leader election,
//! throughput from tolerated completions, the cutoff itself) and the claim's
//! decision, against a real database.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use fusillade_arsenal::manager::{DaemonStorage, Storage};
use fusillade_arsenal::{PostgresRequestManager, PostgresStorageConfig, TestDbPools};
use fusillade_core::release::{ReleaseCutoffParams, TolerationsReleaseSettings};
use fusillade_core::request::{CreateFlexInput, DaemonId, DispatchTolerations, TolerationsRelease};
use sqlx::PgPool;
use uuid::Uuid;

type Manager = PostgresRequestManager<TestDbPools>;

async fn manager(pool: &PgPool) -> Arc<Manager> {
    Arc::new(PostgresRequestManager::new(
        TestDbPools::new(pool.clone()).await.unwrap(),
        PostgresStorageConfig::default(),
    ))
}

const PARAMS: ReleaseCutoffParams = ReleaseCutoffParams {
    refresh_interval_secs: 300.0,
    window_secs: 900.0,
    min_samples: 20,
    safety_margin: 0.1,
};

/// `count` pending flex requests (1h window) for `model`.
async fn flex(manager: &Manager, model: &str, count: usize) -> Vec<Uuid> {
    let mut ids = Vec::with_capacity(count);
    for _ in 0..count {
        let id = Uuid::new_v4();
        manager
            .create_flex(CreateFlexInput {
                request_id: id,
                body: r#"{"model":"m"}"#.to_string(),
                model: model.to_string(),
                endpoint: "http://localhost".to_string(),
                method: "POST".to_string(),
                path: "/v1/chat/completions".to_string(),
                api_key: "k".to_string(),
                created_by: "u".to_string(),
                metadata: None,
            })
            .await
            .unwrap();
        ids.push(id);
    }
    ids
}

/// A finished request, as if dispatched `ago_secs` ago for `busy_secs`.
async fn completed(
    pool: &PgPool,
    model: &str,
    tolerated: Option<bool>,
    busy_secs: f64,
    ago_secs: f64,
) {
    sqlx::query(
        "INSERT INTO requests (id, model, state, response_status, response_body, created_by,
                               service_tier, started_at, completed_at, dispatched_tolerated)
         VALUES ($1, $2, 'completed', 200, '{}', 'u', 'flex',
                 NOW() - make_interval(secs => $4 + $3), NOW() - make_interval(secs => $4), $5)",
    )
    .bind(Uuid::new_v4())
    .bind(model)
    .bind(busy_secs)
    .bind(ago_secs)
    .bind(tolerated)
    .execute(pool)
    .await
    .unwrap();
}

/// Move pending requests into processing, as dispatched work in flight.
async fn in_flight(pool: &PgPool, ids: &[Uuid]) {
    sqlx::query(
        "UPDATE requests SET state = 'processing', daemon_id = $2, claimed_at = NOW(), started_at = NOW()
         WHERE id = ANY($1)",
    )
    .bind(ids)
    .bind(Uuid::new_v4())
    .execute(pool)
    .await
    .unwrap();
}

async fn make_cutoffs_stale(pool: &PgPool) {
    sqlx::query("UPDATE model_release_cutoffs SET computed_at = NOW() - INTERVAL '10 minutes'")
        .execute(pool)
        .await
        .unwrap();
}

#[sqlx::test]
async fn throughput_counts_only_tolerated_completions_and_ignores_idle_time(pool: PgPool) {
    let storage = manager(&pool).await;
    let pending = flex(&storage, "m", 10).await;
    in_flight(&pool, &pending[..4]).await;

    // 30 tolerated completions of 2s each, spread over 10 minutes with idle
    // gaps between them: 30 / 60 busy-seconds = 0.5 per in-flight slot.
    for i in 0..30 {
        completed(&pool, "m", Some(true), 2.0, 20.0 * i as f64).await;
    }
    // Released or undecided completions (possibly served elsewhere, fast)
    // and tolerated ones outside the 15-minute window must not count.
    for _ in 0..50 {
        completed(&pool, "m", Some(false), 0.1, 30.0).await;
        completed(&pool, "m", None, 0.1, 30.0).await;
    }
    for _ in 0..50 {
        completed(&pool, "m", Some(true), 0.1, 3_600.0).await;
    }

    let cutoffs = storage
        .refresh_release_cutoffs(PARAMS)
        .await
        .unwrap()
        .unwrap();
    let m = cutoffs.iter().find(|c| c.model == "m").unwrap();
    assert_eq!(m.samples, 30);
    // 0.5 per slot times the 4 requests in flight.
    assert!((m.throughput - 2.0).abs() < 1e-6, "{}", m.throughput);
    assert_eq!(m.backlog_requests, 10, "pending plus in flight");
    // 10 requests at 2/s is 5 seconds: on track, no cutoff.
    assert_eq!(m.release_before_deadline, None);

    let row: (Option<chrono::DateTime<chrono::Utc>>, f64, i64, i64) = sqlx::query_as(
        "SELECT release_before_deadline, throughput, backlog_requests, samples
         FROM model_release_cutoffs WHERE model = 'm'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, None);
    assert!((row.1 - 2.0).abs() < 1e-6);
    assert_eq!((row.2, row.3), (10, 30));
}

#[sqlx::test]
async fn a_slow_model_gets_a_cutoff_and_a_cold_one_does_not(pool: PgPool) {
    let storage = manager(&pool).await;
    // 600 flex requests due in about an hour, 1 in flight.
    let slow = flex(&storage, "slow", 600).await;
    in_flight(&pool, &slow[..1]).await;
    // Each takes 30s while tolerated: 1 in flight -> 1/30 per second, so the
    // hour's work takes about 5 hours. Release everything due within the hour.
    for i in 0..20 {
        completed(&pool, "slow", Some(true), 30.0, 30.0 * i as f64).await;
    }
    // A model with work but too few tolerated completions stays floor-only.
    flex(&storage, "cold", 5).await;
    completed(&pool, "cold", Some(true), 1.0, 10.0).await;

    let cutoffs = storage
        .refresh_release_cutoffs(PARAMS)
        .await
        .unwrap()
        .unwrap();
    let slow = cutoffs.iter().find(|c| c.model == "slow").unwrap();
    let computed_at = slow.computed_at;
    let cutoff = slow
        .release_before_deadline
        .expect("the slow model is behind");
    assert_eq!(cutoff, computed_at + chrono::Duration::seconds(3_600));
    // The claim reads the stored cutoff, not the returned one.
    let stored: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT release_before_deadline FROM model_release_cutoffs WHERE model = 'slow'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored, Some(computed_at + chrono::Duration::seconds(3_600)));
    // Every daemon publishes the metrics from the stored rows.
    let status = storage.release_cutoff_status().await.unwrap();
    let slow_status = status.iter().find(|s| s.model == "slow").unwrap();
    assert_eq!(slow_status.backlog_requests, slow.backlog_requests);
    assert!((slow_status.throughput - slow.throughput).abs() < 1e-9);
    assert!(slow_status.age_secs >= 0.0);
    let cold = cutoffs.iter().find(|c| c.model == "cold").unwrap();
    assert_eq!(cold.release_before_deadline, None);
    assert_eq!(cold.throughput, 0.0);
    assert_eq!(cold.samples, 1);

    // Models with no work lose their row on the next computation.
    sqlx::query("DELETE FROM requests WHERE model = 'cold'")
        .execute(&pool)
        .await
        .unwrap();
    make_cutoffs_stale(&pool).await;
    let cutoffs = storage
        .refresh_release_cutoffs(PARAMS)
        .await
        .unwrap()
        .unwrap();
    assert!(cutoffs.iter().all(|c| c.model != "cold"));
    let cold_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM model_release_cutoffs WHERE model = 'cold'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(cold_rows, 0);
}

#[sqlx::test]
async fn exactly_one_daemon_computes_per_interval_and_another_takes_over(pool: PgPool) {
    let (a, b) = (manager(&pool).await, manager(&pool).await);
    flex(&a, "m", 3).await;

    // Concurrent ticks: exactly one computes.
    let (ra, rb) = tokio::join!(
        a.refresh_release_cutoffs(PARAMS),
        b.refresh_release_cutoffs(PARAMS)
    );
    let computed = [ra.unwrap(), rb.unwrap()]
        .iter()
        .filter(|result| result.is_some())
        .count();
    assert_eq!(
        computed, 1,
        "one daemon computes, the other finds it locked or fresh"
    );

    // Fresh cutoffs: nobody recomputes within the interval.
    assert!(a.refresh_release_cutoffs(PARAMS).await.unwrap().is_none());
    assert!(b.refresh_release_cutoffs(PARAMS).await.unwrap().is_none());

    // While another session holds the lock (a leader mid-computation), no one
    // else computes even though the cutoffs are stale.
    make_cutoffs_stale(&pool).await;
    let mut holder = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('fusillade.release_cutoffs', 0))")
        .execute(&mut *holder)
        .await
        .unwrap();
    assert!(b.refresh_release_cutoffs(PARAMS).await.unwrap().is_none());

    // The holder dies (its transaction ends): the lock is released and the
    // next tick of any daemon takes over.
    holder.rollback().await.unwrap();
    assert!(b.refresh_release_cutoffs(PARAMS).await.unwrap().is_some());
}

fn settings(sla_release_enabled: bool) -> TolerationsReleaseSettings {
    TolerationsReleaseSettings {
        tolerations_enabled: true,
        sla_release_enabled,
        cutoff_max_age_secs: 900.0,
    }
}

async fn claim(
    manager: &Manager,
    model: &str,
    limit: usize,
) -> HashMap<Uuid, Option<DispatchTolerations>> {
    manager
        .claim_batchless_requests(
            limit,
            DaemonId(Uuid::new_v4()),
            &HashMap::from([(model.to_string(), limit)]),
            &HashMap::new(),
            &HashSet::new(),
        )
        .await
        .unwrap()
        .into_iter()
        .map(|request| (request.data.id.0, request.state.tolerations))
        .collect()
}

async fn recorded(pool: &PgPool, id: Uuid) -> Option<bool> {
    sqlx::query_scalar("SELECT dispatched_tolerated FROM requests WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn set_cutoff(pool: &PgPool, model: &str, release_before_secs: i64, age_secs: i64) {
    sqlx::query(
        "INSERT INTO model_release_cutoffs
             (model, release_before_deadline, throughput, backlog_requests, samples, computed_at)
         VALUES ($1, NOW() + make_interval(secs => $2), 1.0, 100, 100, NOW() - make_interval(secs => $3))
         ON CONFLICT (model) DO UPDATE SET
             release_before_deadline = EXCLUDED.release_before_deadline,
             computed_at = EXCLUDED.computed_at",
    )
    .bind(model)
    .bind(release_before_secs as f64)
    .bind(age_secs as f64)
    .execute(pool)
    .await
    .unwrap();
}

#[sqlx::test]
async fn the_claim_applies_the_cutoff_and_falls_back_to_the_floor(pool: PgPool) {
    let (a, b) = (manager(&pool).await, manager(&pool).await);
    a.configure_tolerations_release(settings(true));
    b.configure_tolerations_release(settings(true));

    // Flex requests are due an hour after creation; the cutoff is at 2h.
    set_cutoff(&pool, "m", 7_200, 0).await;
    let ids = flex(&a, "m", 6).await;
    // Two replicas claim halves of the same queue: same decision on both.
    let mut decisions = claim(&a, "m", 3).await;
    decisions.extend(claim(&b, "m", 3).await);
    assert_eq!(decisions.len(), 6);
    for id in &ids {
        assert_eq!(
            decisions[id],
            Some(DispatchTolerations::Release(
                TolerationsRelease::SlaProjection
            ))
        );
        assert_eq!(
            recorded(&pool, *id).await,
            Some(false),
            "released: not tolerated"
        );
    }

    // Due after the cutoff: tolerations kept.
    set_cutoff(&pool, "m", 1_800, 0).await;
    let later = flex(&a, "m", 2).await;
    let decisions = claim(&b, "m", 2).await;
    for id in &later {
        assert_eq!(decisions[id], Some(DispatchTolerations::Keep));
        assert_eq!(recorded(&pool, *id).await, Some(true));
    }

    // A stale cutoff (older than 3 intervals) is ignored: floor only.
    set_cutoff(&pool, "m", 7_200, 1_000).await;
    let stale = flex(&a, "m", 1).await;
    assert_eq!(
        claim(&a, "m", 1).await[&stale[0]],
        Some(DispatchTolerations::Keep)
    );

    // The floor applies whatever the cutoff: inside the 1h ramp (~9.9 min)...
    let ramp = flex(&a, "m", 1).await;
    sqlx::query("UPDATE requests SET created_at = NOW() - INTERVAL '55 minutes' WHERE id = $1")
        .bind(ramp[0])
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        claim(&a, "m", 1).await[&ramp[0]],
        Some(DispatchTolerations::Release(TolerationsRelease::Ramp))
    );
    // ...and past the deadline.
    let late = flex(&a, "m", 1).await;
    sqlx::query("UPDATE requests SET created_at = NOW() - INTERVAL '2 hours' WHERE id = $1")
        .bind(late[0])
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        claim(&a, "m", 1).await[&late[0]],
        Some(DispatchTolerations::Release(
            TolerationsRelease::PastDeadline
        ))
    );

    // With the projection off, a fresh cutoff is ignored.
    a.configure_tolerations_release(settings(false));
    set_cutoff(&pool, "m", 7_200, 0).await;
    let off = flex(&a, "m", 1).await;
    assert_eq!(
        claim(&a, "m", 1).await[&off[0]],
        Some(DispatchTolerations::Keep)
    );

    // Without tolerations the claim makes no decision and records nothing.
    a.configure_tolerations_release(TolerationsReleaseSettings {
        tolerations_enabled: false,
        sla_release_enabled: true,
        cutoff_max_age_secs: 900.0,
    });
    let none = flex(&a, "m", 1).await;
    assert_eq!(claim(&a, "m", 1).await[&none[0]], None);
    assert_eq!(recorded(&pool, none[0]).await, None);

    // Unconfigured storage decides nothing either.
    let fresh = manager(&pool).await;
    let unconfigured = flex(&fresh, "m", 1).await;
    assert_eq!(claim(&fresh, "m", 1).await[&unconfigured[0]], None);
}
