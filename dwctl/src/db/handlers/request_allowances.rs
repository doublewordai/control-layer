//! Lifetime request allowances for hidden dashboard execution keys.

use crate::{
    db::errors::Result,
    types::{ApiKeyId, UserId},
};
use sqlx::{PgConnection, Row};

/// Outcome of attempting to reserve hidden-key trial capacity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowanceReservation {
    /// The owning account has a positive credit balance, so no trial capacity
    /// was consumed and normal billing should handle the request.
    CreditsAvailable,
    /// Trial capacity was reserved atomically.
    Reserved { remaining: i64 },
    /// This key was never provisioned with a request allowance.
    NotProvisioned,
    /// The account has no sufficient trial capacity.
    Unavailable,
}

pub struct RequestAllowances<'c> {
    db: &'c mut PgConnection,
}

impl<'c> RequestAllowances<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    /// Attach independently configured allowances to a new account's shared
    /// hidden playground and batch keys. Existing accounts are never backfilled
    /// because this is called only inside account-creation transactions.
    pub async fn provision(&mut self, user_id: UserId, playground_requests: i64, batch_requests: i64) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO api_key_request_allowances (api_key_id, initial_requests, remaining_requests)
            SELECT
                ak.id,
                CASE ak.purpose
                    WHEN 'playground' THEN $2
                    WHEN 'batch' THEN $3
                END,
                CASE ak.purpose
                    WHEN 'playground' THEN $2
                    WHEN 'batch' THEN $3
                END
            FROM api_keys ak
            WHERE ak.user_id = $1
              AND ak.created_by = $1
              AND ak.hidden = true
              AND ak.parent_api_key_id IS NULL
              AND ak.is_deleted = false
              AND (
                    (ak.purpose = 'playground' AND $2 > 0)
                 OR (ak.purpose = 'batch' AND $3 > 0)
              )
            ON CONFLICT (api_key_id) DO NOTHING
            "#,
        )
        .bind(user_id)
        .bind(playground_requests)
        .bind(batch_requests)
        .execute(&mut *self.db)
        .await?;

        Ok(())
    }

    /// Check whether a hidden key has any trial capacity without reserving it.
    /// Batch creation uses this as an early payment preflight before reading or
    /// validating the input file; the exact request count is still reserved
    /// atomically only after the file has been accepted.
    pub async fn has_remaining(&mut self, api_key_id: ApiKeyId) -> Result<bool> {
        Ok(sqlx::query_scalar(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM api_key_request_allowances
                WHERE api_key_id = $1
                  AND remaining_requests > 0
            )
            "#,
        )
        .bind(api_key_id)
        .fetch_one(&mut *self.db)
        .await?)
    }

    /// Atomically reserve `requested` requests when the owning account is at
    /// non-positive credits. Positive balances bypass the trial without a
    /// decrement; insufficient allowances fail closed. Allowing a negative
    /// balance is necessary because accepted trial usage may be charged before
    /// the next request is admitted.
    pub async fn reserve(&mut self, api_key_id: ApiKeyId, requested: i64) -> Result<AllowanceReservation> {
        if requested <= 0 {
            return Ok(AllowanceReservation::Unavailable);
        }

        let row = sqlx::query(
            r#"
            WITH key_state AS (
                SELECT ub.balance
                FROM api_keys ak
                JOIN user_balance_checkpoints ub ON ub.user_id = ak.user_id
                WHERE ak.id = $1
                  AND ak.hidden = true
                  AND ak.purpose IN ('playground', 'batch')
                  AND ak.is_deleted = false
            ),
            reserved AS (
                UPDATE api_key_request_allowances allowance
                SET remaining_requests = allowance.remaining_requests - $2
                FROM key_state
                WHERE allowance.api_key_id = $1
                  AND key_state.balance <= 0
                  AND allowance.remaining_requests >= $2
                RETURNING allowance.remaining_requests
            )
            SELECT
                CASE
                    WHEN EXISTS (SELECT 1 FROM key_state WHERE balance > 0) THEN 'credits'
                    WHEN EXISTS (SELECT 1 FROM reserved) THEN 'reserved'
                    WHEN NOT EXISTS (
                        SELECT 1 FROM api_key_request_allowances WHERE api_key_id = $1
                    ) THEN 'not_provisioned'
                    ELSE 'unavailable'
                END AS outcome,
                (SELECT remaining_requests FROM reserved) AS remaining
            "#,
        )
        .bind(api_key_id)
        .bind(requested)
        .fetch_one(&mut *self.db)
        .await?;

        let outcome: &str = row.try_get("outcome")?;
        let remaining: Option<i64> = row.try_get("remaining")?;
        Ok(match outcome {
            "credits" => AllowanceReservation::CreditsAvailable,
            "reserved" => AllowanceReservation::Reserved {
                remaining: remaining.expect("reserved allowance must return its remaining count"),
            },
            "not_provisioned" => AllowanceReservation::NotProvisioned,
            _ => AllowanceReservation::Unavailable,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sqlx::PgPool;
    use tokio::sync::Barrier;
    use uuid::Uuid;

    use super::{AllowanceReservation, RequestAllowances};

    async fn create_user_and_hidden_keys(pool: &PgPool) -> (Uuid, Uuid, Uuid) {
        let user_id = Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, username, email, auth_source) VALUES ($1, $2, $3, 'native')")
            .bind(user_id)
            .bind(format!("allowance-{user_id}"))
            .bind(format!("allowance-{user_id}@example.com"))
            .execute(pool)
            .await
            .unwrap();

        let playground_id = Uuid::new_v4();
        let batch_id = Uuid::new_v4();
        for (id, purpose) in [(playground_id, "playground"), (batch_id, "batch")] {
            sqlx::query(
                "INSERT INTO api_keys (id, name, secret, purpose, user_id, created_by, hidden) \
                 VALUES ($1, $2, $3, $4, $5, $5, true)",
            )
            .bind(id)
            .bind(format!("Internal {purpose} Key"))
            .bind(format!("sk-{id}"))
            .bind(purpose)
            .bind(user_id)
            .execute(pool)
            .await
            .unwrap();
        }

        (user_id, playground_id, batch_id)
    }

    async fn remaining(pool: &PgPool, key_id: Uuid) -> Option<i64> {
        sqlx::query_scalar("SELECT remaining_requests FROM api_key_request_allowances WHERE api_key_id = $1")
            .bind(key_id)
            .fetch_optional(pool)
            .await
            .unwrap()
    }

    #[sqlx::test]
    async fn request_allowances_provision_independent_hidden_key_budgets(pool: PgPool) {
        let (user_id, playground_id, batch_id) = create_user_and_hidden_keys(&pool).await;
        let mut conn = pool.acquire().await.unwrap();

        RequestAllowances::new(&mut conn).provision(user_id, 3, 7).await.unwrap();

        assert_eq!(remaining(&pool, playground_id).await, Some(3));
        assert_eq!(remaining(&pool, batch_id).await, Some(7));
    }

    #[sqlx::test]
    async fn request_allowances_reject_oversized_reservation_without_partial_decrement(pool: PgPool) {
        let (user_id, playground_id, _) = create_user_and_hidden_keys(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        RequestAllowances::new(&mut conn).provision(user_id, 2, 0).await.unwrap();

        let result = RequestAllowances::new(&mut conn).reserve(playground_id, 3).await.unwrap();

        assert_eq!(result, AllowanceReservation::Unavailable);
        assert_eq!(remaining(&pool, playground_id).await, Some(2));
    }

    #[sqlx::test]
    async fn request_allowances_preflight_does_not_consume_capacity(pool: PgPool) {
        let (user_id, playground_id, _) = create_user_and_hidden_keys(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        RequestAllowances::new(&mut conn).provision(user_id, 1, 0).await.unwrap();

        assert!(RequestAllowances::new(&mut conn).has_remaining(playground_id).await.unwrap());
        assert_eq!(remaining(&pool, playground_id).await, Some(1));

        RequestAllowances::new(&mut conn).reserve(playground_id, 1).await.unwrap();

        assert!(!RequestAllowances::new(&mut conn).has_remaining(playground_id).await.unwrap());
    }

    #[sqlx::test]
    async fn request_allowances_positive_balance_bypasses_without_consuming(pool: PgPool) {
        let (user_id, playground_id, _) = create_user_and_hidden_keys(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        RequestAllowances::new(&mut conn).provision(user_id, 2, 0).await.unwrap();
        sqlx::query("UPDATE user_balance_checkpoints SET balance = 1 WHERE user_id = $1")
            .bind(user_id)
            .execute(&pool)
            .await
            .unwrap();

        let result = RequestAllowances::new(&mut conn).reserve(playground_id, 1).await.unwrap();

        assert_eq!(result, AllowanceReservation::CreditsAvailable);
        assert_eq!(remaining(&pool, playground_id).await, Some(2));
    }

    #[sqlx::test]
    async fn request_allowances_continue_after_trial_usage_makes_balance_negative(pool: PgPool) {
        let (user_id, playground_id, _) = create_user_and_hidden_keys(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        RequestAllowances::new(&mut conn).provision(user_id, 2, 0).await.unwrap();
        sqlx::query("UPDATE user_balance_checkpoints SET balance = -1 WHERE user_id = $1")
            .bind(user_id)
            .execute(&pool)
            .await
            .unwrap();

        let result = RequestAllowances::new(&mut conn).reserve(playground_id, 1).await.unwrap();

        assert_eq!(result, AllowanceReservation::Reserved { remaining: 1 });
        assert_eq!(remaining(&pool, playground_id).await, Some(1));
    }

    #[sqlx::test]
    async fn request_allowances_concurrent_reservations_cannot_overspend(pool: PgPool) {
        let (user_id, playground_id, _) = create_user_and_hidden_keys(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        RequestAllowances::new(&mut conn).provision(user_id, 4, 0).await.unwrap();
        drop(conn);

        let barrier = Arc::new(Barrier::new(9));
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let pool = pool.clone();
            let barrier = Arc::clone(&barrier);
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                let mut conn = pool.acquire().await.unwrap();
                RequestAllowances::new(&mut conn).reserve(playground_id, 1).await.unwrap()
            }));
        }
        barrier.wait().await;

        let mut reserved = 0;
        for task in tasks {
            if matches!(task.await.unwrap(), AllowanceReservation::Reserved { .. }) {
                reserved += 1;
            }
        }

        assert_eq!(reserved, 4);
        assert_eq!(remaining(&pool, playground_id).await, Some(0));
    }
}
