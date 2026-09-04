//! The forward migration that retires the generation-1 request template heap
//! must fail closed while any legacy row is still reachable, and must leave
//! the generation-transparent views resolving generation-2 rows only once it
//! has run.

use std::borrow::Cow;

use fusillade_arsenal::MIGRATOR;
use sqlx::PgPool;
use uuid::Uuid;

const DROP_LEGACY_HEAP_MIGRATION: i64 = 20260905000000;

fn migrator_where(predicate: impl Fn(i64) -> bool) -> sqlx::migrate::Migrator {
    sqlx::migrate::Migrator {
        migrations: Cow::Owned(
            MIGRATOR
                .iter()
                .filter(|migration| predicate(migration.version))
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    }
}

/// Apply every migration before the retirement, leaving the legacy heap in
/// place so a test can populate it.
async fn migrate_to_before_retirement(pool: &PgPool) {
    migrator_where(|version| version < DROP_LEGACY_HEAP_MIGRATION)
        .run(pool)
        .await
        .expect("migrations before the retirement must apply");
}

async fn apply_retirement(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    migrator_where(|version| version <= DROP_LEGACY_HEAP_MIGRATION)
        .run(pool)
        .await
}

fn sqlstate(error: &sqlx::migrate::MigrateError) -> Option<String> {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(candidate) = current {
        if let Some(sqlx::Error::Database(database_error)) = candidate.downcast_ref::<sqlx::Error>()
        {
            return database_error.code().map(|code| code.into_owned());
        }
        current = candidate.source();
    }
    None
}

async fn insert_legacy_template(pool: &PgPool, file_id: Option<Uuid>) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO request_templates (file_id, endpoint, method, path, body, model, api_key) \
         VALUES ($1, 'https://example.invalid', 'POST', '/v1/x', '{\"gen\":1}', \
                 'test-model', 'test-key') RETURNING id",
    )
    .bind(file_id)
    .fetch_one(pool)
    .await
    .expect("legacy template must insert")
}

async fn current_week(pool: &PgPool) -> chrono::NaiveDate {
    let week: chrono::NaiveDate = sqlx::query_scalar(
        "SELECT (date_trunc('week', statement_timestamp() AT TIME ZONE 'UTC'))::date",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query("SELECT ensure_request_template_partition($1, NULL)")
        .bind(week)
        .execute(pool)
        .await
        .unwrap();
    week
}

async fn insert_g2_template(pool: &PgPool, week: chrono::NaiveDate) -> Uuid {
    let template_id: Uuid = sqlx::query_scalar(
        "INSERT INTO request_templates_g2 (created_on, endpoint, method, path, body, model, api_key) \
         VALUES ($1, 'https://example.invalid', 'POST', '/v1/x', '{\"gen\":2}', \
                 'test-model', 'test-key') RETURNING id",
    )
    .bind(week)
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO request_template_routes (template_id, week_start) VALUES ($1, $2)")
        .bind(template_id)
        .bind(week)
        .execute(pool)
        .await
        .unwrap();
    template_id
}

#[sqlx::test(migrations = false)]
async fn refuses_while_a_live_request_references_a_legacy_template(pool: PgPool) {
    migrate_to_before_retirement(&pool).await;
    let week = current_week(&pool).await;
    // Generation-2 writes have moved on, so the age probe alone would pass.
    insert_g2_template(&pool, week).await;
    let legacy_id = insert_legacy_template(&pool, None).await;
    sqlx::query(
        "UPDATE request_templates SET created_at = NOW() - INTERVAL '30 days' WHERE id = $1",
    )
    .bind(legacy_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO requests (template_id, model, state, created_by) \
         VALUES ($1, 'test-model', 'pending', 'tester')",
    )
    .bind(legacy_id)
    .execute(&pool)
    .await
    .unwrap();

    let error = apply_retirement(&pool)
        .await
        .expect_err("a referenced legacy template must block the retirement");
    assert_eq!(
        sqlstate(&error).as_deref(),
        Some("55000"),
        "the guard must fail closed with object_not_in_prerequisite_state: {error}"
    );

    // Nothing was dropped: the heap, its row, and the two-arm view survive.
    let (legacy_rows, view_rows): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM request_templates), \
                (SELECT COUNT(*) FROM request_templates_all WHERE id = $1)",
    )
    .bind(legacy_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((legacy_rows, view_rows), (1, 1));
}

#[sqlx::test(migrations = false)]
async fn refuses_while_the_legacy_heap_is_still_being_written(pool: PgPool) {
    migrate_to_before_retirement(&pool).await;
    let week = current_week(&pool).await;
    insert_g2_template(&pool, week).await;
    // An unreferenced legacy row written this week means generation-1 writes
    // have not stopped.
    insert_legacy_template(&pool, None).await;

    let error = apply_retirement(&pool)
        .await
        .expect_err("recent legacy writes must block the retirement");
    assert_eq!(sqlstate(&error).as_deref(), Some("55000"), "{error}");
}

#[sqlx::test(migrations = false)]
async fn refuses_while_a_legacy_template_belongs_to_a_live_file(pool: PgPool) {
    migrate_to_before_retirement(&pool).await;
    let week = current_week(&pool).await;
    insert_g2_template(&pool, week).await;
    let file_id: Uuid = sqlx::query_scalar(
        "INSERT INTO files (name, size_bytes, size_finalized, status, purpose) \
         VALUES ('legacy-file', 0, TRUE, 'processed', 'batch') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let legacy_id = insert_legacy_template(&pool, Some(file_id)).await;
    sqlx::query(
        "UPDATE request_templates SET created_at = NOW() - INTERVAL '30 days' WHERE id = $1",
    )
    .bind(legacy_id)
    .execute(&pool)
    .await
    .unwrap();

    let error = apply_retirement(&pool)
        .await
        .expect_err("a live file's legacy template must block the retirement");
    assert_eq!(sqlstate(&error).as_deref(), Some("55000"), "{error}");
}

#[sqlx::test(migrations = false)]
async fn drops_an_empty_heap_and_collapses_the_views_to_generation_two(pool: PgPool) {
    migrate_to_before_retirement(&pool).await;
    let week = current_week(&pool).await;
    let g2_id = insert_g2_template(&pool, week).await;

    apply_retirement(&pool)
        .await
        .expect("an unreferenced, write-quiet heap must retire");

    let legacy_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_class WHERE relname = 'request_templates' \
                        AND relkind IN ('r', 'p'))",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!legacy_exists, "the legacy heap must be dropped");

    // Both views keep their names and shapes and resolve generation-2 rows,
    // including a dedicated (file-less) template in the active view.
    let active: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, body FROM active_request_templates")
            .fetch_all(&pool)
            .await
            .unwrap();
    let all: Vec<(Uuid, String)> = sqlx::query_as("SELECT id, body FROM request_templates_all")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(active, vec![(g2_id, "{\"gen\":2}".to_string())]);
    assert_eq!(all, vec![(g2_id, "{\"gen\":2}".to_string())]);

    let view_sources: Vec<String> =
        sqlx::query_scalar("SELECT pg_get_viewdef(to_regclass($1), true)")
            .bind("active_request_templates")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(
        view_sources
            .iter()
            .all(|definition| !definition.contains("UNION")),
        "the active view must no longer union two generations: {view_sources:?}"
    );
}

#[sqlx::test(migrations = false)]
async fn the_down_migration_restores_an_empty_heap_and_the_two_arm_views(pool: PgPool) {
    migrate_to_before_retirement(&pool).await;
    let week = current_week(&pool).await;
    let g2_id = insert_g2_template(&pool, week).await;
    apply_retirement(&pool).await.unwrap();

    migrator_where(|version| version <= DROP_LEGACY_HEAP_MIGRATION)
        .undo(&pool, DROP_LEGACY_HEAP_MIGRATION - 1)
        .await
        .expect("the down migration must apply");

    let legacy_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_templates")
        .fetch_one(&pool)
        .await
        .expect("the legacy heap must exist again");
    assert_eq!(legacy_rows, 0, "rows are never restored");

    let legacy_id = insert_legacy_template(&pool, None).await;
    let visible: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM request_templates_all ORDER BY body")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        visible,
        vec![legacy_id, g2_id],
        "the restored view must read both generations again"
    );
}
