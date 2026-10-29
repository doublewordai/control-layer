use super::*;
use crate::api::models::users::Role;
use crate::test::utils::{create_test_endpoint, create_test_model, create_test_user, insert_fusillade_template};
use sqlx::{PgPool, Row};
use uuid::Uuid;

fn manager(pool: &PgPool) -> fusillade_arsenal::PostgresRequestManager<DbPools> {
    fusillade_arsenal::PostgresRequestManager::new(DbPools::new(pool.clone()), Default::default()).with_query_schema("fusillade")
}

async fn catalog(pool: &PgPool) {
    let user = create_test_user(pool, Role::StandardUser).await;
    let endpoint = create_test_endpoint(pool, "sla-fixture", user.id).await;
    for model in ["public-model", "excluded-model", "private-model", "private-model-2"] {
        let deployment = create_test_model(pool, model, model, endpoint, user.id).await;
        if !model.starts_with("private-model") {
            sqlx::query("INSERT INTO deployment_groups (deployment_id, group_id, granted_by) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
                .bind(deployment)
                .bind(Uuid::nil())
                .bind(user.id)
                .execute(pool)
                .await
                .unwrap();
        }
    }
}

// Offset seconds relative to a fixed clock. Synthetic bodies/credentials ensure
// the reader and rendered metrics never need payload fields.
#[allow(clippy::too_many_arguments)]
async fn batch(
    pool: &PgPool,
    now: DateTime<Utc>,
    window: &str,
    created: i64,
    expiry: Option<i64>,
    terminal: [Option<i64>; 3],
    cancelling: bool,
    deleted: bool,
    models: &[&str],
) {
    let file: Uuid = sqlx::query_scalar("INSERT INTO fusillade.files (name) VALUES ('fixture') RETURNING id")
        .fetch_one(pool)
        .await
        .unwrap();
    for model in models {
        insert_fusillade_template(
            pool,
            Uuid::new_v4(),
            Some(file),
            model,
            "SECRET-SENTINEL",
            "https://example.invalid",
            "/v1/chat/completions",
            "PAYLOAD-SENTINEL",
            Some("CUSTOM-ID-SENTINEL"),
        )
        .await;
    }
    let time = |seconds| now + chrono::Duration::seconds(seconds);
    sqlx::query("INSERT INTO fusillade.batches (file_id, completion_window, created_at, expires_at, completed_at, failed_at,
        cancelled_at, cancelling_at, deleted_at, created_by, api_key, service_tier) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,'OWNER-SENTINEL','SECRET-SENTINEL',$10)")
        .bind(file).bind(expiry.map(|_| window)).bind(time(created)).bind(expiry.map(time))
        .bind(terminal[0].map(time)).bind(terminal[1].map(time)).bind(terminal[2].map(time))
        .bind(cancelling.then_some(now)).bind(deleted.then_some(now)).bind(expiry.is_none().then_some("background")).execute(pool).await.unwrap();
}

#[dwctl_test_macros::test]
async fn aggregate_queries_match_legacy_alerts_and_preserve_privacy(pool: PgPool) {
    catalog(&pool).await;
    let now = Utc::now();
    let public = vec!["public-model".to_string(), "excluded-model".to_string()];
    let excluded = vec!["excluded-model".to_string()];
    let manager = manager(&pool);
    // Normal, stuck, boundary, missing metadata, mixed-model, cancelling,
    // deleted and multiply-terminal batches share one isolated migrated DB.
    for window in ["1h", "24h"] {
        batch(&pool, now, window, -7200, Some(-600), [None; 3], false, false, &["public-model"]).await;
        batch(
            &pool,
            now,
            window,
            -600000,
            Some(-500000),
            [None; 3],
            false,
            false,
            &["private-model"],
        )
        .await;
        batch(
            &pool,
            now,
            window,
            -7200,
            Some(-600),
            [Some(-300), Some(-200), Some(-100)],
            false,
            false,
            &["public-model"],
        )
        .await;
        batch(&pool, now, window, -8000, Some(-700), [None; 3], false, false, &["private-model-2"]).await;
        batch(&pool, now, window, -7200, Some(-600), [None; 3], false, false, &["excluded-model"]).await;
        batch(
            &pool,
            now,
            window,
            -7200,
            Some(-600),
            [None; 3],
            false,
            false,
            &["excluded-model", "public-model"],
        )
        .await;
        batch(&pool, now, window, -7200, Some(-600), [None; 3], false, false, &[]).await;
        batch(&pool, now, window, -7200, Some(-600), [None; 3], true, false, &["public-model"]).await;
        batch(&pool, now, window, -7200, Some(-600), [None; 3], false, true, &["public-model"]).await;
        batch(&pool, now, window, -7200, None, [None; 3], false, false, &["public-model"]).await;
        batch(&pool, now, window, -7200, Some(0), [None; 3], false, false, &["public-model"]).await;
        batch(
            &pool,
            now,
            window,
            -12000,
            Some(-4000),
            [Some(-3600), None, None],
            false,
            false,
            &["public-model"],
        )
        .await;
        batch(
            &pool,
            now,
            window,
            -400000,
            Some(-600),
            [None, Some(-100), None],
            false,
            false,
            &["public-model"],
        )
        .await;
        for terminal in [
            [Some(-100), None, None],
            [None, Some(-100), None],
            [None, None, Some(-100)],
            [Some(-700), None, None],
        ] {
            batch(&pool, now, window, -7200, Some(-600), terminal, false, false, &["public-model"]).await;
        }
    }
    // Distinct batch counting despite repeated templates; keep cancelling near misses.
    for (created, expiry) in [(-3300, 300), (-3000, 600), (-84600, 1800), (-84601, 1799), (-172801, 300)] {
        batch(
            &pool,
            now,
            "24h",
            created,
            Some(expiry),
            [None; 3],
            true,
            false,
            &["public-model", "public-model", "private-model"],
        )
        .await;
    }
    let mut tx = manager.begin_write().await.unwrap();
    let rows = batch_sla::snapshot(&mut tx, now, &public, &excluded).await.unwrap();
    for (policy, count_sql, detail_sql, detail_name) in [
        (
            "flex",
            include_str!("fixtures/flex_count.sql"),
            include_str!("fixtures/flex_detail.sql"),
            "max_minutes_late",
        ),
        (
            "near",
            include_str!("fixtures/near_count.sql"),
            include_str!("fixtures/near_detail.sql"),
            "min_minutes_remaining",
        ),
        (
            "standard",
            include_str!("fixtures/standard_count.sql"),
            include_str!("fixtures/standard_detail.sql"),
            "max_minutes_late",
        ),
    ] {
        let counts = sqlx::query(count_sql).bind(now).fetch_all(&mut *tx).await.unwrap();
        let details = sqlx::query(detail_sql).bind(now).fetch_all(&mut *tx).await.unwrap();
        let count_name = if policy == "near" {
            "batches_at_risk"
        } else {
            "batches_missed_sla"
        };
        let mut expected_rows = std::collections::BTreeMap::<String, (i64, f64)>::new();
        for expected in counts {
            let original_model = if policy == "standard" {
                "_all".into()
            } else {
                expected.get::<String, _>("model")
            };
            let model = if original_model.starts_with("private-model") {
                "_private"
            } else if original_model == "unknown" {
                "_unknown"
            } else {
                &original_model
            };
            let detail = details
                .iter()
                .find(|row| policy == "standard" || row.get::<String, _>("model") == original_model)
                .unwrap();
            let minutes = f64::from(detail.get::<i32, _>(detail_name));
            let value = expected_rows.entry(model.to_string()).or_insert((0, minutes));
            value.0 += expected.get::<i64, _>(count_name);
            value.1 = if policy == "near" {
                value.1.min(minutes)
            } else {
                value.1.max(minutes)
            };
        }
        assert_eq!(
            rows.iter().filter(|row| row.policy == policy).count(),
            expected_rows.len(),
            "unexpected {policy} series"
        );
        for (model, (count, minutes)) in expected_rows {
            let actual = rows.iter().find(|row| row.policy == policy && row.model == model).unwrap();
            assert_eq!(actual.affected_batches, count, "{policy}/{model}");
            let seconds = if policy == "near" {
                actual.min_time_remaining_seconds
            } else {
                actual.max_lateness_seconds
            };
            assert!((seconds / 60.0 - minutes).abs() <= 0.5, "{policy}/{model}: {seconds} vs {minutes}");
        }
    }
    tx.rollback().await.unwrap();
    let snapshot = collect(&DbPools::new(pool.clone()), &manager, &BatchSlaMetricsConfig::default())
        .await
        .unwrap();
    let rendered = snapshot.render();
    for forbidden in [
        "private-model",
        "PAYLOAD-SENTINEL",
        "SECRET-SENTINEL",
        "OWNER-SENTINEL",
        "CUSTOM-ID-SENTINEL",
    ] {
        assert!(!rendered.contains(forbidden), "leaked {forbidden}");
    }
    // A replacement empty snapshot drops the old model series rather than leaving stale alerts.
    let empty = Snapshot {
        rows: vec![],
        ..snapshot.clone()
    }
    .render();
    assert!(!empty.contains("public-model"));
    assert!(empty.contains("dwctl_batch_sla_collection_success 1"));
    assert!(empty.contains("model=\"_none\"} 0"));
    let failure = snapshot.failed();
    assert_eq!(failure.rows, snapshot.rows);
    assert_eq!(failure.last_success, snapshot.last_success);
    assert!(!failure.success);
    assert!(!failure.active);
}

#[dwctl_test_macros::test]
async fn collector_fails_closed_recovers_and_releases_leadership(pool: PgPool) {
    catalog(&pool).await;
    let main = DbPools::new(pool.clone());
    let manager = Arc::new(manager(&pool));
    let published = Arc::new(ArcSwap::from_pointee(Snapshot::default()));
    let shutdown = CancellationToken::new();
    let config = BatchSlaMetricsConfig {
        enabled: true,
        sample_interval: Duration::from_secs(5),
        query_timeout: Duration::from_secs(1),
        ..Default::default()
    };
    let task = tokio::spawn(run(
        main.clone(),
        manager.clone(),
        main.clone(),
        config,
        published.clone(),
        shutdown.clone(),
    ));
    async fn wait_until(published: &ArcSwap<Snapshot>, predicate: impl Fn(&Snapshot) -> bool) {
        tokio::time::timeout(Duration::from_secs(7), async {
            while !predicate(&published.load()) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
    wait_until(&published, |s| s.success).await;
    let last_success = published.load().last_success;
    let mut contender = None;
    assert!(!claim_leadership(&mut contender, &main).await.unwrap());
    // Force a real database timeout, then check that stale values never look healthy.
    let mut blocker = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE deployed_models IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    wait_until(&published, |s| !s.success && s.last_success == last_success).await;
    blocker.rollback().await.unwrap();
    wait_until(&published, |s| s.success && s.last_success > last_success).await;
    shutdown.cancel();
    task.await.unwrap().unwrap();
    assert!(!published.load().active);
    assert!(claim_leadership(&mut contender, &main).await.unwrap());
    drop(contender);
    // A restart reconstructs the state from metadata, without relying on old in-process counters.
    assert!(collect(&main, &manager, &BatchSlaMetricsConfig::default()).await.unwrap().success);
}

#[test]
fn disabled_and_invalid_configuration_and_label_escaping() {
    assert!(Snapshot::default().render().is_empty());
    assert!(BatchSlaMetricsConfig::default().validate().is_ok());
    assert!(
        BatchSlaMetricsConfig {
            sample_interval: Duration::ZERO,
            ..Default::default()
        }
        .validate()
        .is_err()
    );
    assert!(
        BatchSlaMetricsConfig {
            query_timeout: Duration::from_secs(30),
            ..Default::default()
        }
        .validate()
        .is_err()
    );
    assert_eq!(label("model\"\\\n"), "model\\\"\\\\\\n");
}

#[dwctl_test_macros::test]
async fn collector_uses_component_schema_and_reports_query_errors(pool: PgPool) {
    catalog(&pool).await;
    batch(
        &pool,
        Utc::now(),
        "24h",
        -7200,
        Some(-600),
        [None; 3],
        false,
        false,
        &["public-model"],
    )
    .await;
    // A second metadata schema has different contents. The pool itself still
    // defaults to public; the manager must set its transaction-local schema.
    sqlx::raw_sql(
        "CREATE SCHEMA batch_metrics_fixture;
        CREATE TABLE batch_metrics_fixture.batches (LIKE fusillade.batches INCLUDING DEFAULTS);
        CREATE VIEW batch_metrics_fixture.request_templates_all AS
            SELECT file_id, model FROM fusillade.request_templates_all",
    )
    .execute(&pool)
    .await
    .unwrap();
    let main = DbPools::new(pool.clone());
    let scoped =
        fusillade_arsenal::PostgresRequestManager::new(main.clone(), Default::default()).with_query_schema("batch_metrics_fixture");
    let config = BatchSlaMetricsConfig::default();
    let original = collect(&main, &manager(&pool), &config).await.unwrap();
    let isolated = collect(&main, &scoped, &config).await.unwrap();
    assert_eq!(original.rows.iter().find(|r| r.policy == "standard").unwrap().affected_batches, 1);
    assert!(isolated.rows.iter().all(|r| r.affected_batches == 0));

    // Election is independent per metadata schema, even within one database.
    let direct = sqlx::postgres::PgPoolOptions::new()
        .connect_with(
            pool.connect_options()
                .as_ref()
                .clone()
                .options([("search_path", "batch_metrics_fixture")]),
        )
        .await
        .unwrap();
    let mut first = None;
    let mut second = None;
    assert!(claim_leadership(&mut first, &main).await.unwrap());
    assert!(claim_leadership(&mut second, &DbPools::new(direct.clone())).await.unwrap());
    drop((first, second));
    direct.close().await;

    // Missing tables/permissions are errors, never a successful empty snapshot.
    sqlx::query("DROP VIEW batch_metrics_fixture.request_templates_all")
        .execute(&pool)
        .await
        .unwrap();
    assert!(collect(&main, &scoped, &config).await.is_err());
    sqlx::query("ALTER TABLE deployment_groups RENAME TO unavailable_deployment_groups")
        .execute(&pool)
        .await
        .unwrap();
    assert!(collect(&main, &manager(&pool), &config).await.is_err());
}
