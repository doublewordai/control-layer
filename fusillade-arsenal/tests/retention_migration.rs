use std::borrow::Cow;

use fusillade_arsenal::MIGRATOR;

const RETENTION_MIGRATION: i64 = 20260813000000;

async fn live_heap_identities(pool: &sqlx::PgPool) -> Vec<(String, i64, i64)> {
    sqlx::query_as(
        r#"
        SELECT relation.relname,
               relation.oid::bigint,
               relation.relfilenode::bigint
        FROM pg_class relation
        JOIN pg_namespace namespace ON namespace.oid = relation.relnamespace
        WHERE namespace.nspname = current_schema()
          AND relation.relname IN ('requests', 'request_templates', 'response_steps')
        ORDER BY relation.relname
        "#,
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn live_heap_index_identities(pool: &sqlx::PgPool) -> Vec<(String, String, i64)> {
    sqlx::query_as(
        r#"
        SELECT heap.relname, index_relation.relname, index_relation.oid::bigint
        FROM pg_index index_catalog
        JOIN pg_class heap ON heap.oid = index_catalog.indrelid
        JOIN pg_class index_relation ON index_relation.oid = index_catalog.indexrelid
        JOIN pg_namespace namespace ON namespace.oid = heap.relnamespace
        WHERE namespace.nspname = current_schema()
          AND heap.relname IN ('requests', 'request_templates', 'response_steps')
        ORDER BY heap.relname, index_relation.relname
        "#,
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

#[sqlx::test(migrations = false)]
async fn adds_retained_response_parent_without_rewriting_live_heaps(pool: sqlx::PgPool) {
    let baseline = sqlx::migrate::Migrator {
        migrations: Cow::Owned(
            MIGRATOR
                .iter()
                .filter(|migration| migration.version < RETENTION_MIGRATION)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    baseline.run(&pool).await.unwrap();
    let live_heaps_before = live_heap_identities(&pool).await;
    let live_heap_indexes_before = live_heap_index_identities(&pool).await;

    MIGRATOR.run(&pool).await.unwrap();

    let range_partitioned: bool = sqlx::query_scalar(
        r#"
        SELECT partitioned.partstrat = 'r'
        FROM pg_partitioned_table partitioned
        JOIN pg_class relation ON relation.oid = partitioned.partrelid
        JOIN pg_namespace namespace ON namespace.oid = relation.relnamespace
        WHERE namespace.nspname = current_schema()
          AND relation.relname = 'retained_response_objects'
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("retained_response_objects must be a partitioned table");
    assert!(range_partitioned);

    let columns: Vec<String> = sqlx::query_scalar(
        r#"
        SELECT column_name
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name = 'retained_response_objects'
        ORDER BY ordinal_position
        "#,
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        columns,
        [
            "delete_on",
            "group_id",
            "object_kind",
            "object_id",
            "request_id",
            "head_step_id",
            "created_by",
            "service_tier",
            "state",
            "model",
            "created_at",
            "terminal_at",
            "step_sequence",
            "schema_version",
            "payload",
        ]
        .map(String::from)
    );

    assert_eq!(
        live_heap_identities(&pool).await,
        live_heaps_before,
        "the migration must not rename, replace, or rewrite a live payload heap"
    );
    assert_eq!(
        live_heap_index_identities(&pool).await,
        live_heap_indexes_before,
        "the deployment migration must not build an index on a live payload heap"
    );

    let parent_indexes: Vec<String> = sqlx::query_scalar(
        r#"
        SELECT index_relation.relname
        FROM pg_index index_catalog
        JOIN pg_class parent ON parent.oid = index_catalog.indrelid
        JOIN pg_class index_relation ON index_relation.oid = index_catalog.indexrelid
        JOIN pg_namespace namespace ON namespace.oid = parent.relnamespace
        WHERE namespace.nspname = current_schema()
          AND parent.relname = 'retained_response_objects'
          AND index_catalog.indisvalid
        ORDER BY index_relation.relname
        "#,
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    for required in [
        "idx_retained_response_objects_group",
        "idx_retained_response_objects_model_created",
        "idx_retained_response_objects_owner_created",
        "idx_retained_response_objects_request_id",
        "idx_retained_response_objects_request_object_id",
        "idx_retained_response_objects_state_created",
        "idx_retained_response_objects_step_object_id",
        "idx_retained_response_objects_tier_created",
    ] {
        assert!(
            parent_indexes.iter().any(|name| name == required),
            "missing valid parent index {required}"
        );
    }

    let control_tables: Vec<String> = sqlx::query_scalar(
        r#"
        SELECT table_name
        FROM information_schema.tables
        WHERE table_schema = current_schema()
          AND table_name IN (
              'retained_response_request_routes',
              'retained_response_step_routes',
              'retained_response_group_routes',
              'retained_response_buckets',
              'retention_partition_retirements'
          )
        ORDER BY table_name
        "#,
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(control_tables.len(), 5);

    let content_columns: Vec<(String, String)> = sqlx::query_as(
        r#"
        SELECT table_name, column_name
        FROM information_schema.columns
        WHERE table_schema = current_schema()
          AND table_name IN (
              'retained_response_request_routes',
              'retained_response_step_routes',
              'retained_response_group_routes',
              'retained_response_buckets',
              'retention_partition_retirements'
          )
          AND column_name IN (
              'created_by', 'owner_id', 'api_key', 'prompt', 'response', 'payload'
          )
        "#,
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        content_columns.is_empty(),
        "routing and retirement metadata must remain content-free"
    );

    let archive_index_ready: bool =
        sqlx::query_scalar("SELECT retained_response_archive_index_ready()")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        !archive_index_ready,
        "the deployment migration must not build an index on requests"
    );

    sqlx::query("SELECT ensure_retained_response_partition(DATE '2038-05-17')")
        .execute(&pool)
        .await
        .unwrap();
    let partition_oid: i64 =
        sqlx::query_scalar("SELECT 'retained_response_objects_d20380517'::regclass::oid::bigint")
            .fetch_one(&pool)
            .await
            .unwrap();
    sqlx::query("SELECT ensure_retained_response_partition(DATE '2038-05-17')")
        .execute(&pool)
        .await
        .unwrap();
    let same_partition_oid: i64 =
        sqlx::query_scalar("SELECT 'retained_response_objects_d20380517'::regclass::oid::bigint")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        same_partition_oid, partition_oid,
        "partition creation must be idempotent"
    );

    let bounds: String = sqlx::query_scalar(
        r#"
        SELECT pg_get_expr(child.relpartbound, child.oid)
        FROM pg_class child
        JOIN pg_namespace namespace ON namespace.oid = child.relnamespace
        WHERE namespace.nspname = current_schema()
          AND child.relname = 'retained_response_objects_d20380517'
        "#,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(bounds, "FOR VALUES FROM ('2038-05-17') TO ('2038-05-18')");

    let exact_bounds_check: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM pg_constraint constraint_catalog
            JOIN pg_class child ON child.oid = constraint_catalog.conrelid
            JOIN pg_namespace namespace ON namespace.oid = child.relnamespace
            WHERE namespace.nspname = current_schema()
              AND child.relname = 'retained_response_objects_d20380517'
              AND constraint_catalog.contype = 'c'
              AND constraint_catalog.convalidated
              AND pg_get_constraintdef(constraint_catalog.oid) LIKE
                  '%delete_on >= ''2038-05-17''::date%delete_on < ''2038-05-18''::date%'
        )
        "#,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        exact_bounds_check,
        "the standalone child check must remain validated after attach"
    );

    let created: i32 =
        sqlx::query_scalar("SELECT ensure_retained_response_partitions(DATE '2038-05-17', 2)")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(created, 2, "the existing first day must not be recounted");
    let created_again: i32 =
        sqlx::query_scalar("SELECT ensure_retained_response_partitions(DATE '2038-05-17', 2)")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(created_again, 0);

    let (concurrent_first, concurrent_second) = tokio::join!(
        sqlx::query("SELECT ensure_retained_response_partition(DATE '2038-05-20')").execute(&pool),
        sqlx::query("SELECT ensure_retained_response_partition(DATE '2038-05-20')").execute(&pool),
    );
    concurrent_first.unwrap();
    concurrent_second.unwrap();
    let concurrent_children: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM pg_class child
        JOIN pg_namespace namespace ON namespace.oid = child.relnamespace
        WHERE namespace.nspname = current_schema()
          AND child.relname = 'retained_response_objects_d20380520'
        "#,
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(concurrent_children, 1, "concurrent helpers must converge");

    let invalid_kind = sqlx::query(
        r#"
        INSERT INTO retained_response_objects (
            delete_on, group_id, object_kind, object_id, schema_version, payload
        ) VALUES (DATE '2038-05-17', gen_random_uuid(), 'template',
                  gen_random_uuid(), 1, '{}'::jsonb)
        "#,
    )
    .execute(&pool)
    .await
    .expect_err("unknown retained-object tags must fail closed");
    assert_eq!(
        invalid_kind
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    let invalid_state = sqlx::query(
        "UPDATE retained_response_buckets SET state = 'draining' WHERE delete_on = DATE '2038-05-17'",
    )
    .execute(&pool)
    .await
    .expect_err("unknown bucket states must fail closed");
    assert_eq!(
        invalid_state
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("23514")
    );

    let negative_runway =
        sqlx::query("SELECT ensure_retained_response_partitions(DATE '2038-05-17', -1)")
            .execute(&pool)
            .await
            .expect_err("negative partition runway must be rejected");
    assert_eq!(
        negative_runway
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("22023")
    );

    sqlx::query(
        r#"
        CREATE INDEX idx_requests_batchless_retention_due
        ON requests (
            service_tier,
            (CASE state
               WHEN 'completed' THEN completed_at
               WHEN 'failed' THEN failed_at
               WHEN 'canceled' THEN canceled_at
             END),
            id
        )
        WHERE batch_id IS NULL
          AND state IN ('completed', 'failed', 'canceled')
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    let archive_index_ready: bool =
        sqlx::query_scalar("SELECT retained_response_archive_index_ready()")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(archive_index_ready);

    let group_id = uuid::Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO retained_response_objects (
            delete_on, group_id, object_kind, object_id, schema_version, payload
        ) VALUES (DATE '2038-05-17', $1, 'group', $1, 1, '{}'::jsonb)
        "#,
    )
    .bind(group_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO retained_response_group_routes (group_id, delete_on) VALUES ($1, DATE '2038-05-17')",
    )
    .bind(group_id)
    .execute(&pool)
    .await
    .unwrap();

    let down_sql =
        include_str!("../migrations/20260813000000_add_partitioned_content_retention.down.sql");
    let rollback_with_route = sqlx::raw_sql(down_sql)
        .execute(&pool)
        .await
        .expect_err("routes must fence rollback");
    assert_eq!(
        rollback_with_route
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("55000")
    );

    sqlx::query("DELETE FROM retained_response_group_routes")
        .execute(&pool)
        .await
        .unwrap();
    let rollback_with_object = sqlx::raw_sql(down_sql)
        .execute(&pool)
        .await
        .expect_err("retained content must fence rollback");
    assert_eq!(
        rollback_with_object
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("55000")
    );

    sqlx::query("DELETE FROM retained_response_objects")
        .execute(&pool)
        .await
        .unwrap();
    let rollback_with_active_bucket = sqlx::raw_sql(down_sql)
        .execute(&pool)
        .await
        .expect_err("active buckets must fence rollback");
    assert_eq!(
        rollback_with_active_bucket
            .as_database_error()
            .and_then(|error| error.code())
            .as_deref(),
        Some("55000")
    );

    sqlx::query("UPDATE retained_response_buckets SET state = 'retired'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(down_sql)
        .execute(&pool)
        .await
        .expect("an empty retained store must roll back cleanly");
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = $1")
        .bind(RETENTION_MIGRATION)
        .execute(&pool)
        .await
        .unwrap();
    MIGRATOR
        .run(&pool)
        .await
        .expect("the migration must reapply after rollback");
    assert_eq!(live_heap_identities(&pool).await, live_heaps_before);

    sqlx::raw_sql(down_sql)
        .execute(&pool)
        .await
        .expect("a fresh empty retained store must roll back cleanly");
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = $1")
        .bind(RETENTION_MIGRATION)
        .execute(&pool)
        .await
        .unwrap();
    MIGRATOR
        .run(&pool)
        .await
        .expect("the empty up/down cycle must remain reversible");
    assert_eq!(live_heap_identities(&pool).await, live_heaps_before);
}
