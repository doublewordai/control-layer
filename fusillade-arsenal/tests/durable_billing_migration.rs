//! Rollout preserves legacy ownership for existing rows and leaves new rows unassigned.

#[sqlx::test(migrations = false)]
async fn durable_billing_migration_preserves_existing_legacy_rows(pool: sqlx::PgPool) {
    sqlx::raw_sql(
        "CREATE TABLE requests (id UUID, state TEXT, completed_at TIMESTAMPTZ, created_by TEXT, batch_id UUID); \
         CREATE TABLE batch_requests_archive (id UUID, state TEXT, completed_at TIMESTAMPTZ, created_by TEXT, batch_id UUID); \
         INSERT INTO requests (id,state) VALUES ('00000000-0000-0000-0000-000000000001', 'completed'); \
         INSERT INTO batch_requests_archive (id,state) VALUES ('00000000-0000-0000-0000-000000000001', 'completed');",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!(
        "../migrations/20260914000000_add_durable_billing_acceptance.up.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();
    for table in ["requests", "batch_requests_archive"] {
        let mode: String = sqlx::query_scalar(&format!(
            "SELECT billing_mode FROM {table} WHERE id = '00000000-0000-0000-0000-000000000001'"
        ))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(mode, "legacy");
        let mode: Option<String> = sqlx::query_scalar(&format!(
            "INSERT INTO {table} (id, state) VALUES ('00000000-0000-0000-0000-000000000002', 'pending') RETURNING billing_mode"
        ))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(mode, None);
        assert!(
            sqlx::query(&format!(
                "UPDATE {table} SET billing_mode = 'durable' WHERE id = '00000000-0000-0000-0000-000000000001'"
            ))
            .execute(&pool)
            .await
            .is_err(),
            "a completed row cannot become durable without acceptance"
        );
    }
}

#[sqlx::test]
async fn durable_billing_trigger_uses_request_schema_not_caller_search_path(pool: sqlx::PgPool) {
    let request_id = uuid::Uuid::new_v4();
    let event_id = uuid::Uuid::new_v4();
    let batch_id = uuid::Uuid::new_v4();
    sqlx::raw_sql("CREATE SCHEMA caller_shadow; CREATE TABLE caller_shadow.billing_acceptances (LIKE public.billing_acceptances); CREATE TABLE caller_shadow.batches (id UUID, created_by TEXT);")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO batches (id,endpoint,completion_window,created_by,total_requests,created_at,expires_at) VALUES ($1,'/test','24h','actual-owner',1,now(),now()+interval '1 day')")
        .bind(batch_id).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO caller_shadow.batches (id,created_by) VALUES ($1,'wrong-owner')")
        .bind(batch_id)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL search_path = caller_shadow, public")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO public.requests (id,batch_id,state,model,billing_mode,accepted_event_id,completed_at,created_by,response_status,response_body,claimed_at,started_at) VALUES ($1,$2,'completed','test','durable',$3,now(),NULL,200,'{}',now(),now())")
        .bind(request_id).bind(batch_id).bind(event_id).execute(&mut *tx).await.unwrap();
    let owner: String =
        sqlx::query_scalar("SELECT owner_id FROM public.billing_acceptances WHERE request_id=$1")
            .bind(request_id)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(owner, "actual-owner");
    let wrong_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM caller_shadow.billing_acceptances")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!(wrong_count, 0);
    tx.commit().await.unwrap();
}
