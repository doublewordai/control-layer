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

    // The legacy heap remains an ordinary, untouched relation.
    let legacy_kind: String = sqlx::query_scalar(
        "SELECT relkind::text FROM pg_class WHERE relname = 'request_templates'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(legacy_kind, "r");

    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_name = 'request_templates' AND column_name <> 'created_on' \
         ORDER BY column_name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let g2_columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_name = 'request_templates_g2' AND column_name <> 'created_on' \
         ORDER BY column_name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        columns, g2_columns,
        "generation-2 must carry every legacy column plus created_on"
    );
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
async fn active_view_reads_both_generations_identically(pool: PgPool) {
    let week = monday(&pool, 0).await;
    sqlx::query("SELECT ensure_request_template_partition($1, NULL)")
        .bind(week)
        .execute(&pool)
        .await
        .unwrap();

    let legacy_file = create_file(&pool).await;
    let legacy_id: Uuid = sqlx::query_scalar(
        "INSERT INTO request_templates (file_id, endpoint, method, path, body, model, api_key) \
         VALUES ($1, 'https://example.invalid', 'POST', '/v1/x', '{\"gen\":1}', \
                 'test-model', 'test-key') RETURNING id",
    )
    .bind(legacy_file)
    .fetch_one(&pool)
    .await
    .unwrap();

    let g2_file = create_file(&pool).await;
    let g2_id = insert_g2_template(&pool, week, g2_file).await;

    let bodies: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, body FROM active_request_templates WHERE id = ANY($1) ORDER BY body",
    )
    .bind(vec![legacy_id, g2_id])
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        bodies,
        vec![
            (legacy_id, "{\"gen\":1}".to_string()),
            (g2_id, "{\"gen\":2}".to_string())
        ],
        "both generations must be visible through one identical view shape"
    );

    // A soft-deleted file hides its templates in either generation.
    sqlx::query("UPDATE files SET deleted_at = NOW() WHERE id = ANY($1)")
        .bind(vec![legacy_file, g2_file])
        .execute(&pool)
        .await
        .unwrap();
    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM active_request_templates WHERE id = ANY($1)")
            .bind(vec![legacy_id, g2_id])
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(remaining, 0);
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
async fn routes_resolve_generation_and_prune_to_one_partition(pool: PgPool) {
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

    // An id without a route is a legacy template by definition.
    let unrouted: Option<chrono::NaiveDate> = sqlx::query_scalar(
        "SELECT week_start FROM request_template_routes WHERE template_id = gen_random_uuid()",
    )
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert_eq!(unrouted, None);
}

#[sqlx::test]
async fn generation_two_writes_land_with_routes_and_read_identically(pool: PgPool) {
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

    let legacy_manager = PostgresRequestManager::new(
        TestDbPools::new(pool.clone()).await.unwrap(),
        PostgresStorageConfig::default(),
    );
    let legacy_file = legacy_manager
        .create_file("legacy-cutover".to_string(), None, vec![template(1)])
        .await
        .unwrap();

    let g2_manager = PostgresRequestManager::new(
        TestDbPools::new(pool.clone()).await.unwrap(),
        PostgresStorageConfig::default(),
    )
    .with_template_generation_writes(true);
    let g2_file = g2_manager
        .create_file(
            "g2-cutover".to_string(),
            None,
            vec![template(2), template(3)],
        )
        .await
        .unwrap();

    // Physical placement: the flag decides the generation of NEW writes.
    let legacy_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM request_templates WHERE file_id = $1")
            .bind(*legacy_file)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(legacy_rows, 1);
    let (g2_rows, g2_routes, g2_legacy_rows): (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM request_templates_g2 WHERE file_id = $1), \
                (SELECT COUNT(*) FROM request_template_routes route \
                 JOIN request_templates_g2 g2 ON g2.id = route.template_id \
                 WHERE g2.file_id = $1), \
                (SELECT COUNT(*) FROM request_templates WHERE file_id = $1)",
    )
    .bind(*g2_file)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (g2_rows, g2_routes, g2_legacy_rows),
        (2, 2, 0),
        "generation-2 writes must land with routes and never touch the legacy heap"
    );

    // Read parity: per-model statistics resolve either generation.
    let legacy_stats = legacy_manager
        .get_file_template_stats(legacy_file)
        .await
        .unwrap();
    let g2_stats = legacy_manager
        .get_file_template_stats(g2_file)
        .await
        .unwrap();
    assert_eq!(legacy_stats.len(), 1);
    assert_eq!(g2_stats.len(), 1);
    assert_eq!(g2_stats[0].request_count, 2);

    // Explicit erasure reaches generation 2 and removes routes with rows.
    g2_manager.delete_file(g2_file).await.unwrap();
    while g2_manager.purge_orphaned_rows(100).await.unwrap() > 0 {}
    let (left_rows, left_routes): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM request_templates_g2 WHERE file_id = $1), \
                (SELECT COUNT(*) FROM request_template_routes)",
    )
    .bind(*g2_file)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        (left_rows, left_routes),
        (0, 0),
        "explicit erasure must remove generation-2 rows and their routes"
    );
}
