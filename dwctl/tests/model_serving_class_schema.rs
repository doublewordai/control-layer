//! Storage invariants for the dormant model/class representation.

use std::borrow::Cow;

use sqlx::PgPool;
use sqlx::migrate::Migrator;
use uuid::Uuid;

use dwctl::db::models::model_aliases::ModelAlias;
use dwctl::db::models::model_serving_classes::ModelServingClass;
use dwctl::migrations::{Target, apply, check};

const CLASS_MIGRATION: i64 = 20261001120000;
const ALIAS_MIGRATION: i64 = 20261001120010;

struct Fixture {
    model: Uuid,
    other_model: Uuid,
    endpoint: Uuid,
}

async fn fixture(pool: &PgPool) -> Fixture {
    let user =
        sqlx::query_scalar::<_, Uuid>("INSERT INTO users (username, email) VALUES ('class-test', 'class-test@example.com') RETURNING id")
            .fetch_one(pool)
            .await
            .unwrap();
    let endpoint = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO inference_endpoints (name, url, created_by) VALUES ('test-upstream', 'http://localhost:8080', $1) RETURNING id",
    )
    .bind(user)
    .fetch_one(pool)
    .await
    .unwrap();
    let mut models = Vec::new();
    for alias in ["example/model", "example/other"] {
        let id = sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO deployed_models (model_name, alias, created_by, hosted_on) VALUES ($1, $1, $2, $3) RETURNING id",
        )
        .bind(alias)
        .bind(user)
        .bind(endpoint)
        .fetch_one(pool)
        .await
        .unwrap();
        models.push(id);
    }
    Fixture {
        model: models[0],
        other_model: models[1],
        endpoint,
    }
}

async fn insert_class(pool: &PgPool, model: Uuid, endpoint: Uuid, key: &str, upstream: &str) -> Result<ModelServingClass, sqlx::Error> {
    sqlx::query_as(
        "INSERT INTO model_serving_classes (deployed_model_id, class_key, display_name, inference_endpoint_id, upstream_model_name)
         VALUES ($1, $2, 'Test class', $3, $4)
         RETURNING id, deployed_model_id, class_key, display_name, inference_endpoint_id, upstream_model_name, created_at, updated_at",
    )
    .bind(model)
    .bind(key)
    .bind(endpoint)
    .bind(upstream)
    .fetch_one(pool)
    .await
}

fn assert_database_error(error: sqlx::Error, code: &str, constraint: &str) {
    let database = error.as_database_error().expect("expected a PostgreSQL constraint error");
    assert_eq!(database.code().as_deref(), Some(code), "{error}");
    assert_eq!(database.constraint(), Some(constraint), "{error}");
}

async fn insert_alias(pool: &PgPool, alias: &str, model: Uuid, class: Option<Uuid>) -> Result<ModelAlias, sqlx::Error> {
    sqlx::query_as(
        "INSERT INTO model_aliases (alias, deployed_model_id, serving_class_id) VALUES ($1, $2, $3)
         RETURNING alias, deployed_model_id, serving_class_id, created_at, updated_at",
    )
    .bind(alias)
    .bind(model)
    .bind(class)
    .fetch_one(pool)
    .await
}

#[sqlx::test]
async fn alternate_spellings_share_a_class_without_creating_models(pool: PgPool) {
    let f = fixture(&pool).await;
    let fast = insert_class(&pool, f.model, f.endpoint, "fast", "dynamo-example/model:fast")
        .await
        .unwrap();
    let hyphen = insert_alias(&pool, "example/model-fast", f.model, Some(fast.id)).await.unwrap();
    let alternate = insert_alias(&pool, "example/model-quick", f.model, Some(fast.id)).await.unwrap();
    assert_eq!(alternate.deployed_model_id, hyphen.deployed_model_id);
    assert_eq!(alternate.serving_class_id, hyphen.serving_class_id);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM deployed_models")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 2);
}

#[sqlx::test]
async fn an_alias_cannot_select_another_models_class(pool: PgPool) {
    let f = fixture(&pool).await;
    let fast = insert_class(&pool, f.other_model, f.endpoint, "fast", "dynamo-example/other:fast")
        .await
        .unwrap();
    let error = insert_alias(&pool, "example/model-fast", f.model, Some(fast.id)).await.unwrap_err();
    assert_database_error(error, "23503", "model_aliases_model_class_fkey");
}

#[sqlx::test]
async fn additional_aliases_require_an_explicit_class(pool: PgPool) {
    let f = fixture(&pool).await;
    let error = insert_alias(&pool, "example/model-fast", f.model, None).await.unwrap_err();
    let database = error.as_database_error().unwrap();
    assert_eq!(database.code().as_deref(), Some("23502"));
}

#[sqlx::test]
async fn a_class_with_aliases_cannot_be_deleted(pool: PgPool) {
    let f = fixture(&pool).await;
    let fast = insert_class(&pool, f.model, f.endpoint, "fast", "any-upstream").await.unwrap();
    insert_alias(&pool, "example/model-fast", f.model, Some(fast.id)).await.unwrap();
    let error = sqlx::query("DELETE FROM model_serving_classes WHERE id = $1")
        .bind(fast.id)
        .execute(&pool)
        .await
        .unwrap_err();
    let database = error.as_database_error().unwrap();
    assert!(matches!(database.code().as_deref(), Some("23001" | "23503")), "{error}");
    assert_eq!(database.constraint(), Some("model_aliases_model_class_fkey"));
}

#[sqlx::test]
async fn class_aliases_reject_empty_or_whitespace_names(pool: PgPool) {
    let f = fixture(&pool).await;
    let fast = insert_class(&pool, f.model, f.endpoint, "fast", "any-upstream").await.unwrap();
    for name in ["", " example/model", "example/model ", "example/model\nfast"] {
        let error = insert_alias(&pool, name, f.model, Some(fast.id)).await.unwrap_err();
        assert_database_error(error, "23514", "model_aliases_name_check");
    }
}

#[sqlx::test]
async fn standard_and_fast_share_a_model_but_have_distinct_service_identities(pool: PgPool) {
    let f = fixture(&pool).await;
    let standard = insert_class(&pool, f.model, f.endpoint, "standard", "example/model:throughput")
        .await
        .unwrap();
    let fast = insert_class(&pool, f.model, f.endpoint, "fast", "example/model:fast")
        .await
        .unwrap();
    assert_eq!(standard.deployed_model_id, fast.deployed_model_id);
    assert_ne!(standard.id, fast.id);
    assert_ne!(standard.class_key, fast.class_key);
    assert_eq!(standard.upstream_model_name, "example/model:throughput");
    assert_eq!(fast.upstream_model_name, "example/model:fast");
    let alias: String = sqlx::query_scalar("SELECT alias FROM deployed_models WHERE id = $1")
        .bind(f.model)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(alias, "example/model", "class creation must not rename the canonical model");
}

#[sqlx::test]
async fn class_keys_are_unique_within_a_model(pool: PgPool) {
    let f = fixture(&pool).await;
    insert_class(&pool, f.model, f.endpoint, "fast", "example/model:fast")
        .await
        .unwrap();
    let error = insert_class(&pool, f.model, f.endpoint, "fast", "example/other-upstream")
        .await
        .unwrap_err();
    assert_database_error(error, "23505", "model_serving_classes_model_key_unique");
}

#[sqlx::test]
async fn different_models_can_use_the_same_class_key(pool: PgPool) {
    let f = fixture(&pool).await;
    let first = insert_class(&pool, f.model, f.endpoint, "fast", "example/model:fast")
        .await
        .unwrap();
    let second = insert_class(&pool, f.other_model, f.endpoint, "fast", "example/other:fast")
        .await
        .unwrap();
    assert_ne!(first.id, second.id);
    assert_eq!(first.class_key, second.class_key);
}

#[sqlx::test]
async fn additional_classes_can_share_an_upstream_without_sharing_identity(pool: PgPool) {
    let f = fixture(&pool).await;
    let standard = insert_class(&pool, f.model, f.endpoint, "standard", "example/model:throughput")
        .await
        .unwrap();
    let economy = insert_class(&pool, f.model, f.endpoint, "economy", "example/model:throughput")
        .await
        .unwrap();
    assert_ne!(standard.id, economy.id);
    assert_eq!(standard.inference_endpoint_id, economy.inference_endpoint_id);
    assert_eq!(standard.upstream_model_name, economy.upstream_model_name);
}

#[sqlx::test]
async fn class_keys_reject_suffix_syntax_and_unstable_spellings(pool: PgPool) {
    let f = fixture(&pool).await;
    for key in ["", "Fast", "fast:extra", "example/fast", " fast", "fast "] {
        let error = insert_class(&pool, f.model, f.endpoint, key, "example/model:fast")
            .await
            .unwrap_err();
        assert_database_error(error, "23514", "model_serving_classes_class_key_check");
    }
}

#[sqlx::test]
async fn class_requires_an_existing_model(pool: PgPool) {
    let f = fixture(&pool).await;
    let error = insert_class(&pool, Uuid::new_v4(), f.endpoint, "fast", "example/model:fast")
        .await
        .unwrap_err();
    assert_database_error(error, "23503", "model_serving_classes_deployed_model_id_fkey");
}

#[sqlx::test]
async fn class_requires_an_existing_endpoint(pool: PgPool) {
    let f = fixture(&pool).await;
    let error = insert_class(&pool, f.model, Uuid::new_v4(), "fast", "example/model:fast")
        .await
        .unwrap_err();
    assert_database_error(error, "23503", "model_serving_classes_inference_endpoint_id_fkey");
}

#[sqlx::test]
async fn upstream_and_display_name_changes_preserve_class_identity(pool: PgPool) {
    let f = fixture(&pool).await;
    let original = insert_class(&pool, f.model, f.endpoint, "fast", "example/model:fast")
        .await
        .unwrap();
    let updated: ModelServingClass = sqlx::query_as(
        "UPDATE model_serving_classes SET display_name = 'Fast service', upstream_model_name = 'example/new-upstream'
         WHERE id = $1
         RETURNING id, deployed_model_id, class_key, display_name, inference_endpoint_id, upstream_model_name, created_at, updated_at",
    )
    .bind(original.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(updated.id, original.id);
    assert_eq!(updated.class_key, original.class_key);
    assert_eq!(updated.created_at, original.created_at);
    assert_eq!(updated.display_name, "Fast service");
    assert_eq!(updated.upstream_model_name, "example/new-upstream");
}

#[sqlx::test]
async fn a_model_with_classes_cannot_be_hard_deleted(pool: PgPool) {
    let f = fixture(&pool).await;
    insert_class(&pool, f.model, f.endpoint, "fast", "example/model:fast")
        .await
        .unwrap();
    let error = sqlx::query("DELETE FROM deployed_models WHERE id = $1")
        .bind(f.model)
        .execute(&pool)
        .await
        .unwrap_err();
    let database = error.as_database_error().unwrap();
    assert!(matches!(database.code().as_deref(), Some("23001" | "23503")), "{error}");
    assert_eq!(database.constraint(), Some("model_serving_classes_deployed_model_id_fkey"));
}

#[sqlx::test(migrations = false)]
async fn migration_preserves_legacy_rows_and_prepared_reads_without_activating_classes(pool: PgPool) {
    let current = sqlx::migrate!("./migrations");
    let previous = Migrator {
        migrations: Cow::Owned(current.iter().filter(|m| m.version < CLASS_MIGRATION).cloned().collect()),
        ..Migrator::DEFAULT
    };
    previous.run(&pool).await.unwrap();
    let f = fixture(&pool).await;

    // The existing public composite keeps its identity and legacy member relation.
    let composite: Uuid = sqlx::query_scalar(
        "INSERT INTO deployed_models (model_name, alias, created_by, is_composite)
         SELECT 'example/public', 'example/public', created_by, true FROM deployed_models WHERE id = $1 RETURNING id",
    )
    .bind(f.model)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO deployed_model_components (composite_model_id, deployed_model_id) VALUES ($1, $2)")
        .bind(composite)
        .bind(f.model)
        .execute(&pool)
        .await
        .unwrap();
    let members_before: Vec<serde_json::Value> = sqlx::query_scalar("SELECT to_jsonb(c) FROM deployed_model_components c ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();

    // Keep a prepared statement alive while the actual SQLx runner applies the expansion.
    let mut connection = pool.acquire().await.unwrap();
    let legacy_query = "SELECT id, alias, model_name, hosted_on, is_composite FROM deployed_models ORDER BY alias";
    let before: Vec<(Uuid, String, String, Option<Uuid>, bool)> = sqlx::query_as(legacy_query).fetch_all(&mut *connection).await.unwrap();
    current.run(&pool).await.unwrap();
    let after: Vec<(Uuid, String, String, Option<Uuid>, bool)> = sqlx::query_as(legacy_query).fetch_all(&mut *connection).await.unwrap();
    assert_eq!(before, after);
    assert!(after.iter().any(|row| row.0 == f.model));
    let members_after: Vec<serde_json::Value> = sqlx::query_scalar("SELECT to_jsonb(c) FROM deployed_model_components c ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(members_before, members_after);
    let previous_release = Target {
        name: "main",
        migrator: previous,
    };
    assert_eq!(
        check(&previous_release, &pool).await.unwrap().ahead,
        vec![CLASS_MIGRATION, ALIAS_MIGRATION]
    );
    let classes: i64 = sqlx::query_scalar("SELECT count(*) FROM model_serving_classes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(classes, 0, "migration must not activate or seed the new representation");
    let aliases: i64 = sqlx::query_scalar("SELECT count(*) FROM model_aliases")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(aliases, 0, "existing names must not be backfilled into optional synonyms");
    current.run(&pool).await.unwrap();
}

#[sqlx::test(migrations = false)]
async fn migration_runner_resumes_after_class_storage_without_reapplying_it(pool: PgPool) {
    let target = Target::main();
    target.run_to(CLASS_MIGRATION, &pool).await.unwrap();
    let f = fixture(&pool).await;
    let class = insert_class(&pool, f.model, f.endpoint, "fast", "configured-upstream")
        .await
        .unwrap();
    let aliases_exist: bool = sqlx::query_scalar("SELECT to_regclass('model_aliases') IS NOT NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(!aliases_exist);

    let report = apply(&target, &pool).await.unwrap();
    assert_eq!(report.applied, vec![ALIAS_MIGRATION]);
    let alias = insert_alias(&pool, "example/model-fast", f.model, Some(class.id)).await.unwrap();
    assert_eq!(alias.serving_class_id, class.id);
    assert!(apply(&target, &pool).await.unwrap().applied.is_empty());
}

#[sqlx::test]
async fn canonical_model_and_class_creation_leave_synonyms_empty(pool: PgPool) {
    let f = fixture(&pool).await;
    insert_class(&pool, f.model, f.endpoint, "standard", "dynamo-example/model:throughput")
        .await
        .unwrap();
    insert_class(&pool, f.model, f.endpoint, "fast", "dynamo-example/model:fast")
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM model_aliases")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test]
async fn model_rename_preserves_explicit_synonyms_without_creating_new_ones(pool: PgPool) {
    let f = fixture(&pool).await;
    let fast = insert_class(&pool, f.model, f.endpoint, "fast", "dynamo-example/model:fast")
        .await
        .unwrap();
    let before = insert_alias(&pool, "example/model-quick", f.model, Some(fast.id)).await.unwrap();
    sqlx::query("UPDATE deployed_models SET alias = 'example/renamed' WHERE id = $1")
        .bind(f.model)
        .execute(&pool)
        .await
        .unwrap();
    let aliases: Vec<ModelAlias> =
        sqlx::query_as("SELECT alias, deployed_model_id, serving_class_id, created_at, updated_at FROM model_aliases")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(aliases, vec![before]);
}

#[sqlx::test]
async fn duplicate_synonyms_cannot_select_different_models(pool: PgPool) {
    let f = fixture(&pool).await;
    let first = insert_class(&pool, f.model, f.endpoint, "fast", "upstream-one").await.unwrap();
    let second = insert_class(&pool, f.other_model, f.endpoint, "fast", "upstream-two")
        .await
        .unwrap();
    insert_alias(&pool, "example/quick", f.model, Some(first.id)).await.unwrap();
    let error = insert_alias(&pool, "example/quick", f.other_model, Some(second.id))
        .await
        .unwrap_err();
    assert_database_error(error, "23505", "model_aliases_pkey");
}

#[sqlx::test]
async fn removing_a_synonym_preserves_its_class_and_model(pool: PgPool) {
    let f = fixture(&pool).await;
    let fast = insert_class(&pool, f.model, f.endpoint, "fast", "upstream").await.unwrap();
    insert_alias(&pool, "example/quick", f.model, Some(fast.id)).await.unwrap();
    sqlx::query("DELETE FROM model_aliases WHERE alias = 'example/quick'")
        .execute(&pool)
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM model_serving_classes c JOIN deployed_models m ON m.id = c.deployed_model_id WHERE c.id = $1",
    )
    .bind(fast.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
}
