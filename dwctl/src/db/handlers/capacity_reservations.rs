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

    /// Sum unexpired reservations per model for one completion window.
    ///
    /// Active (unreleased) reservations are always counted. When
    /// `released_since` is set, reservations released at or after that instant
    /// are counted too: `reserve_capacity` snapshots committed pending rows
    /// *before* taking the admission lock, so a peer batch that committed its
    /// rows after the snapshot and released its reservation before this read
    /// would otherwise be counted by neither. Including recently released
    /// reservations closes that gap; the worst case is a batch counted twice,
    /// which only errs towards under-acceptance. `released_since` must come
    /// from the same clock as `released_at` (this database's `now()`).
    #[instrument(skip(self, model_ids), fields(count = model_ids.len()), err)]
    pub async fn sum_active_by_model_window(
        &mut self,
        model_ids: &[Uuid],
        completion_window: &str,
        released_since: Option<DateTime<Utc>>,
    ) -> Result<Vec<(Uuid, i64)>> {
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
              AND (released_at IS NULL OR released_at >= $3)
              AND expires_at > now()
            GROUP BY model_id
            "#,
            model_ids,
            completion_window,
            released_since
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(rows.into_iter().map(|r| (r.model_id, r.reserved.unwrap_or(0))).collect())
    }

    #[instrument(skip(self, rows), fields(count = rows.len()), err)]
    pub async fn insert_reservations(&mut self, rows: &[(Uuid, &str, i64, DateTime<Utc>)]) -> Result<Vec<Uuid>> {
        if rows.is_empty() {
            return Ok(vec![]);
        }

        let model_ids: Vec<Uuid> = rows.iter().map(|(id, _, _, _)| *id).collect();
        let windows: Vec<&str> = rows.iter().map(|(_, w, _, _)| *w).collect();
        let counts: Vec<i64> = rows.iter().map(|(_, _, c, _)| *c).collect();
        let expires_ats: Vec<DateTime<Utc>> = rows.iter().map(|(_, _, _, e)| *e).collect();

        let ids = sqlx::query_scalar!(
            r#"
            INSERT INTO batch_capacity_reservations
                (model_id, completion_window, reserved_requests, expires_at)
            SELECT * FROM UNNEST($1::uuid[], $2::text[], $3::bigint[], $4::timestamptz[])
            RETURNING id
            "#,
            &model_ids,
            &windows as &[&str],
            &counts,
            &expires_ats as &[DateTime<Utc>],
        )
        .fetch_all(&mut *self.db)
        .await?;

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

        let rows = repo.sum_active_by_model_window(&[model_a, model_b], "24h", None).await.unwrap();

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

        let rows = repo.sum_active_by_model_window(&[model_a], "24h", None).await.unwrap();

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

        let rows = repo.sum_active_by_model_window(&[model_a], "24h", None).await.unwrap();

        let sum = rows.into_iter().find(|(id, _)| *id == model_a).map(|(_, v)| v).unwrap_or(0);

        assert_eq!(sum, 0);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_reservations_released_since_are_counted(pool: PgPool) {
        let (model_a, _) = setup_models(&pool).await;

        let expires_at = Utc::now() + Duration::minutes(10);

        let mut conn = pool.acquire().await.unwrap();

        // Released before the snapshot instant: excluded.
        let old = BatchCapacityReservations::new(&mut conn)
            .insert_reservations(&[(model_a, "24h", 15, expires_at)])
            .await
            .unwrap();
        BatchCapacityReservations::new(&mut conn).release_reservations(&old).await.unwrap();

        let since: chrono::DateTime<Utc> = sqlx::query_scalar!(r#"SELECT now() AS "now!""#)
            .fetch_one(&mut *conn)
            .await
            .unwrap();

        // Released at/after the snapshot instant: still counted.
        let recent = BatchCapacityReservations::new(&mut conn)
            .insert_reservations(&[(model_a, "24h", 20, expires_at)])
            .await
            .unwrap();
        BatchCapacityReservations::new(&mut conn)
            .release_reservations(&recent)
            .await
            .unwrap();

        // Never released: always counted.
        BatchCapacityReservations::new(&mut conn)
            .insert_reservations(&[(model_a, "24h", 7, expires_at)])
            .await
            .unwrap();

        let sum_for = |rows: Vec<(Uuid, i64)>| rows.into_iter().find(|(id, _)| *id == model_a).map(|(_, v)| v).unwrap_or(0);

        let with_since = BatchCapacityReservations::new(&mut conn)
            .sum_active_by_model_window(&[model_a], "24h", Some(since))
            .await
            .unwrap();
        assert_eq!(sum_for(with_since), 27);

        let active_only = BatchCapacityReservations::new(&mut conn)
            .sum_active_by_model_window(&[model_a], "24h", None)
            .await
            .unwrap();
        assert_eq!(sum_for(active_only), 7);
    }
}
