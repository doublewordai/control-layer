//! Startup synonym snapshots, independently of request routing and authorization.

use dwctl::inference::model_aliases::ModelAliasMap;
use sqlx::PgPool;
use uuid::Uuid;

async fn add_fast_alias(pool: &PgPool) -> (Uuid, Uuid) {
    let endpoint = Uuid::new_v4();
    let model = Uuid::new_v4();
    let class = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO inference_endpoints (id,name,url,created_by)
         VALUES ($1,'gateway','http://gateway.test','00000000-0000-0000-0000-000000000000')",
    )
    .bind(endpoint)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO deployed_models (id,model_name,alias,hosted_on,created_by)
         VALUES ($1,'old-upstream','example/model',$2,'00000000-0000-0000-0000-000000000000')",
    )
    .bind(model)
    .bind(endpoint)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO model_serving_classes
         (id,deployed_model_id,class_key,display_name,inference_endpoint_id,upstream_model_name)
         VALUES ($1,$2,'fast','Fast',$3,'dynamo-example/fast')",
    )
    .bind(class)
    .bind(model)
    .bind(endpoint)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO model_aliases (alias,deployed_model_id,serving_class_id) VALUES ('example/model-fast',$1,$2)")
        .bind(model)
        .bind(class)
        .execute(pool)
        .await
        .unwrap();
    (model, class)
}

#[sqlx::test]
async fn empty_synonym_table_does_not_reserve_primary_names(pool: PgPool) {
    let aliases = ModelAliasMap::load(&pool).await.unwrap();
    for name in ["example/model", "example/model:fast", "example/new-model", "example/model-fast"] {
        assert!(aliases.resolve(name).is_none());
    }
}

#[sqlx::test]
async fn synonym_loads_canonical_identity_without_activating_dormant_routes(pool: PgPool) {
    let (model, class) = add_fast_alias(&pool).await;
    let aliases = ModelAliasMap::load(&pool).await.unwrap();
    let target = aliases.resolve("example/model-fast").unwrap();
    assert_eq!(target.deployed_model_id, model);
    assert_eq!(target.serving_class_id, class);
    assert_eq!(target.canonical_alias, "example/model");
    assert_eq!(target.class_key, "fast");
    let mode: String = sqlx::query_scalar("SELECT routing_mode FROM deployed_models WHERE id=$1")
        .bind(model)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(mode, "legacy");
}

#[sqlx::test]
async fn lookup_is_exact_and_does_not_need_a_live_database(pool: PgPool) {
    add_fast_alias(&pool).await;
    let aliases = ModelAliasMap::load(&pool).await.unwrap();
    pool.close().await;
    assert!(aliases.resolve("example/model-fast").is_some());
    for name in [
        "example/model",
        "example/model:fast",
        "EXAMPLE/model-fast",
        "example/model-fast:high",
        "example/model_fast",
    ] {
        assert!(aliases.resolve(name).is_none(), "unexpected synonym: {name}");
    }
}

#[sqlx::test]
async fn new_synonyms_arrive_only_in_a_new_snapshot(pool: PgPool) {
    let (model, class) = add_fast_alias(&pool).await;
    let old = ModelAliasMap::load(&pool).await.unwrap();
    let shared = old.clone();
    sqlx::query("INSERT INTO model_aliases (alias,deployed_model_id,serving_class_id) VALUES ('example/model-quick',$1,$2)")
        .bind(model)
        .bind(class)
        .execute(&pool)
        .await
        .unwrap();
    let new = ModelAliasMap::load(&pool).await.unwrap();
    assert!(old.resolve("example/model-quick").is_none());
    assert!(shared.resolve("example/model-quick").is_none());
    let added = new.resolve("example/model-quick").unwrap();
    let existing = old.resolve("example/model-fast").unwrap();
    assert_eq!(added.deployed_model_id, existing.deployed_model_id);
    assert_eq!(added.serving_class_id, existing.serving_class_id);
    assert_eq!(added.class_key, existing.class_key);
}

#[sqlx::test]
async fn destination_changes_do_not_change_alias_identity(pool: PgPool) {
    let (_, class) = add_fast_alias(&pool).await;
    let before = ModelAliasMap::load(&pool).await.unwrap();
    sqlx::query("UPDATE model_serving_classes SET upstream_model_name='another-upstream', display_name='Renamed display' WHERE id=$1")
        .bind(class)
        .execute(&pool)
        .await
        .unwrap();
    let after = ModelAliasMap::load(&pool).await.unwrap();
    assert_eq!(before.resolve("example/model-fast"), after.resolve("example/model-fast"));
}

#[sqlx::test]
async fn deleted_models_are_excluded_from_new_snapshots(pool: PgPool) {
    let (model, _) = add_fast_alias(&pool).await;
    let old = ModelAliasMap::load(&pool).await.unwrap();
    sqlx::query("UPDATE deployed_models SET deleted=true WHERE id=$1")
        .bind(model)
        .execute(&pool)
        .await
        .unwrap();
    let new = ModelAliasMap::load(&pool).await.unwrap();
    assert!(new.resolve("example/model-fast").is_none());
    // A startup snapshot deliberately stays unchanged. Dispatch must recheck
    // live authorization/availability; a mapping is never an access grant.
    assert!(old.resolve("example/model-fast").is_some());
}
