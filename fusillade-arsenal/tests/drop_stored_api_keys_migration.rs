//! The down migrations that stop storing API key secrets restore the columns
//! and views the previous release reads. Retained-response payloads written
//! before 16.5.0 keep `template.api_key` until their partitions are retired.

use fusillade_arsenal::MIGRATOR;
use sqlx::PgPool;
use uuid::Uuid;

/// The last migration of the release that still has the key columns.
const BEFORE_DROP: i64 = 20261105105806;

async fn column(pool: &PgPool, table: &str) -> Option<(i32, String, Option<String>)> {
    sqlx::query_as(
        "SELECT ordinal_position, is_nullable, column_default FROM information_schema.columns
         WHERE table_schema = current_schema() AND table_name = $1 AND column_name = 'api_key'",
    )
    .bind(table)
    .fetch_optional(pool)
    .await
    .unwrap()
}

#[sqlx::test(migrations = false)]
async fn the_down_migrations_restore_the_columns_and_views(pool: PgPool) {
    MIGRATOR
        .run(&pool)
        .await
        .expect("every migration must apply");
    for table in [
        "request_templates_g2",
        "batches",
        "active_request_templates",
        "request_templates_all",
        "request_templates",
    ] {
        assert_eq!(column(&pool, table).await, None, "{table} has no api_key");
    }

    MIGRATOR
        .undo(&pool, BEFORE_DROP)
        .await
        .expect("the down migrations must apply");

    let (_, nullable, default) = column(&pool, "request_templates_g2")
        .await
        .expect("g2 api_key");
    assert_eq!(nullable, "NO");
    assert_eq!(default.as_deref(), Some("''::text"));
    let (_, nullable, _) = column(&pool, "batches").await.expect("batches api_key");
    assert_eq!(nullable, "YES");
    // In the position the previous release's views had it: after model.
    for view in [
        "active_request_templates",
        "request_templates_all",
        "request_templates",
    ] {
        let (position, _, _) = column(&pool, view)
            .await
            .unwrap_or_else(|| panic!("{view} api_key"));
        assert_eq!(position, 8, "{view}");
    }

    // The restored schema accepts the previous release's writes, which omit
    // the key from templates.
    let week: chrono::NaiveDate =
        sqlx::query_scalar("SELECT date_trunc('week', now() AT TIME ZONE 'UTC')::date")
            .fetch_one(&pool)
            .await
            .unwrap();
    sqlx::query("SELECT ensure_request_template_partition($1, NULL)")
        .bind(week)
        .execute(&pool)
        .await
        .unwrap();
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO request_templates_g2 (created_on, endpoint, method, path, model)
         VALUES ($1, 'e', 'POST', '/x', 'm') RETURNING id",
    )
    .bind(week)
    .fetch_one(&pool)
    .await
    .expect("a template without a key must insert");
    let stored: String =
        sqlx::query_scalar("SELECT api_key FROM request_templates_g2 WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, "");
}
