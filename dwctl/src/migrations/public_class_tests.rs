//! Recovery and definition checks for public-class tariff indexes, via SQLx.
use uuid::Uuid;

use super::*;

const INDEXES: [(&str, i64); 4] = [
    ("idx_model_tariffs_public_class_active", 20261001150110),
    ("idx_model_cache_tariffs_public_class_active", 20261001150210),
    ("idx_model_cache_tariffs_public_class_version", 20261001150310),
    ("idx_model_tariffs_public_nonbatch_class_active", 20261001153610),
];

#[sqlx::test]
async fn public_class_nonbatch_uniqueness_ignores_completion_window(pool: PgPool) {
    let model: Uuid = sqlx::query_scalar(
        "INSERT INTO deployed_models (alias,model_name,is_composite,created_by)
         VALUES ('example/unique','example/unique',true,'00000000-0000-0000-0000-000000000000') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    for class in [None, Some("standard"), Some("fast")] {
        for purpose in ["realtime", "playground", "platform", "continuation"] {
            sqlx::query("INSERT INTO model_tariffs (deployed_model_id,name,serving_class,api_key_purpose,input_price_per_token,output_price_per_token) VALUES ($1,'first',$2,$3,1,1)")
                .bind(model).bind(class).bind(purpose).execute(&pool).await.unwrap();
            let error = sqlx::query("INSERT INTO model_tariffs (deployed_model_id,name,serving_class,api_key_purpose,completion_window,input_price_per_token,output_price_per_token) VALUES ($1,'duplicate',$2,$3,'24h',2,2)")
                .bind(model).bind(class).bind(purpose).execute(&pool).await.unwrap_err();
            assert_eq!(error.as_database_error().unwrap().code().as_deref(), Some("23505"));
        }
        for window in ["1h", "24h"] {
            sqlx::query("INSERT INTO model_tariffs (deployed_model_id,name,serving_class,api_key_purpose,completion_window,input_price_per_token,output_price_per_token) VALUES ($1,'batch',$2,'batch',$3,1,1)")
                .bind(model).bind(class).bind(window).execute(&pool).await.unwrap();
        }
    }
}

#[sqlx::test(migrations = false)]
async fn public_class_indexes_recover_invalid_prebuilds(pool: PgPool) {
    let target = Target::main();
    target.run_to(20261001150000, &pool).await.unwrap();
    for (name, version) in INDEXES {
        let migration = target.migrator.iter().find(|m| m.version == version).unwrap();
        sqlx::raw_sql(&migration.sql).execute(&pool).await.unwrap();
        sqlx::query("UPDATE pg_index SET indisvalid=false,indisready=false WHERE indexrelid=to_regclass($1)")
            .bind(name)
            .execute(&pool)
            .await
            .unwrap();
    }
    apply(&target, &pool).await.unwrap();
    for (name, _) in INDEXES {
        let valid: bool = sqlx::query_scalar(
            "SELECT indisvalid AND indisready AND indisunique AND indnullsnotdistinct FROM pg_index WHERE indexrelid=to_regclass($1)",
        )
        .bind(name)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(valid);
    }
}

#[sqlx::test(migrations = false)]
async fn public_class_indexes_reject_wrong_prebuilt_definitions(pool: PgPool) {
    let target = Target::main();
    for (name, version) in INDEXES {
        target.run_to(version - 1, &pool).await.unwrap();
        let migration = target.migrator.iter().find(|m| m.version == version).unwrap();
        for wrong in [
            migration.sql.replace(" NULLS NOT DISTINCT", ""),
            migration.sql.replace("UNIQUE INDEX", "INDEX").replace(" NULLS NOT DISTINCT", ""),
            migration.sql.replace("user_id IS NULL", "user_id IS NOT NULL"),
        ] {
            sqlx::raw_sql(&wrong).execute(&pool).await.unwrap();
            let error = target.run_to(version + 20, &pool).await.unwrap_err();
            assert!(format!("{error:#}").contains("wrong definition"));
            // Failed validation does not record its migration. Build/reindex were
            // recorded, so subsequent attempts validate the replacement directly.
            sqlx::raw_sql(&format!("DROP INDEX {name}")).execute(&pool).await.unwrap();
        }
        sqlx::raw_sql(&migration.sql).execute(&pool).await.unwrap();
        target.run_to(version + 20, &pool).await.unwrap();
    }
    apply(&target, &pool).await.unwrap();
}
