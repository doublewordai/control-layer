//! The migrations that stop storing API key secrets: the retained-payload
//! scrub removes every stored key and is safe to re-run, and the down
//! migrations restore the columns and views the previous release reads.

use std::borrow::Cow;

use fusillade_arsenal::MIGRATOR;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

/// The last migration of the release that still has the key columns.
const BEFORE_DROP: i64 = 20261105105806;
const DROP_BATCHES_API_KEY: i64 = 20261105143830;
const SCRUB_RETAINED_PAYLOADS: i64 = 20261105143840;

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

async fn insert_retained(pool: &PgPool, kind: &str, payload: Value) -> (chrono::NaiveDate, Uuid) {
    let delete_on: chrono::NaiveDate =
        sqlx::query_scalar("SELECT (now() AT TIME ZONE 'UTC')::date + 2")
            .fetch_one(pool)
            .await
            .unwrap();
    sqlx::query("SELECT ensure_retained_response_partition($1, NULL)")
        .bind(delete_on)
        .execute(pool)
        .await
        .expect("retained response partition must be available");
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO retained_response_objects
             (delete_on, group_id, object_kind, object_id, request_id, schema_version, payload)
         VALUES ($1, $2, $3, $2, CASE WHEN $3 = 'request' THEN $2 END, 1, $4)",
    )
    .bind(delete_on)
    .bind(id)
    .bind(kind)
    .bind(payload)
    .execute(pool)
    .await
    .expect("retained object must insert");
    (delete_on, id)
}

async fn payload(pool: &PgPool, key: (chrono::NaiveDate, Uuid)) -> Value {
    sqlx::query_scalar(
        "SELECT payload FROM retained_response_objects WHERE delete_on = $1 AND object_id = $2",
    )
    .bind(key.0)
    .bind(key.1)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test(migrations = false)]
async fn the_scrub_removes_retained_keys_and_is_idempotent(pool: PgPool) {
    migrator_where(|version| version <= DROP_BATCHES_API_KEY)
        .run(&pool)
        .await
        .expect("migrations before the scrub must apply");

    let with_key = insert_retained(
        &pool,
        "request",
        json!({"request": {"id": "r1"}, "template": {"body": "b", "api_key": "sk-stored-secret"}}),
    )
    .await;
    let without_key = insert_retained(
        &pool,
        "request",
        json!({"request": {"id": "r2"}, "template": {"body": "b"}}),
    )
    .await;
    let group = insert_retained(&pool, "group", json!({"group_id": "g", "request_ids": []})).await;

    migrator_where(|version| version <= SCRUB_RETAINED_PAYLOADS)
        .run(&pool)
        .await
        .expect("the scrub must apply");

    assert_eq!(
        payload(&pool, with_key).await,
        json!({"request": {"id": "r1"}, "template": {"body": "b"}})
    );
    assert_eq!(
        payload(&pool, without_key).await,
        json!({"request": {"id": "r2"}, "template": {"body": "b"}})
    );
    assert_eq!(
        payload(&pool, group).await,
        json!({"group_id": "g", "request_ids": []})
    );

    // A re-run (an interrupted Job retrying) changes nothing.
    let scrub = MIGRATOR
        .iter()
        .find(|migration| {
            migration.version == SCRUB_RETAINED_PAYLOADS
                && !migration.migration_type.is_down_migration()
        })
        .expect("the scrub migration");
    let before: Vec<Value> =
        sqlx::query_scalar("SELECT payload FROM retained_response_objects ORDER BY object_id")
            .fetch_all(&pool)
            .await
            .unwrap();
    sqlx::raw_sql(&scrub.sql)
        .execute(&pool)
        .await
        .expect("the scrub re-runs");
    let after: Vec<Value> =
        sqlx::query_scalar("SELECT payload FROM retained_response_objects ORDER BY object_id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(after, before);
    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM retained_response_objects WHERE payload -> 'template' ? 'api_key'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(remaining, 0);
}

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
