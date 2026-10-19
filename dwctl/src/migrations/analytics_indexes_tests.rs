//! Recovery and definition checks for request-analytics indexes, via SQLx.
use super::*;

const BEFORE_REPAIR: i64 = 20261014114500;
const ADD_INDEX: i64 = 20261019142010;
const REINDEX: i64 = 20261019142020;

async fn assert_custom_id_index(pool: &PgPool) {
    let (valid, ready, unique, keys, attributes, options, key, predicate, comment): (
        bool,
        bool,
        bool,
        i16,
        i16,
        String,
        String,
        String,
        Option<String>,
    ) = sqlx::query_as(
        r#"
        SELECT
            i.indisvalid,
            i.indisready,
            i.indisunique,
            i.indnkeyatts,
            i.indnatts,
            i.indoption::text,
            pg_get_indexdef(i.indexrelid, 1, true),
            pg_get_expr(i.indpred, i.indrelid),
            obj_description(i.indexrelid, 'pg_class')
        FROM pg_index i
        WHERE i.indexrelid = 'idx_analytics_custom_id'::regclass
          AND i.indrelid = 'http_analytics'::regclass
        "#,
    )
    .fetch_one(pool)
    .await
    .unwrap();

    assert!(valid);
    assert!(ready);
    assert!(!unique);
    assert_eq!(keys, 1);
    assert_eq!(attributes, 1);
    assert_eq!(options, "0");
    assert_eq!(key, "custom_id");
    assert_eq!(predicate, "(custom_id IS NOT NULL)");
    assert!(comment.is_some());
}

#[sqlx::test(migrations = false)]
async fn custom_id_index_upgrade_restores_a_missing_index_and_recovers_an_interrupted_build(pool: PgPool) {
    let target = Target::main();
    target.run_to(BEFORE_REPAIR, &pool).await.unwrap();
    sqlx::query("DROP INDEX idx_analytics_custom_id").execute(&pool).await.unwrap();

    target.run_to(ADD_INDEX, &pool).await.unwrap();
    sqlx::query(
        "UPDATE pg_index SET indisvalid = false, indisready = false \
         WHERE indexrelid = 'idx_analytics_custom_id'::regclass",
    )
    .execute(&pool)
    .await
    .unwrap();

    apply(&target, &pool).await.unwrap();
    assert_custom_id_index(&pool).await;
}

#[sqlx::test(migrations = false)]
async fn custom_id_index_validation_rejects_a_wrong_prebuilt_definition(pool: PgPool) {
    let target = Target::main();
    target.run_to(BEFORE_REPAIR, &pool).await.unwrap();
    sqlx::query("DROP INDEX idx_analytics_custom_id").execute(&pool).await.unwrap();
    sqlx::query("CREATE INDEX idx_analytics_custom_id ON http_analytics (timestamp)")
        .execute(&pool)
        .await
        .unwrap();

    target.run_to(REINDEX, &pool).await.unwrap();
    let error = apply(&target, &pool).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("idx_analytics_custom_id is missing, invalid, not ready, or has the wrong definition"),
        "{error:#}"
    );
}
