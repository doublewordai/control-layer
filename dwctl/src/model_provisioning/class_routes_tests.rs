use super::*;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use tempfile::TempDir;
use uuid::Uuid;

fn fixture() -> serde_yaml::Value {
    serde_yaml::from_str(
        r#"
model: example/model
clay:
  alias: example/model
  deployments:
    - alias: legacy-example/model
      model_name: legacy-upstream
      endpoint: gateway
  routing:
    pools:
      default:
        - deployment: legacy-example/model
  tariffs:
    - name: compatibility
      purpose: realtime
      input_per_million_tokens: '1'
      output_per_million_tokens: '2'
  cache_tariff:
    write_multiplier_5m: '1.25'
    write_multiplier_1h: '2'
    write_multiplier_24h: '2.5'
    read_multiplier: '0.1'
    min_prefix_tokens: 1024
  class_routes:
    standard:
      display_name: Standard
      endpoint: gateway
      upstream_model_name: dynamo-example/throughput
      tariffs:
        - name: standard
          purpose: realtime
          input_per_million_tokens: '3'
          output_per_million_tokens: '4'
    fast:
      display_name: Fast
      endpoint: gateway
      upstream_model_name: dynamo-example/fast
      aliases: [example/model-fast]
      tariffs:
        - name: fast
          purpose: realtime
          input_per_million_tokens: '5'
          output_per_million_tokens: '6'
"#,
    )
    .unwrap()
}

fn catalog(value: &serde_yaml::Value) -> Result<Catalog> {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("model.yaml"), serde_yaml::to_string(value).unwrap()).unwrap();
    Catalog::load(dir.path())
}

async fn setup(pool: &PgPool) {
    sqlx::query("INSERT INTO inference_endpoints (name,url,created_by) VALUES ('gateway','http://gateway.test','00000000-0000-0000-0000-000000000000')")
        .execute(pool).await.unwrap();
}

#[test]
fn class_catalog_rejects_missing_fast() {
    let mut v = fixture();
    v["clay"]["class_routes"].as_mapping_mut().unwrap().remove("fast");
    assert!(catalog(&v).unwrap_err().to_string().contains("standard and fast"));
}

#[test]
fn class_catalog_rejects_synonym_shadowing_primary_fast_name() {
    let mut v = fixture();
    v["clay"]["class_routes"]["fast"]["aliases"] = serde_yaml::to_value(["example/model:fast"]).unwrap();
    assert!(catalog(&v).is_err());
}

#[test]
fn class_catalog_rejects_activation_in_yaml() {
    let mut v = fixture();
    v["clay"]["routing_mode"] = serde_yaml::Value::String("class_routes".into());
    assert!(catalog(&v).is_err());
}

#[sqlx::test]
async fn staging_classes_preserves_legacy_route_and_effective_price(pool: PgPool) {
    setup(&pool).await;
    apply(&pool, &catalog(&fixture()).unwrap()).await.unwrap();
    let (id, mode, composite, targets): (Uuid, String, bool, serde_json::Value) =
        sqlx::query_as("SELECT id,routing_mode,is_composite,serving_classes FROM deployed_models WHERE alias='example/model'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(mode, "legacy");
    assert!(composite);
    assert_eq!(targets, serde_json::json!({}));
    let members: i64 = sqlx::query_scalar("SELECT count(*) FROM deployed_model_components WHERE composite_model_id=$1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(members, 1);
    let names: Vec<String> = sqlx::query_scalar("SELECT alias FROM model_aliases ORDER BY alias")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(names, ["example/model-fast"]);
    for class in [None, Some("standard"), Some("fast")] {
        let price: Decimal =
            sqlx::query_scalar("SELECT input_price_per_token FROM effective_model_tariff($1,NULL,'realtime',NULL,$2,NOW())")
                .bind(id)
                .bind(class)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(price, parse_per_million("1").unwrap());
    }
    let mut tx = pool.begin().await.unwrap();
    let current = crate::db::handlers::Tariffs::new(&mut tx).list_current_by_model(id).await.unwrap();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].name, "compatibility");
}

#[sqlx::test]
async fn repeated_catalog_preserves_class_ids_timestamps_and_price_versions(pool: PgPool) {
    setup(&pool).await;
    let catalog = catalog(&fixture()).unwrap();
    apply(&pool, &catalog).await.unwrap();
    let before: Vec<(Uuid, DateTime<Utc>)> = sqlx::query_as("SELECT id,updated_at FROM model_serving_classes ORDER BY class_key")
        .fetch_all(&pool)
        .await
        .unwrap();
    let versions: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM model_tariffs ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    apply(&pool, &catalog).await.unwrap();
    let after: Vec<(Uuid, DateTime<Utc>)> = sqlx::query_as("SELECT id,updated_at FROM model_serving_classes ORDER BY class_key")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(before, after);
    let after: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM model_tariffs ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(versions, after);
}

#[sqlx::test]
async fn class_destination_edit_keeps_identity_and_versions_only_changed_price(pool: PgPool) {
    setup(&pool).await;
    let mut v = fixture();
    apply(&pool, &catalog(&v).unwrap()).await.unwrap();
    let id: Uuid = sqlx::query_scalar("SELECT id FROM model_serving_classes WHERE class_key='fast'")
        .fetch_one(&pool)
        .await
        .unwrap();
    v["clay"]["class_routes"]["fast"]["upstream_model_name"] = serde_yaml::Value::String("different-upstream".into());
    v["clay"]["class_routes"]["fast"]["tariffs"][0]["input_per_million_tokens"] = serde_yaml::Value::String("0".into());
    apply(&pool, &catalog(&v).unwrap()).await.unwrap();
    let after: (Uuid, String) = sqlx::query_as("SELECT id,upstream_model_name FROM model_serving_classes WHERE class_key='fast'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(after, (id, "different-upstream".into()));
    let counts: (i64, i64) =
        sqlx::query_as("SELECT count(*),count(*) FILTER (WHERE valid_until IS NULL) FROM model_tariffs WHERE serving_class='fast'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(counts, (2, 1));
}

#[sqlx::test]
async fn omitted_class_configuration_retires_prices_without_erasing_history(pool: PgPool) {
    setup(&pool).await;
    let mut v = fixture();
    apply(&pool, &catalog(&v).unwrap()).await.unwrap();
    v["clay"].as_mapping_mut().unwrap().remove("class_routes");
    apply(&pool, &catalog(&v).unwrap()).await.unwrap();
    for table in ["model_serving_classes", "model_aliases"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
    let counts: (i64, i64) =
        sqlx::query_as("SELECT count(*),count(*) FILTER (WHERE valid_until IS NULL) FROM model_tariffs WHERE serving_class IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(counts, (2, 0));
}

#[sqlx::test]
async fn synonym_collision_with_existing_private_model_rolls_back_catalog(pool: PgPool) {
    setup(&pool).await;
    sqlx::query("INSERT INTO deployed_models (alias,model_name,is_composite,created_by) VALUES ('example/model-fast','private',true,'00000000-0000-0000-0000-000000000000')").execute(&pool).await.unwrap();
    assert!(apply(&pool, &catalog(&fixture()).unwrap()).await.is_err());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM model_serving_classes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test]
async fn catalog_refuses_activated_models_without_resetting_operator_mode(pool: PgPool) {
    setup(&pool).await;
    let c = catalog(&fixture()).unwrap();
    apply(&pool, &c).await.unwrap();
    sqlx::query("UPDATE deployed_models SET routing_mode='class_routes' WHERE alias='example/model'")
        .execute(&pool)
        .await
        .unwrap();
    let error = apply(&pool, &c).await.unwrap_err();
    assert!(error.to_string().contains("activated"));
    let mode: String = sqlx::query_scalar("SELECT routing_mode FROM deployed_models WHERE alias='example/model'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(mode, "class_routes");
}

#[sqlx::test]
async fn ordinary_model_creation_cannot_claim_a_class_synonym(pool: PgPool) {
    use crate::db::handlers::{Deployments, repository::Repository};
    use crate::db::models::deployments::DeploymentCreateDBRequest;
    setup(&pool).await;
    apply(&pool, &catalog(&fixture()).unwrap()).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    for name in ["example/model-fast", "example/model:fast"] {
        let request = DeploymentCreateDBRequest::builder()
            .created_by(Uuid::nil())
            .alias(name.into())
            .model_name(name.into())
            .is_composite(true)
            .build();
        assert!(Deployments::new(&mut conn).create(&request).await.is_err());
    }
}

#[sqlx::test]
async fn renaming_a_model_cannot_shadow_an_existing_synonym_with_its_class_name(pool: PgPool) {
    use crate::db::handlers::{Deployments, repository::Repository};
    use crate::db::models::deployments::DeploymentUpdateDBRequest;
    setup(&pool).await;
    let mut v = fixture();
    v["clay"]["class_routes"]["fast"]["aliases"] = serde_yaml::to_value(["renamed/model:fast"]).unwrap();
    apply(&pool, &catalog(&v).unwrap()).await.unwrap();
    let id: Uuid = sqlx::query_scalar("SELECT id FROM deployed_models WHERE alias='example/model'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let request = DeploymentUpdateDBRequest::builder().alias("renamed/model".into()).build();
    let error = Deployments::new(&mut conn).update(id, &request).await.unwrap_err();
    assert!(matches!(error, crate::db::errors::DbError::UniqueViolation { .. }));
    let alias: String = sqlx::query_scalar("SELECT alias FROM deployed_models WHERE id=$1")
        .bind(id)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(alias, "example/model");
}

#[sqlx::test]
async fn concurrent_model_writer_waits_for_catalog_and_then_rejects_collision(pool: PgPool) {
    use crate::db::handlers::{Deployments, repository::Repository};
    use crate::db::models::deployments::DeploymentCreateDBRequest;
    setup(&pool).await;
    let mut catalog_tx = pool.begin().await.unwrap();
    ModelProvisioning::new(&mut catalog_tx)
        .apply(&catalog(&fixture()).unwrap())
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()").fetch_one(&mut *conn).await.unwrap();
    let request = DeploymentCreateDBRequest::builder()
        .created_by(Uuid::nil())
        .alias("example/model-fast".into())
        .model_name("competing".into())
        .is_composite(true)
        .build();
    let (result, ()) = tokio::join!(async { Deployments::new(&mut conn).create(&request).await }, async {
        crate::test::utils::wait_for_advisory_waiter(&pool, pid).await;
        catalog_tx.commit().await.unwrap();
    });
    assert!(result.is_err());
}

#[sqlx::test]
async fn class_cache_prices_do_not_replace_model_enablement_or_legacy_multipliers(pool: PgPool) {
    use crate::db::handlers::cache_tariffs::CacheTariffs;
    setup(&pool).await;
    let mut v = fixture();
    v["clay"]["class_routes"]["fast"]["cache_tariff"] =
        serde_yaml::from_str("write_multiplier_5m: '2'\nwrite_multiplier_1h: '3'\nwrite_multiplier_24h: '4'\nread_multiplier: '0'")
            .unwrap();
    apply(&pool, &catalog(&v).unwrap()).await.unwrap();
    let id: Uuid = sqlx::query_scalar("SELECT id FROM deployed_models WHERE alias='example/model'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let mut tariffs = CacheTariffs::new(&mut conn);
    let general = tariffs.get_active(id).await.unwrap().unwrap();
    assert_eq!(general.read_multiplier, Decimal::new(1, 1));
    assert_eq!(general.min_prefix_tokens, 1024);
    tariffs.disable(id).await.unwrap();
    assert!(tariffs.get_active(id).await.unwrap().is_none());
    let active: i64 = sqlx::query_scalar("SELECT count(*) FROM model_cache_tariffs WHERE serving_class='fast' AND valid_until IS NULL")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(active, 1, "legacy cache edits must not overwrite dormant class prices");
}
