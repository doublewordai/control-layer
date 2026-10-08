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

    /// Sum reservations per model and completion window.
    ///
    /// Unreleased reservations count until they expire (the TTL is the safety
    /// net for a handler that died before releasing). When `released_since` is
    /// set, reservations released at or after that instant are counted too,
    /// whatever their expiry: `reserve_capacity` takes its outstanding-work
    /// snapshot (possibly a cached one) *before* reading reservations, so a
    /// peer batch that committed after the snapshot and released its
    /// reservation since would otherwise be counted by neither. A released
    /// reservation stands for a batch that is already committed, so its TTL is
    /// irrelevant here. The worst case is a batch counted twice, which only
    /// errs towards under-acceptance. `released_since` must come from the same
    /// clock as `released_at` (this database's `now()`).
    ///
    /// The two branches are served by `idx_batch_capacity_reservations_active`
    /// (unreleased rows) and `idx_batch_capacity_reservations_released`
    /// (recently released rows), so the cost does not grow with the number of
    /// reservations the ledger has accumulated.
    #[instrument(skip(self, model_ids), fields(count = model_ids.len()), err)]
    pub async fn sum_by_model_and_window(
        &mut self,
        model_ids: &[Uuid],
        released_since: Option<DateTime<Utc>>,
    ) -> Result<Vec<(Uuid, String, i64)>> {
        if model_ids.is_empty() {
            return Ok(Vec::new());
        }

        let rows = sqlx::query!(
            r#"
            SELECT model_id AS "model_id!",
                   completion_window AS "completion_window!",
                   COALESCE(SUM(reserved_requests), 0)::BIGINT AS "reserved!"
            FROM (
                SELECT model_id, completion_window, reserved_requests
                FROM batch_capacity_reservations
                WHERE model_id = ANY($1)
                  AND released_at IS NULL
                  AND expires_at > now()
                UNION ALL
                SELECT model_id, completion_window, reserved_requests
                FROM batch_capacity_reservations
                WHERE $2::timestamptz IS NOT NULL
                  AND model_id = ANY($1)
                  AND released_at IS NOT NULL
                  AND released_at >= $2::timestamptz
            ) r
            GROUP BY model_id, completion_window
            "#,
            model_ids,
            released_since
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(rows.into_iter().map(|r| (r.model_id, r.completion_window, r.reserved)).collect())
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

    #[dwctl_test_macros::test]
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

        let rows = repo.sum_by_model_and_window(&[model_a, model_b], None).await.unwrap();

        let mut map = HashMap::new();
        for (id, window, sum) in rows {
            assert_eq!(window, "24h");
            map.insert(id, sum);
        }

        assert_eq!(map.get(&model_a).copied().unwrap_or(0), 10);
        assert_eq!(map.get(&model_b).copied().unwrap_or(0), 20);
    }

    #[dwctl_test_macros::test]
    #[test_log::test]
    async fn test_release_reservations_excluded_from_sum(pool: PgPool) {
        let (model_a, _) = setup_models(&pool).await;

        let expires_at = Utc::now() + Duration::minutes(10);

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = BatchCapacityReservations::new(&mut conn);

        let ids = repo.insert_reservations(&[(model_a, "24h", 15, expires_at)]).await.unwrap();

        repo.release_reservations(&ids).await.unwrap();

        let rows = repo.sum_by_model_and_window(&[model_a], None).await.unwrap();

        let sum = sum_for(rows, model_a, "24h");

        assert_eq!(sum, 0);
    }

    #[dwctl_test_macros::test]
    #[test_log::test]
    async fn test_expired_reservations_excluded_from_sum(pool: PgPool) {
        let (model_a, _) = setup_models(&pool).await;

        let expires_at = Utc::now() - Duration::minutes(1);

        let mut conn = pool.acquire().await.unwrap();
        let mut repo = BatchCapacityReservations::new(&mut conn);

        repo.insert_reservations(&[(model_a, "24h", 25, expires_at)]).await.unwrap();

        let rows = repo.sum_by_model_and_window(&[model_a], None).await.unwrap();

        let sum = sum_for(rows, model_a, "24h");

        assert_eq!(sum, 0);
    }

    #[dwctl_test_macros::test]
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

        let with_since = BatchCapacityReservations::new(&mut conn)
            .sum_by_model_and_window(&[model_a], Some(since))
            .await
            .unwrap();
        assert_eq!(sum_for(with_since, model_a, "24h"), 27);

        let active_only = BatchCapacityReservations::new(&mut conn)
            .sum_by_model_and_window(&[model_a], None)
            .await
            .unwrap();
        assert_eq!(sum_for(active_only, model_a, "24h"), 7);
    }

    #[dwctl_test_macros::test]
    #[test_log::test]
    async fn test_released_since_ignores_ttl_and_sums_per_window(pool: PgPool) {
        let (model_a, _) = setup_models(&pool).await;

        let mut conn = pool.acquire().await.unwrap();
        let since: chrono::DateTime<Utc> = sqlx::query_scalar!(r#"SELECT now() AS "now!""#)
            .fetch_one(&mut *conn)
            .await
            .unwrap();

        // A reservation whose TTL lapsed before it was released still stands
        // for a batch that committed after `since`, so it must be counted.
        let lapsed = BatchCapacityReservations::new(&mut conn)
            .insert_reservations(&[(model_a, "24h", 11, Utc::now() - Duration::minutes(1))])
            .await
            .unwrap();
        BatchCapacityReservations::new(&mut conn)
            .release_reservations(&lapsed)
            .await
            .unwrap();

        // Active reservations in two windows are reported separately.
        BatchCapacityReservations::new(&mut conn)
            .insert_reservations(&[
                (model_a, "1h", 3, Utc::now() + Duration::minutes(10)),
                (model_a, "24h", 5, Utc::now() + Duration::minutes(10)),
            ])
            .await
            .unwrap();

        let rows = BatchCapacityReservations::new(&mut conn)
            .sum_by_model_and_window(&[model_a], Some(since))
            .await
            .unwrap();
        assert_eq!(sum_for(rows.clone(), model_a, "24h"), 16);
        assert_eq!(sum_for(rows, model_a, "1h"), 3);

        // Without `since`, the lapsed-then-released reservation is not counted.
        let rows = BatchCapacityReservations::new(&mut conn)
            .sum_by_model_and_window(&[model_a], None)
            .await
            .unwrap();
        assert_eq!(sum_for(rows, model_a, "24h"), 5);
    }

    fn sum_for(rows: Vec<(Uuid, String, i64)>, model: Uuid, window: &str) -> i64 {
        rows.into_iter()
            .filter(|(id, w, _)| *id == model && w == window)
            .map(|(_, _, v)| v)
            .sum()
    }
}
