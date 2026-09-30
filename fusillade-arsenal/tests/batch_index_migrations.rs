//! Migration-runner coverage for the concurrently built `batches` indexes.
//!
//! Each index ships as build (`CREATE INDEX CONCURRENTLY IF NOT EXISTS`),
//! reindex and validate migrations. `IF NOT EXISTS` only checks the name, so
//! the sequence must, through the real SQLx runner and its bookkeeping, adopt a
//! correctly prebuilt index, repair the invalid remnant of an interrupted
//! build, and refuse a same-name index with the wrong definition.

use std::borrow::Cow;

use fusillade_arsenal::MIGRATOR;
use sqlx::PgPool;
use sqlx::migrate::Migrator;

struct IndexMigrations {
    name: &'static str,
    /// Version of the concurrent build migration.
    build: i64,
    /// Version of the validation migration that ends the sequence.
    validate: i64,
    /// The build migration itself, run by hand the way a predeploy would.
    build_sql: &'static str,
    /// A same-name index that the validation migration must reject.
    wrong_definition: &'static str,
}

const CANCELLING: IndexMigrations = IndexMigrations {
    name: "idx_batches_cancelling",
    build: 20260930150010,
    validate: 20260930150030,
    build_sql: include_str!("../migrations/20260930150010_add_batches_cancelling_index.up.sql"),
    // Missing the deleted_at condition.
    wrong_definition: "CREATE INDEX idx_batches_cancelling ON batches (id) WHERE cancelling_at IS NOT NULL",
};

const NOTIFICATION_DUE: IndexMigrations = IndexMigrations {
    name: "idx_batches_notification_due",
    build: 20260930151010,
    validate: 20260930151030,
    build_sql: include_str!(
        "../migrations/20260930151010_add_batches_notification_due_index.up.sql"
    ),
    // Right predicate, descending key: the claim orders oldest-frozen first.
    wrong_definition: "CREATE INDEX idx_batches_notification_due ON batches (counts_frozen_at DESC)
        WHERE counts_frozen_at IS NOT NULL AND notification_sent_at IS NULL
          AND cancelling_at IS NULL AND deleted_at IS NULL AND total_requests > 0",
};

/// Apply every migration that precedes the index's build migration.
async fn migrate_to_before(pool: &PgPool, index: &IndexMigrations) {
    let baseline = Migrator {
        migrations: Cow::Owned(
            MIGRATOR
                .iter()
                .filter(|migration| migration.version < index.build)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
    };
    baseline.run(pool).await.unwrap();
}

async fn recorded_migrations(pool: &PgPool, index: &IndexMigrations) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM _sqlx_migrations WHERE version BETWEEN $1 AND $2 AND success",
    )
    .bind(index.build)
    .bind(index.validate)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// The index is valid, ready and commented, and all three migrations are
/// recorded as applied.
async fn assert_adopted(pool: &PgPool, index: &IndexMigrations) {
    let (usable, comment): (bool, Option<String>) = sqlx::query_as(
        "SELECT i.indisvalid AND i.indisready, obj_description(i.indexrelid, 'pg_class')
         FROM pg_index i WHERE i.indexrelid = to_regclass($1)",
    )
    .bind(index.name)
    .fetch_optional(pool)
    .await
    .unwrap()
    .unwrap_or_else(|| panic!("{} is missing", index.name));
    assert!(usable, "{} must be valid and ready", index.name);
    assert!(comment.is_some(), "{} must carry its comment", index.name);
    assert_eq!(recorded_migrations(pool, index).await, 3);
}

async fn fresh(pool: PgPool, index: &IndexMigrations) {
    migrate_to_before(&pool, index).await;
    MIGRATOR.run(&pool).await.unwrap();
    assert_adopted(&pool, index).await;
}

async fn prebuilt(pool: PgPool, index: &IndexMigrations) {
    migrate_to_before(&pool, index).await;
    sqlx::raw_sql(index.build_sql).execute(&pool).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();
    assert_adopted(&pool, index).await;
}

async fn invalid_remnant(pool: PgPool, index: &IndexMigrations) {
    migrate_to_before(&pool, index).await;
    sqlx::raw_sql(index.build_sql).execute(&pool).await.unwrap();
    // The catalog state an interrupted concurrent build leaves behind. Only
    // in this isolated test database.
    sqlx::query("UPDATE pg_index SET indisvalid = false WHERE indexrelid = to_regclass($1)")
        .bind(index.name)
        .execute(&pool)
        .await
        .unwrap();
    let valid: bool =
        sqlx::query_scalar("SELECT indisvalid FROM pg_index WHERE indexrelid = to_regclass($1)")
            .bind(index.name)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!valid, "{} must start out invalid", index.name);
    MIGRATOR.run(&pool).await.unwrap();
    assert_adopted(&pool, index).await;
}

async fn wrong_definition(pool: PgPool, index: &IndexMigrations) {
    migrate_to_before(&pool, index).await;
    sqlx::raw_sql(index.wrong_definition)
        .execute(&pool)
        .await
        .unwrap();
    let error = MIGRATOR.run(&pool).await.unwrap_err();
    assert!(
        error.to_string().contains("wrong definition"),
        "{}: {error}",
        index.name
    );
    let validated: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM _sqlx_migrations WHERE version = $1)")
            .bind(index.validate)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        !validated,
        "a rejected index must not record its validation"
    );
}

#[sqlx::test(migrations = false)]
async fn cancelling_index_applies_fresh(pool: PgPool) {
    fresh(pool, &CANCELLING).await;
}

#[sqlx::test(migrations = false)]
async fn cancelling_index_adopts_a_prebuilt_index(pool: PgPool) {
    prebuilt(pool, &CANCELLING).await;
}

#[sqlx::test(migrations = false)]
async fn cancelling_index_repairs_an_invalid_remnant(pool: PgPool) {
    invalid_remnant(pool, &CANCELLING).await;
}

#[sqlx::test(migrations = false)]
async fn cancelling_index_rejects_a_same_name_wrong_definition(pool: PgPool) {
    wrong_definition(pool, &CANCELLING).await;
}

#[sqlx::test(migrations = false)]
async fn notification_due_index_applies_fresh(pool: PgPool) {
    fresh(pool, &NOTIFICATION_DUE).await;
}

#[sqlx::test(migrations = false)]
async fn notification_due_index_adopts_a_prebuilt_index(pool: PgPool) {
    prebuilt(pool, &NOTIFICATION_DUE).await;
}

#[sqlx::test(migrations = false)]
async fn notification_due_index_repairs_an_invalid_remnant(pool: PgPool) {
    invalid_remnant(pool, &NOTIFICATION_DUE).await;
}

#[sqlx::test(migrations = false)]
async fn notification_due_index_rejects_a_same_name_wrong_definition(pool: PgPool) {
    wrong_definition(pool, &NOTIFICATION_DUE).await;
}
