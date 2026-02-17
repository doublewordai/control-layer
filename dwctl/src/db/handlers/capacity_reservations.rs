use chrono::{DateTime, Utc};
use sqlx::PgConnection;
use tracing::instrument;
use uuid::Uuid;

use crate::db::errors::Result;

pub struct BatchCapacityReservations<'c> {
    db: &'c mut PgConnection,
}

impl<'c> BatchCapacityReservations<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    #[instrument(skip(self, model_ids), fields(count = model_ids.len()), err)]
    pub async fn sum_active_by_model_window(&mut self, model_ids: &[Uuid], completion_window: &str) -> Result<Vec<(Uuid, i64)>> {
        if model_ids.is_empty() {
            return Ok(Vec::new());
        }

        let rows = sqlx::query!(
            r#"
            SELECT model_id,
                   COALESCE(SUM(reserved_requests), 0)::BIGINT AS reserved
            FROM batch_capacity_reservations
            WHERE model_id = ANY($1)
              AND completion_window = $2
              AND released_at IS NULL
              AND expires_at > now()
            GROUP BY model_id
            "#,
            model_ids,
            completion_window
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(rows.into_iter().map(|r| (r.model_id, r.reserved.unwrap_or(0))).collect())
    }

    #[instrument(skip(self, rows), fields(count = rows.len()), err)]
    pub async fn insert_reservations(&mut self, rows: &[(Uuid, &str, i64, DateTime<Utc>)]) -> Result<Vec<Uuid>> {
        let mut ids = Vec::new();
        for (model_id, window, count, expires_at) in rows {
            let id = sqlx::query_scalar!(
                r#"
                INSERT INTO batch_capacity_reservations
                    (model_id, completion_window, reserved_requests, expires_at)
                VALUES ($1, $2, $3, $4)
                RETURNING id
                "#,
                model_id,
                *window,
                *count,
                *expires_at
            )
            .fetch_one(&mut *self.db)
            .await?;
            ids.push(id);
        }
        Ok(ids)
    }

    #[instrument(skip(self, ids), fields(count = ids.len()), err)]
    pub async fn release_reservations(&mut self, ids: &[Uuid]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }

        sqlx::query!(
            r#"
            UPDATE batch_capacity_reservations
            SET released_at = now()
            WHERE id = ANY($1)
            "#,
            ids
        )
        .execute(&mut *self.db)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::users::Role;
    use crate::test::utils::{create_test_endpoint, create_test_model, create_test_user};
    use chrono::{Duration, Utc};
    use sqlx::PgPool;
    use std::collections::HashMap;
    use uuid::Uuid;

    async fn setup_models(pool: &PgPool) -> (Uuid, Uuid) {
        let user = create_test_user(pool, Role::StandardUser).await;
        let endpoint_id = create_test_endpoint(pool, &format!("test-{}", Uuid::new_v4()), user.id).await;

        let model_a = create_test_model(pool, "model-a", &format!("alias-a-{}", Uuid::new_v4()), endpoint_id, user.id).await;

        let model_b = create_test_model(pool, "model-b", &format!("alias-b-{}", Uuid::new_v4()), endpoint_id, user.id).await;

        (model_a, model_b)
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_insert_and_sum_active_reservations(pool: PgPool) {
        let (model_a, model_b) = setup_models(&pool).await;

        let expires_at = Utc::now() + Duration::minutes(10);

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = BatchCapacityReservations::new(&mut conn);

        let ids = repo
            .insert_reservations(&[(model_a, "24h", 10, expires_at), (model_b, "24h", 20, expires_at)])
            .await
            .unwrap();

        assert_eq!(ids.len(), 2);

        let rows = repo.sum_active_by_model_window(&[model_a, model_b], "24h").await.unwrap();

        let mut map = HashMap::new();
        for (id, sum) in rows {
            map.insert(id, sum);
        }

        assert_eq!(map.get(&model_a).copied().unwrap_or(0), 10);
        assert_eq!(map.get(&model_b).copied().unwrap_or(0), 20);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_release_reservations_excluded_from_sum(pool: PgPool) {
        let (model_a, _) = setup_models(&pool).await;

        let expires_at = Utc::now() + Duration::minutes(10);

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = BatchCapacityReservations::new(&mut conn);

        let ids = repo.insert_reservations(&[(model_a, "24h", 15, expires_at)]).await.unwrap();

        repo.release_reservations(&ids).await.unwrap();

        let rows = repo.sum_active_by_model_window(&[model_a], "24h").await.unwrap();

        let sum = rows.into_iter().find(|(id, _)| *id == model_a).map(|(_, v)| v).unwrap_or(0);

        assert_eq!(sum, 0);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_expired_reservations_excluded_from_sum(pool: PgPool) {
        let (model_a, _) = setup_models(&pool).await;

        let expires_at = Utc::now() - Duration::minutes(1);

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = BatchCapacityReservations::new(&mut conn);

        repo.insert_reservations(&[(model_a, "24h", 25, expires_at)]).await.unwrap();

        let rows = repo.sum_active_by_model_window(&[model_a], "24h").await.unwrap();

        let sum = rows.into_iter().find(|(id, _)| *id == model_a).map(|(_, v)| v).unwrap_or(0);

        assert_eq!(sum, 0);
    }
}
