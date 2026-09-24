//! Exercise the shipped SQLx files, including autocommit and interrupted-build recovery.
use super::*;

const FIRST_INDEX: i64 = 20260924120010;
const LAST_VALIDATION: i64 = 20260924120630;
const ORG_BATCH: &str = "idx_model_tariffs_unique_active_org_batch_per_sla";

fn index_builds() -> Vec<sqlx::migrate::Migration> {
    Target::main()
        .migrator
        .iter()
        .filter(|m| m.version >= FIRST_INDEX && m.sql.contains("CREATE ") && m.no_tx)
        .cloned()
        .collect()
}

async fn assert_valid(pool: &PgPool) {
    check(&Target::main(), pool).await.unwrap();
    for build in index_builds() {
        let name = build.sql.split("IF NOT EXISTS ").nth(1).unwrap().split_whitespace().next().unwrap();
        let (valid, comment): (bool, Option<String>) = sqlx::query_as(
            "SELECT i.indisvalid AND i.indisready, obj_description(i.indexrelid, 'pg_class') FROM pg_index i WHERE i.indexrelid = to_regclass($1)",
        ).bind(name).fetch_one(pool).await.unwrap();
        assert!(valid, "{name}");
        assert!(comment.is_some(), "{name}");
    }
}

#[sqlx::test(migrations = false)]
async fn tariff_indexes_fresh_upgrade_preserves_rows_and_uniqueness(pool: PgPool) {
    let target = Target::main();
    target.run_to(155, &pool).await.unwrap();
    let user: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO users (username,email) VALUES ('index-test','index-test@example.invalid') RETURNING id")
            .fetch_one(&pool)
            .await
            .unwrap();
    let model: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO deployed_models (model_name,alias,created_by,type,is_composite) VALUES ('index-test','index-test',$1,'CHAT',true) RETURNING id",
    )
    .bind(user)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO model_tariffs (deployed_model_id,name,api_key_purpose,input_price_per_token,output_price_per_token) VALUES ($1,'general','realtime',1,2)")
        .bind(model).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO model_cache_tariffs (deployed_model_id,write_multiplier_5m,write_multiplier_1h,write_multiplier_24h,read_multiplier,min_prefix_tokens) VALUES ($1,1,2,3,0.1,1024)")
        .bind(model).execute(&pool).await.unwrap();
    target.run_to(LAST_VALIDATION, &pool).await.unwrap();
    // Old guards stay until every scoped replacement is built and validated.
    assert!(sqlx::query("INSERT INTO model_tariffs (deployed_model_id,name,api_key_purpose,input_price_per_token,output_price_per_token) VALUES ($1,'duplicate','realtime',1,2)")
        .bind(model).execute(&pool).await.is_err());
    apply(&target, &pool).await.unwrap();
    assert_valid(&pool).await;
    assert!(sqlx::query("INSERT INTO model_tariffs (deployed_model_id,name,api_key_purpose,input_price_per_token,output_price_per_token) VALUES ($1,'duplicate','realtime',1,2)")
        .bind(model).execute(&pool).await.is_err());
    for class in [None, Some("interactive"), Some("throughput")] {
        let statement = "INSERT INTO model_tariffs (deployed_model_id,user_id,serving_class,name,api_key_purpose,input_price_per_token,output_price_per_token) VALUES ($1,$2,$3,'deal','realtime',0,0)";
        sqlx::query(statement)
            .bind(model)
            .bind(user)
            .bind(class)
            .execute(&pool)
            .await
            .unwrap();
        let err = sqlx::query(statement)
            .bind(model)
            .bind(user)
            .bind(class)
            .execute(&pool)
            .await
            .unwrap_err();
        assert_eq!(err.as_database_error().unwrap().code().as_deref(), Some("23505"));
        let cache = "INSERT INTO model_cache_tariffs (deployed_model_id,user_id,serving_class,write_multiplier_5m,write_multiplier_1h,write_multiplier_24h,read_multiplier,min_prefix_tokens,valid_from) SELECT $1,$2,$3,1,2,3,0.1,1024,valid_from FROM model_cache_tariffs WHERE deployed_model_id=$1 AND user_id IS NULL";
        sqlx::query(cache).bind(model).bind(user).bind(class).execute(&pool).await.unwrap();
        assert!(sqlx::query(cache).bind(model).bind(user).bind(class).execute(&pool).await.is_err());
    }
    let prices: (i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM model_tariffs WHERE deployed_model_id=$1), (SELECT count(*) FROM model_cache_tariffs WHERE deployed_model_id=$1)")
        .bind(model).fetch_one(&pool).await.unwrap();
    assert_eq!(prices, (4, 4));
    apply(&target, &pool).await.unwrap();
}

#[sqlx::test(migrations = false)]
async fn tariff_indexes_accept_prebuilt_and_recover_interrupted_builds(pool: PgPool) {
    let target = Target::main();
    target.run_to(161, &pool).await.unwrap();
    for build in index_builds() {
        sqlx::raw_sql(&build.sql).execute(&pool).await.unwrap();
    }
    // Correct prebuilds and remnants from both early and late interrupted phases.
    sqlx::query("UPDATE pg_index SET indisvalid=false, indisready=false WHERE indexrelid='idx_model_tariffs_user_id'::regclass")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE pg_index SET indisvalid=false WHERE indexrelid='idx_model_cache_tariffs_unique_active_org'::regclass")
        .execute(&pool)
        .await
        .unwrap();
    apply(&target, &pool).await.unwrap();
    assert_valid(&pool).await;
}

#[sqlx::test(migrations = false)]
async fn tariff_indexes_reject_wrong_definitions_before_dropping_old_guards(pool: PgPool) {
    let target = Target::main();
    target.run_to(161, &pool).await.unwrap();
    let build = index_builds().into_iter().find(|m| m.sql.contains(ORG_BATCH)).unwrap();
    target.run_to(build.version - 1, &pool).await.unwrap();
    for wrong in [
        "CREATE INDEX idx_model_tariffs_unique_active_org_batch_per_sla ON model_tariffs (user_id, deployed_model_id, api_key_purpose, completion_window, COALESCE(serving_class, '')) WHERE valid_until IS NULL AND user_id IS NOT NULL AND api_key_purpose = 'batch' AND completion_window IS NOT NULL",
        "CREATE UNIQUE INDEX idx_model_tariffs_unique_active_org_batch_per_sla ON model_tariffs (deployed_model_id, user_id, api_key_purpose, completion_window, COALESCE(serving_class, '')) WHERE valid_until IS NULL AND user_id IS NOT NULL AND api_key_purpose = 'batch' AND completion_window IS NOT NULL",
        "CREATE UNIQUE INDEX idx_model_tariffs_unique_active_org_batch_per_sla ON model_tariffs (user_id, deployed_model_id, api_key_purpose, completion_window, COALESCE(serving_class, '')) WHERE valid_until IS NULL",
        "CREATE UNIQUE INDEX idx_model_tariffs_unique_active_org_batch_per_sla ON model_tariffs (user_id DESC, deployed_model_id, api_key_purpose, completion_window, COALESCE(serving_class, '')) WHERE valid_until IS NULL AND user_id IS NOT NULL AND api_key_purpose = 'batch' AND completion_window IS NOT NULL",
        "CREATE UNIQUE INDEX idx_model_tariffs_unique_active_org_batch_per_sla ON model_tariffs (user_id, deployed_model_id, api_key_purpose, completion_window, serving_class) WHERE valid_until IS NULL AND user_id IS NOT NULL AND api_key_purpose = 'batch' AND completion_window IS NOT NULL",
        "CREATE UNIQUE INDEX idx_model_tariffs_unique_active_org_batch_per_sla ON model_tariffs (user_id, deployed_model_id, api_key_purpose, completion_window, COALESCE(serving_class, '')) INCLUDE (id) WHERE valid_until IS NULL AND user_id IS NOT NULL AND api_key_purpose = 'batch' AND completion_window IS NOT NULL",
        "CREATE INDEX idx_model_tariffs_unique_active_org_batch_per_sla ON model_cache_tariffs (user_id)",
    ] {
        sqlx::raw_sql(wrong).execute(&pool).await.unwrap();
        let error = apply(&target, &pool).await.unwrap_err();
        assert!(format!("{error:#}").contains("wrong definition"), "{error:#}");
        assert!(format!("{error:#}").contains(ORG_BATCH), "{error:#}");
        let old: bool = sqlx::query_scalar("SELECT to_regclass('idx_model_tariffs_unique_active_realtime') IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(old);
        sqlx::query("DROP INDEX idx_model_tariffs_unique_active_org_batch_per_sla")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM _sqlx_migrations WHERE version >= $1")
            .bind(build.version)
            .execute(&pool)
            .await
            .unwrap();
    }
}

#[sqlx::test(migrations = false)]
async fn tariff_indexes_validation_rejects_missing_invalid_and_not_ready(pool: PgPool) {
    let target = Target::main();
    target.run_to(FIRST_INDEX + 10, &pool).await.unwrap();
    for flag in ["indisvalid", "indisready"] {
        sqlx::query(&format!(
            "UPDATE pg_index SET {flag}=false WHERE indexrelid='idx_model_tariffs_user_id'::regclass"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let error = apply(&target, &pool).await.unwrap_err();
        assert!(format!("{error:#}").contains("missing, invalid, not ready"));
        sqlx::query(&format!(
            "UPDATE pg_index SET {flag}=true WHERE indexrelid='idx_model_tariffs_user_id'::regclass"
        ))
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query("DROP INDEX idx_model_tariffs_user_id").execute(&pool).await.unwrap();
    let error = apply(&target, &pool).await.unwrap_err();
    assert!(format!("{error:#}").contains("missing, invalid, not ready"));
}
