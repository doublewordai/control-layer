use sqlx::PgPool;

pub async fn reconcile_auto_probes(pool: &PgPool) -> Result<u64, anyhow::Error> {
    let result = sqlx::query(
        r#"
        INSERT INTO probes (name, deployment_id)
        SELECT 'auto-' || alias, id
        FROM deployed_models
        WHERE deleted = FALSE
          AND is_composite = FALSE
          AND hosted_on IS NOT NULL
        ON CONFLICT DO NOTHING
        "#,
    )
    .execute(pool)
    .await?;

    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    async fn create_endpoint(pool: &PgPool) -> Uuid {
        sqlx::query_scalar!(
            "INSERT INTO inference_endpoints (name, url, created_by) VALUES ($1, $2, $3) RETURNING id",
            format!("endpoint-{}", Uuid::new_v4()),
            "http://localhost:8080",
            Uuid::nil()
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn create_leaf_model(pool: &PgPool, alias: &str, endpoint_id: Uuid) -> Uuid {
        sqlx::query_scalar!(
            "INSERT INTO deployed_models (model_name, alias, type, hosted_on, created_by) VALUES ($1, $2, $3, $4, $5) RETURNING id",
            alias,
            alias,
            "chat" as _,
            endpoint_id,
            Uuid::nil()
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn create_composite_model(pool: &PgPool, alias: &str) -> Uuid {
        sqlx::query_scalar!(
            "INSERT INTO deployed_models (model_name, alias, type, is_composite, created_by) VALUES ($1, $2, $3, TRUE, $4) RETURNING id",
            alias,
            alias,
            "chat" as _,
            Uuid::nil()
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn probe_names(pool: &PgPool) -> Vec<String> {
        sqlx::query_scalar!("SELECT name FROM probes ORDER BY name")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn reconcile_is_idempotent_and_skips_non_leaf_entries(pool: PgPool) {
        let endpoint_id = create_endpoint(&pool).await;
        create_leaf_model(&pool, "leaf-a", endpoint_id).await;
        create_composite_model(&pool, "composite-a").await;
        let deleted_id = create_leaf_model(&pool, "leaf-deleted", endpoint_id).await;
        sqlx::query!("UPDATE deployed_models SET deleted = TRUE WHERE id = $1", deleted_id)
            .execute(&pool)
            .await
            .unwrap();

        let created = reconcile_auto_probes(&pool).await.unwrap();
        assert_eq!(created, 1);
        assert_eq!(probe_names(&pool).await, vec!["auto-leaf-a"]);

        let created_again = reconcile_auto_probes(&pool).await.unwrap();
        assert_eq!(created_again, 0);
        assert_eq!(probe_names(&pool).await, vec!["auto-leaf-a"]);

        let probe = sqlx::query!("SELECT interval_seconds, active, http_method, request_path, request_body FROM probes")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(probe.interval_seconds, 60);
        assert!(probe.active);
        assert_eq!(probe.http_method, "POST");
        assert!(probe.request_path.is_none());
        assert!(probe.request_body.is_none());
    }

    #[sqlx::test]
    async fn reconcile_never_clobbers_manual_probes(pool: PgPool) {
        let endpoint_id = create_endpoint(&pool).await;
        let leaf_id = create_leaf_model(&pool, "leaf-manual", endpoint_id).await;
        sqlx::query!(
            "INSERT INTO probes (name, deployment_id, interval_seconds) VALUES ($1, $2, 30)",
            "my-manual-probe",
            leaf_id
        )
        .execute(&pool)
        .await
        .unwrap();

        let created = reconcile_auto_probes(&pool).await.unwrap();
        assert_eq!(created, 0);

        let probe = sqlx::query!("SELECT name, interval_seconds FROM probes")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(probe.name, "my-manual-probe");
        assert_eq!(probe.interval_seconds, 30);
    }
}
