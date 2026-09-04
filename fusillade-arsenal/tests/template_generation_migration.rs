use sqlx::PgPool;
use uuid::Uuid;

async fn create_file(pool: &PgPool) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO files (name, size_bytes, size_finalized, status, purpose) \
         VALUES ('g2-test-' || gen_random_uuid(), 0, TRUE, 'processed', 'batch') \
         RETURNING id",
    )
    .fetch_one(pool)
    .await
    .expect("file fixture must insert")
}

async fn monday(pool: &PgPool, weeks_from_now: i32) -> chrono::NaiveDate {
    sqlx::query_scalar(
        "SELECT (date_trunc('week', statement_timestamp() AT TIME ZONE 'UTC'))::date \
             + ($1::int * 7)",
    )
    .bind(weeks_from_now)
    .fetch_one(pool)
    .await
    .expect("week boundary must resolve")
}

async fn insert_g2_template(pool: &PgPool, week_start: chrono::NaiveDate, file_id: Uuid) -> Uuid {
    let template_id: Uuid = sqlx::query_scalar(
        "INSERT INTO request_templates_g2 (
             created_on, file_id, endpoint, method, path, body, model, api_key
         ) VALUES ($1, $2, 'https://example.invalid', 'POST', '/v1/x',
                   '{\"gen\":2}', 'test-model', 'test-key')
         RETURNING id",
    )
    .bind(week_start)
    .bind(file_id)
    .fetch_one(pool)
    .await
    .expect("generation-2 template must insert");
    sqlx::query("INSERT INTO request_template_routes (template_id, week_start) VALUES ($1, $2)")
        .bind(template_id)
        .bind(week_start)
        .execute(pool)
        .await
        .expect("route must insert");
    template_id
}

#[sqlx::test]
async fn generation_two_parent_is_weekly_range_partitioned(pool: PgPool) {
    let range_partitioned: bool = sqlx::query_scalar(
        "SELECT partstrat = 'r' FROM pg_partitioned_table p \
         JOIN pg_class c ON c.oid = p.partrelid \
         WHERE c.relname = 'request_templates_g2'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(range_partitioned);

    // The generation-1 heap is retired: nothing but the weekly store remains.
    let legacy_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_class WHERE relname = 'request_templates' \
                        AND relkind IN ('r', 'p'))",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!legacy_exists, "the legacy template heap must be gone");
}

#[sqlx::test]
async fn ensure_helpers_create_exact_weekly_partitions(pool: PgPool) {
    let week = monday(&pool, 1).await;
    sqlx::query("SELECT ensure_request_template_partition($1, NULL)")
        .bind(week)
        .execute(&pool)
        .await
        .unwrap();
    // Idempotent replay.
    sqlx::query("SELECT ensure_request_template_partition($1, NULL)")
        .bind(week)
        .execute(&pool)
        .await
        .unwrap();

    let (bucket_state, bounds_ok): (String, bool) = sqlx::query_as(
        "SELECT bucket.state, \
                pg_get_expr(child.relpartbound, child.oid) = format( \
                    'FOR VALUES FROM (%L) TO (%L)', $1::date, $1::date + 7) \
         FROM request_template_buckets bucket \
         JOIN pg_class child ON child.oid = bucket.partition_oid \
         WHERE bucket.week_start = $1",
    )
    .bind(week)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(bucket_state, "active");
    assert!(bounds_ok, "the weekly child must carry exact Monday bounds");

    let mid_week_error = sqlx::query("SELECT ensure_request_template_partition($1 + 1, NULL)")
        .bind(week)
        .execute(&pool)
        .await
        .expect_err("a non-Monday week start must be rejected");
    assert_eq!(
        mid_week_error
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("22023")
    );

    let created: i32 = sqlx::query_scalar("SELECT ensure_request_template_partitions($1, 2)")
        .bind(week)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(created, 2, "the existing first week must not be recounted");
    let recreated: i32 = sqlx::query_scalar("SELECT ensure_request_template_partitions($1, 2)")
        .bind(week)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(recreated, 0);
}

#[sqlx::test]
async fn active_view_hides_templates_of_soft_deleted_files_but_keeps_dedicated_ones(pool: PgPool) {
    let week = monday(&pool, 0).await;
    sqlx::query("SELECT ensure_request_template_partition($1, NULL)")
        .bind(week)
        .execute(&pool)
        .await
        .unwrap();

    let file = create_file(&pool).await;
    let file_backed = insert_g2_template(&pool, week, file).await;
    // A dedicated batchless template has no file and must stay visible to
    // the claim join.
    let dedicated: Uuid = sqlx::query_scalar(
        "INSERT INTO request_templates_g2 (created_on, endpoint, method, path, body, model, api_key) \
         VALUES ($1, 'e', 'POST', '/x', '{\"dedicated\":true}', 'm', 'k') RETURNING id",
    )
    .bind(week)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO request_template_routes (template_id, week_start) VALUES ($1, $2)")
        .bind(dedicated)
        .bind(week)
        .execute(&pool)
        .await
        .unwrap();

    let visible: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM active_request_templates WHERE id = ANY($1)")
            .bind(vec![file_backed, dedicated])
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(visible, 2);

    sqlx::query("UPDATE files SET deleted_at = NOW() WHERE id = $1")
        .bind(file)
        .execute(&pool)
        .await
        .unwrap();
    let remaining: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM active_request_templates WHERE id = ANY($1)")
            .bind(vec![file_backed, dedicated])
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        remaining,
        vec![dedicated],
        "a soft-deleted file hides its templates; dedicated templates stay"
    );
}

#[sqlx::test]
async fn a_fenced_bucket_hides_generation_two_templates(pool: PgPool) {
    let week = monday(&pool, 0).await;
    sqlx::query("SELECT ensure_request_template_partition($1, NULL)")
        .bind(week)
        .execute(&pool)
        .await
        .unwrap();
    let file = create_file(&pool).await;
    let template_id = insert_g2_template(&pool, week, file).await;

    let visible: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM active_request_templates WHERE id = $1")
            .bind(template_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(visible, 1);

    sqlx::query(
        "UPDATE request_template_buckets \
         SET state = 'retiring', state_changed_at = statement_timestamp() \
         WHERE week_start = $1",
    )
    .bind(week)
    .execute(&pool)
    .await
    .unwrap();

    let fenced: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM active_request_templates WHERE id = $1")
            .bind(template_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        fenced, 0,
        "a retiring bucket must fence its templates out of every active read"
    );
}

#[sqlx::test]
async fn routes_resolve_to_one_partition(pool: PgPool) {
    let week = monday(&pool, 0).await;
    sqlx::query("SELECT ensure_request_template_partitions($1, 3)")
        .bind(week)
        .execute(&pool)
        .await
        .unwrap();
    let file = create_file(&pool).await;
    let template_id = insert_g2_template(&pool, week, file).await;

    // Route-directed point read prunes to exactly one weekly child.
    let plan: Vec<String> = sqlx::query_scalar(
        "EXPLAIN (COSTS OFF) \
         SELECT g2.body FROM request_template_routes route \
         JOIN request_templates_g2 g2 \
           ON g2.created_on >= route.week_start \
          AND g2.created_on < route.week_start + 7 \
          AND g2.id = route.template_id \
         WHERE route.template_id = $1",
    )
    .bind(template_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    let scanned_children = plan
        .iter()
        .filter(|line| line.contains("request_templates_g2_y"))
        .count();
    assert!(
        scanned_children >= 1,
        "the point read must reach a weekly child: {plan:?}"
    );
}

#[sqlx::test]
async fn file_writes_land_with_routes_and_erase_with_them(pool: PgPool) {
    use fusillade_arsenal::batch::RequestTemplateInput;
    use fusillade_arsenal::manager::DaemonStorage;
    use fusillade_arsenal::{PostgresRequestManager, PostgresStorageConfig, Storage, TestDbPools};

    let template = |n: u8| RequestTemplateInput {
        custom_id: Some(format!("cut-{n}")),
        endpoint: "https://example.invalid".to_string(),
        method: "POST".to_string(),
        path: "/v1/x".to_string(),
        body: format!("{{\"cut\":{n}}}"),
        model: "test-model".to_string(),
        api_key: "test-key".to_string(),
    };

    let manager = PostgresRequestManager::new(
        TestDbPools::new(pool.clone()).await.unwrap(),
        PostgresStorageConfig::default(),
    );
    let file = manager
        .create_file("g2-file".to_string(), None, vec![template(2), template(3)])
        .await
        .unwrap();

    let (rows, routes): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM request_templates_g2 WHERE file_id = $1), \
                (SELECT COUNT(*) FROM request_template_routes route \
                 JOIN request_templates_g2 g2 ON g2.id = route.template_id \
                 WHERE g2.file_id = $1)",
    )
    .bind(*file)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (rows, routes),
        (2, 2),
        "every file template must land with its route"
    );

    let stats = manager.get_file_template_stats(file).await.unwrap();
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].request_count, 2);

    // Explicit erasure removes rows and routes together.
    manager.delete_file(file).await.unwrap();
    while manager.purge_orphaned_rows(100).await.unwrap() > 0 {}
    let (left_rows, left_routes): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM request_templates_g2 WHERE file_id = $1), \
                (SELECT COUNT(*) FROM request_template_routes)",
    )
    .bind(*file)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (left_rows, left_routes),
        (0, 0),
        "explicit erasure must remove template rows and their routes"
    );
}

async fn retirement_manager(
    pool: &PgPool,
) -> fusillade_arsenal::PostgresRequestManager<fusillade_arsenal::TestDbPools> {
    use fusillade_arsenal::{PostgresRequestManager, PostgresStorageConfig, TestDbPools};
    let maintenance_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .min_connections(0)
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    PostgresRequestManager::new(
        TestDbPools::new(pool.clone()).await.unwrap(),
        PostgresStorageConfig::default(),
    )
    .with_partition_maintenance_pool(maintenance_pool)
    .unwrap()
    .attest_partition_maintenance_pool()
    .await
    .unwrap()
}

async fn aged_file(pool: &PgPool, days_old: i64) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO files (name, size_bytes, size_finalized, status, purpose, created_at) \
         VALUES ('aged-' || gen_random_uuid(), 0, TRUE, 'processed', 'batch', \
                 NOW() - make_interval(days => $1::int)) RETURNING id",
    )
    .bind(days_old as i32)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test]
async fn file_content_expiry_tombstones_only_released_files(pool: PgPool) {
    use fusillade_arsenal::manager::DaemonStorage;

    let released = aged_file(&pool, 400).await;
    let pinned = aged_file(&pool, 400).await;
    sqlx::query(
        "INSERT INTO batches (file_id, endpoint, completion_window, expires_at, \
                              location, archive_bucket, counts_frozen_at) \
         VALUES ($1, '/v1/x', '24h', NOW(), 'archive', \
                 date_trunc('week', NOW() AT TIME ZONE 'UTC')::date, \
                 NOW() - INTERVAL '10 days')",
    )
    .bind(pinned)
    .execute(&pool)
    .await
    .unwrap();
    let young = aged_file(&pool, 5).await;

    let manager = retirement_manager(&pool).await;
    let expired = manager.expire_file_content(365, 100).await.unwrap();
    assert_eq!(expired, 1, "only the released aged file may expire");

    let rows: Vec<(Uuid, Option<String>, bool, bool)> = sqlx::query_as(
        "SELECT id, name, deleted_at IS NOT NULL, retention_expired_at IS NOT NULL \
         FROM files WHERE id = ANY($1) ORDER BY created_at",
    )
    .bind(vec![released, pinned, young])
    .fetch_all(&pool)
    .await
    .unwrap();
    for (id, name, deleted, stamped) in rows {
        assert!(name.is_some(), "file metadata (name) must be retained");
        if id == released {
            assert!(deleted && stamped, "released aged file must be tombstoned");
        } else {
            assert!(!deleted && !stamped, "pinned/young files must survive");
        }
    }

    // Once the pinned file's batch content expires, the next pass releases it.
    sqlx::query("UPDATE batches SET retention_expired_at = NOW() WHERE file_id = $1")
        .bind(pinned)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(manager.expire_file_content(365, 100).await.unwrap(), 1);
    assert_eq!(manager.expire_file_content(365, 100).await.unwrap(), 0);
}

#[sqlx::test]
async fn template_week_retires_only_after_every_file_releases(pool: PgPool) {
    use fusillade_arsenal::manager::{DaemonStorage, RetainedResponseRetirementOutcome};

    let week = monday(&pool, -60).await;
    sqlx::query("SELECT ensure_request_template_partition($1, NULL)")
        .bind(week)
        .execute(&pool)
        .await
        .unwrap();
    let file = aged_file(&pool, 500).await;
    let template_id = insert_g2_template(&pool, week, file).await;

    let manager = retirement_manager(&pool).await;
    let blocked = manager
        .retire_expired_template_partition(true, 365)
        .await
        .unwrap();
    assert_eq!(
        blocked,
        RetainedResponseRetirementOutcome::NoCandidate,
        "a live file must pin its template week"
    );

    // File-content expiry releases the window (no batches reference it).
    assert_eq!(manager.expire_file_content(365, 100).await.unwrap(), 1);
    let retired = manager
        .retire_expired_template_partition(true, 365)
        .await
        .unwrap();
    assert_eq!(retired, RetainedResponseRetirementOutcome::Retired);
    let child_exists: Option<i64> = sqlx::query_scalar("SELECT to_regclass($1)::oid::bigint")
        .bind(format!(
            "request_templates_g2_y{}w{:02}",
            {
                use chrono::Datelike;
                week.iso_week().year()
            },
            {
                use chrono::Datelike;
                week.iso_week().week()
            }
        ))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        child_exists, None,
        "the weekly template partition must drop"
    );

    // Routes survive the drop and are removed by the bounded cleanup phase.
    let routed: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM request_template_routes WHERE template_id = $1")
            .bind(template_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(routed, 1);
    assert_eq!(
        manager.cleanup_retired_template_routes(10).await.unwrap(),
        1
    );
    assert_eq!(
        manager.cleanup_retired_template_routes(10).await.unwrap(),
        0
    );
}

#[sqlx::test]
async fn an_unowned_template_row_fails_the_gate_closed(pool: PgPool) {
    use fusillade_arsenal::manager::{DaemonStorage, RetainedResponseRetirementOutcome};

    let week = monday(&pool, -60).await;
    sqlx::query("SELECT ensure_request_template_partition($1, NULL)")
        .bind(week)
        .execute(&pool)
        .await
        .unwrap();
    // A row with no owning file (file_id NULL) must never be dropped
    // implicitly by scheduled retention.
    sqlx::query(
        "INSERT INTO request_templates_g2 (created_on, endpoint, method, path, model, api_key) \
         VALUES ($1, 'e', 'POST', '/x', 'm', 'k')",
    )
    .bind(week)
    .execute(&pool)
    .await
    .unwrap();

    let manager = retirement_manager(&pool).await;
    let blocked = manager
        .retire_expired_template_partition(true, 365)
        .await
        .unwrap();
    assert_eq!(blocked, RetainedResponseRetirementOutcome::NoCandidate);
}

#[sqlx::test]
async fn a_batch_materializes_requests_from_generation_two_templates(pool: PgPool) {
    use fusillade_arsenal::batch::{BatchInput, RequestTemplateInput};
    use fusillade_arsenal::{PostgresRequestManager, PostgresStorageConfig, Storage, TestDbPools};

    let manager = PostgresRequestManager::new(
        TestDbPools::new(pool.clone()).await.unwrap(),
        PostgresStorageConfig::default(),
    );
    let file_id = manager
        .create_file(
            "g2-batch-source".to_string(),
            None,
            vec![RequestTemplateInput {
                custom_id: Some("g2-req".to_string()),
                endpoint: "https://example.invalid".to_string(),
                method: "POST".to_string(),
                path: "/v1/x".to_string(),
                body: "{\"gen\":2}".to_string(),
                model: "test-model".to_string(),
                api_key: "test-key".to_string(),
            }],
        )
        .await
        .unwrap();

    // Regression: requests.template_id once carried a row-level foreign key
    // to a template heap, so materializing from partitioned ids failed.
    let batch = manager
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
    let (count, joined): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COUNT(*) FILTER (WHERE t.id IS NOT NULL) \
         FROM requests r LEFT JOIN request_templates_all t ON t.id = r.template_id \
         WHERE r.batch_id = $1",
    )
    .bind(*batch.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (count, joined),
        (1, 1),
        "requests must materialize from and resolve back to generation-2 templates"
    );
}

/// Batch result downloads join each request back to its input template
/// through `request_templates_all`; a direct heap join would stream nothing.
#[sqlx::test]
async fn batch_results_stream_finds_generation_two_templates(pool: PgPool) {
    use fusillade_arsenal::{PostgresRequestManager, PostgresStorageConfig, Storage, TestDbPools};
    use futures::StreamExt;

    let file_id = create_file(&pool).await;
    let week = monday(&pool, 0).await;
    sqlx::query("SELECT ensure_request_template_partition($1, NULL)")
        .bind(week)
        .execute(&pool)
        .await
        .unwrap();
    let template_id = insert_g2_template(&pool, week, file_id).await;
    let batch_id: Uuid = sqlx::query_scalar(
        "INSERT INTO batches (file_id, endpoint, completion_window, created_by, total_requests, expires_at) \
         VALUES ($1, '/v1/x', '24h', 'tester', 1, NOW() + INTERVAL '1 day') RETURNING id",
    )
    .bind(file_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO requests (batch_id, template_id, model, custom_id, state, response_status, \
                               response_body, completed_at) \
         VALUES ($1, $2, 'test-model', 'line-0', 'completed', 200, '{\"ok\":true}', NOW())",
    )
    .bind(batch_id)
    .bind(template_id)
    .execute(&pool)
    .await
    .unwrap();

    let manager = PostgresRequestManager::new(
        TestDbPools::new(pool.clone()).await.unwrap(),
        PostgresStorageConfig::default(),
    );
    let items: Vec<_> = manager
        .get_batch_results_stream(batch_id.into(), 0, None, None)
        .collect()
        .await;
    assert_eq!(items.len(), 1, "the batch's single request must stream");
    let item = items
        .into_iter()
        .next()
        .unwrap()
        .expect("stream item must not be an error");
    assert_eq!(item.custom_id.as_deref(), Some("line-0"));
    assert_eq!(item.input_body, serde_json::json!({"gen": 2}));
}
