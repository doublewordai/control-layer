//! The dispatch hold against a real database: the claim's hold/release
//! decision from the parameters a daemon passes, the shared `hold_stats`
//! counters, and the inputs every daemon reads back to decide.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Duration, DurationRound, Utc};
use fusillade_arsenal::manager::{DaemonStorage, Storage};
use fusillade_arsenal::{PostgresRequestManager, PostgresStorageConfig, TestDbPools};
use fusillade_core::batch::{BatchInput, RequestTemplateInput};
use fusillade_core::manager::{ModelFilter, ModelFilterState};
use fusillade_core::release::{
    HoldClaim, HoldParams, HoldStatsDelta, ModelHold, decide_model_hold,
};
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

const PARAMS: HoldParams = HoldParams {
    window_secs: 900.0,
    safety_margin: 0.2,
    min_samples: 20,
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

/// Decisions for `models`, computed now.
fn decisions(models: &[(&str, ModelHold)]) -> HoldClaim {
    HoldClaim {
        models: models
            .iter()
            .map(|(model, hold)| (model.to_string(), *hold))
            .collect(),
        computed_at: Utc::now(),
        stale_after: Duration::minutes(5),
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

async fn set_created_ago(pool: &PgPool, id: Uuid, minutes: i64) {
    sqlx::query("UPDATE requests SET created_at = NOW() - make_interval(mins => $2) WHERE id = $1")
        .bind(id)
        .bind(minutes as i32)
        .execute(pool)
        .await
        .unwrap();
}

#[sqlx::test]
async fn the_claim_applies_the_hold_decisions(pool: PgPool) {
    let (a, b) = (manager(&pool).await, manager(&pool).await);
    let hold = |cutoff_secs: Option<i64>| ModelHold::Hold {
        cutoff: cutoff_secs.map(|secs| Utc::now() + Duration::seconds(secs)),
    };

    // Flex requests are due an hour after creation. A cutoff at 2h releases
    // them; both replicas, given the same decisions, decide alike.
    a.set_dispatch_hold(Some(decisions(&[("m", hold(Some(7_200)))])));
    b.set_dispatch_hold(Some(decisions(&[("m", hold(Some(7_200)))])));
    let ids = flex(&a, "m", 6).await;
    let mut decided = claim(&a, "m", 3).await;
    decided.extend(claim(&b, "m", 3).await);
    assert_eq!(decided.len(), 6);
    for id in &ids {
        assert_eq!(
            decided[id],
            Some(DispatchTolerations::Release(
                TolerationsRelease::SlaProjection
            ))
        );
    }

    // Due after the cutoff, or no cutoff at all: held.
    a.set_dispatch_hold(Some(decisions(&[("m", hold(Some(1_800)))])));
    let later = flex(&a, "m", 1).await;
    assert_eq!(
        claim(&a, "m", 1).await[&later[0]],
        Some(DispatchTolerations::Keep)
    );
    a.set_dispatch_hold(Some(decisions(&[("m", hold(None))])));
    let uncut = flex(&a, "m", 1).await;
    assert_eq!(
        claim(&a, "m", 1).await[&uncut[0]],
        Some(DispatchTolerations::Keep)
    );

    // A model released whole.
    a.set_dispatch_hold(Some(decisions(&[(
        "m",
        ModelHold::ReleaseAll(TolerationsRelease::NoOwnCapacity),
    )])));
    let all = flex(&a, "m", 1).await;
    assert_eq!(
        claim(&a, "m", 1).await[&all[0]],
        Some(DispatchTolerations::Release(
            TolerationsRelease::NoOwnCapacity
        ))
    );

    // Stale decisions release everything.
    let mut stale = decisions(&[("m", hold(None))]);
    stale.computed_at = Utc::now() - Duration::minutes(6);
    a.set_dispatch_hold(Some(stale));
    let old = flex(&a, "m", 1).await;
    assert_eq!(
        claim(&a, "m", 1).await[&old[0]],
        Some(DispatchTolerations::Release(TolerationsRelease::Stale))
    );

    // The floor applies whatever the decision: inside the 1h ramp (~9.9
    // minutes before the deadline)...
    a.set_dispatch_hold(Some(decisions(&[("m", hold(None))])));
    let ramp = flex(&a, "m", 1).await;
    set_created_ago(&pool, ramp[0], 55).await;
    assert_eq!(
        claim(&a, "m", 1).await[&ramp[0]],
        Some(DispatchTolerations::Release(TolerationsRelease::Ramp))
    );
    // ...and past the deadline.
    let late = flex(&a, "m", 1).await;
    set_created_ago(&pool, late[0], 120).await;
    assert_eq!(
        claim(&a, "m", 1).await[&late[0]],
        Some(DispatchTolerations::Release(
            TolerationsRelease::PastDeadline
        ))
    );

    // A model that is not held gets no decision, nor does unconfigured
    // storage.
    let other = flex(&a, "other", 1).await;
    assert_eq!(claim(&a, "other", 1).await[&other[0]], None);
    let fresh = manager(&pool).await;
    let unconfigured = flex(&fresh, "m", 1).await;
    assert_eq!(claim(&fresh, "m", 1).await[&unconfigured[0]], None);

    // The decision lives only in the claim result: requests carries no
    // column for it.
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name::TEXT FROM information_schema.columns
         WHERE table_name = 'requests' AND column_name LIKE '%tolerat%'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(columns.is_empty(), "{columns:?}");
}

#[sqlx::test]
async fn the_batch_claim_applies_the_hold_decisions(pool: PgPool) {
    let manager = manager(&pool).await;
    manager
        .append_model_filter_events(&[ModelFilter {
            model: "m".to_string(),
            state: ModelFilterState::Live,
            expected_ready_at: None,
        }])
        .await
        .unwrap();
    let file_id = manager
        .create_file(
            "hold".to_string(),
            None,
            (0..2)
                .map(|i| RequestTemplateInput {
                    custom_id: None,
                    endpoint: "https://api.example.com".to_string(),
                    method: "POST".to_string(),
                    path: "/v1/chat/completions".to_string(),
                    body: format!(r#"{{"model":"m","n":{i}}}"#),
                    model: "m".to_string(),
                    api_key: "key".to_string(),
                })
                .collect(),
        )
        .await
        .unwrap();
    manager
        .create_batch(BatchInput {
            file_id,
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
            created_by: None,
            api_key_id: None,
            api_key: None,
            total_requests: None,
        })
        .await
        .unwrap();

    // The 24h batch is due before a 25h cutoff: released.
    manager.set_dispatch_hold(Some(decisions(&[(
        "m",
        ModelHold::Hold {
            cutoff: Some(Utc::now() + Duration::hours(25)),
        },
    )])));
    let claimed = manager
        .claim_batch_requests(
            1,
            10,
            DaemonId(Uuid::new_v4()),
            &HashMap::from([("m".to_string(), 1)]),
            &HashMap::new(),
        )
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(
        claimed[0].state.tolerations,
        Some(DispatchTolerations::Release(
            TolerationsRelease::SlaProjection
        ))
    );

    // With no cutoff it is held.
    manager.set_dispatch_hold(Some(decisions(&[("m", ModelHold::Hold { cutoff: None })])));
    let claimed = manager
        .claim_batch_requests(
            1,
            10,
            DaemonId(Uuid::new_v4()),
            &HashMap::from([("m".to_string(), 1)]),
            &HashMap::new(),
        )
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(
        claimed[0].state.tolerations,
        Some(DispatchTolerations::Keep)
    );
}

fn minute(now: DateTime<Utc>) -> DateTime<Utc> {
    now.duration_trunc(Duration::minutes(1)).unwrap()
}

async fn stats(pool: &PgPool) -> Vec<(String, DateTime<Utc>, i64, i64)> {
    sqlx::query_as(
        "SELECT model, minute, completed, refused FROM hold_stats ORDER BY model, minute",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

fn delta(model: &str, completed: i64, refused: i64) -> HoldStatsDelta {
    HoldStatsDelta {
        model: model.to_string(),
        completed,
        refused,
    }
}

#[sqlx::test]
async fn counter_flushes_add_up_across_daemons_and_old_rows_are_purged(pool: PgPool) {
    let (a, b) = (manager(&pool).await, manager(&pool).await);
    let now = minute(Utc::now());

    // Two daemons flush into the same (model, minute): the counts add.
    a.flush_hold_stats(now, &[delta("m", 3, 1), delta("n", 0, 2)])
        .await
        .unwrap();
    b.flush_hold_stats(now, &[delta("m", 4, 0)]).await.unwrap();
    a.flush_hold_stats(now, &[]).await.unwrap();
    assert_eq!(
        stats(&pool).await,
        vec![("m".to_string(), now, 7, 1), ("n".to_string(), now, 0, 2),]
    );

    // Purge removes rows before the cutoff and keeps the rest.
    let old = now - Duration::hours(3);
    b.flush_hold_stats(old, &[delta("m", 9, 9)]).await.unwrap();
    assert_eq!(
        a.purge_hold_stats(now - Duration::hours(2)).await.unwrap(),
        1
    );
    assert_eq!(stats(&pool).await.len(), 2);
}

#[sqlx::test]
async fn every_daemon_reads_the_same_inputs_and_decides_alike(pool: PgPool) {
    let (a, b) = (manager(&pool).await, manager(&pool).await);
    let now = minute(Utc::now());
    let models = ["m".to_string(), "idle".to_string()];

    // In the window: 900 completions and some refusals; outside it, more
    // that must not count.
    a.flush_hold_stats(now, &[delta("m", 500, 3)])
        .await
        .unwrap();
    b.flush_hold_stats(now - Duration::minutes(5), &[delta("m", 400, 0)])
        .await
        .unwrap();
    a.flush_hold_stats(now - Duration::minutes(30), &[delta("m", 10_000, 10_000)])
        .await
        .unwrap();

    // Outstanding work: 3 flex requests due in about an hour, one created
    // earlier (due in about 30 minutes), one in flight; a finished one and
    // another model's do not count.
    let pending = flex(&a, "m", 4).await;
    // Pin the creation times so the deadlines fall in two known minutes.
    sqlx::query("UPDATE requests SET created_at = date_trunc('minute', NOW()) WHERE id = ANY($1)")
        .bind(&pending)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE requests SET created_at = created_at - INTERVAL '30 minutes' WHERE id = $1",
    )
    .bind(pending[0])
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE requests SET state = 'processing', daemon_id = $2, claimed_at = NOW(), started_at = NOW() WHERE id = $1")
        .bind(pending[1])
        .bind(Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
    let done = flex(&a, "m", 1).await;
    sqlx::query("UPDATE requests SET state = 'completed', completed_at = NOW(), response_status = 200, response_body = '{}' WHERE id = $1")
        .bind(done[0])
        .execute(&pool)
        .await
        .unwrap();
    flex(&a, "unheld", 2).await;

    let (read_at_a, inputs_a) = a
        .read_hold_inputs(&models, PARAMS.window_secs)
        .await
        .unwrap();
    let (read_at_b, inputs_b) = b
        .read_hold_inputs(&models, PARAMS.window_secs)
        .await
        .unwrap();
    assert_eq!(inputs_a, inputs_b, "every daemon reads the same inputs");

    let m = inputs_a.iter().find(|inputs| inputs.model == "m").unwrap();
    assert_eq!((m.completed, m.refused), (900, 3));
    assert_eq!(
        m.backlog_requests(),
        4,
        "pending and in flight, this model only"
    );
    assert_eq!(
        m.backlog.len(),
        2,
        "one point per deadline minute: {:?}",
        m.backlog
    );
    assert_eq!(m.backlog[0].1, 1, "the earlier request alone");
    assert_eq!(m.backlog[1].1, 4, "cumulative");
    assert!(m.backlog[0].0 < m.backlog[1].0);
    let idle = inputs_a
        .iter()
        .find(|inputs| inputs.model == "idle")
        .unwrap();
    assert_eq!(
        (idle.completed, idle.refused, idle.backlog.len()),
        (0, 0, 0)
    );

    // Same inputs, same decisions: 1 completion/s clears the backlog in
    // seconds, so there is no cutoff.
    let decide = |inputs: &[fusillade_core::release::ModelHoldInputs], at| {
        inputs
            .iter()
            .map(|inputs| {
                let decision = decide_model_hold(inputs, PARAMS, at);
                (decision.model, decision.hold, decision.own_rate)
            })
            .collect::<Vec<_>>()
    };
    let decisions_a = decide(&inputs_a, read_at_a);
    let decisions_b = decide(&inputs_b, read_at_a);
    assert_eq!(decisions_a, decisions_b);
    assert!(read_at_b >= read_at_a);
    let (_, hold, own_rate) = decisions_a.iter().find(|d| d.0 == "m").unwrap();
    assert_eq!(*hold, ModelHold::Hold { cutoff: None });
    assert!((own_rate - 1.0).abs() < 1e-9, "{own_rate}");
}

#[test]
fn the_hold_stats_migration_is_the_newest_and_follows_main() {
    let newest = fusillade_arsenal::MIGRATOR
        .iter()
        .filter(|migration| migration.migration_type.is_up_migration())
        .max_by_key(|migration| migration.version)
        .unwrap();
    assert_eq!(newest.version, 20261014130000);
    assert_eq!(newest.description, "add hold stats");
    assert!(
        newest.version > 20261013100030,
        "sorts after the finished-late index migrations"
    );
}
