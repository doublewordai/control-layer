//! Database repository for credit transactions.

use crate::{
    db::{
        errors::Result,
        models::credits::{CreditTransactionCreateDBRequest, CreditTransactionDBResponse, CreditTransactionType},
    },
    types::{UserId, abbrev_uuid},
};
use chrono::{DateTime, Utc};
use rand::random;
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgConnection};
use std::collections::HashMap;
use tracing::{error, instrument, trace};
use uuid::Uuid;

/// Probability of refreshing checkpoint on each transaction (1 in N).
/// With N=1000, checkpoint lags by ~1000 transactions on average,
/// meaning balance reads aggregate ~500 rows on average.
const CHECKPOINT_REFRESH_PROBABILITY: u32 = 1000;

// Database entity model for credit transaction
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct CreditTransaction {
    pub id: Uuid,
    pub user_id: UserId,
    #[sqlx(rename = "transaction_type")]
    pub transaction_type: CreditTransactionType,
    pub amount: Decimal,
    /// Balance after this transaction. None for new transactions using checkpoint-based calculation.
    pub balance_after: Option<Decimal>,
    pub previous_transaction_id: Option<Uuid>,
    pub description: Option<String>,
    pub source_id: String,
    pub created_at: DateTime<Utc>,
    /// Sequence number for reliable ordering in checkpoint calculations.
    pub seq: i64,
}

impl From<CreditTransaction> for CreditTransactionDBResponse {
    fn from(tx: CreditTransaction) -> Self {
        Self {
            id: tx.id,
            user_id: tx.user_id,
            transaction_type: tx.transaction_type,
            amount: tx.amount,
            balance_after: tx.balance_after,
            previous_transaction_id: tx.previous_transaction_id,
            description: tx.description,
            source_id: tx.source_id,
            created_at: tx.created_at,
        }
    }
}

/// Checkpoint data for a user's balance
#[derive(Debug, Clone)]
pub struct BalanceCheckpoint {
    pub user_id: UserId,
    pub checkpoint_seq: i64,
    pub balance: Decimal,
}

/// Result of aggregating batch transactions
#[derive(Debug)]
pub struct AggregatedBatches {
    /// Aggregated transactions with their associated batch IDs
    pub batched_transactions: Vec<(CreditTransactionDBResponse, Uuid)>,
    /// All source_ids that belong to batches (for filtering)
    pub batched_source_ids: Vec<String>,
}

pub struct Credits<'c> {
    db: &'c mut PgConnection,
}

impl<'c> Credits<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    /// Create a new credit transaction
    ///
    /// This is a lock-free append-only INSERT. Balance is calculated on read via checkpoints.
    /// Probabilistically refreshes the checkpoint (1 in CHECKPOINT_REFRESH_PROBABILITY chance).
    #[instrument(skip(self, request), fields(user_id = %abbrev_uuid(&request.user_id), transaction_type = ?request.transaction_type, amount = %request.amount), err)]
    pub async fn create_transaction(&mut self, request: &CreditTransactionCreateDBRequest) -> Result<CreditTransactionDBResponse> {
        // Lock-free INSERT - no advisory lock, no balance calculation
        // balance_after is NULL for new transactions; balance is calculated on read via checkpoints
        let transaction = sqlx::query_as!(
            CreditTransaction,
            r#"
            INSERT INTO credits_transactions (user_id, transaction_type, amount, source_id, description)
            VALUES ($1, $2, $3, $4, $5)
            RETURNING id, user_id, transaction_type as "transaction_type: CreditTransactionType", amount, source_id,
                      balance_after, previous_transaction_id, description, created_at, seq
            "#,
            request.user_id,
            &request.transaction_type as &CreditTransactionType,
            request.amount,
            request.source_id,
            request.description
        )
        .fetch_one(&mut *self.db)
        .await?;

        trace!("Created transaction {} for user_id {}", transaction.id, request.user_id);

        // Probabilistically refresh checkpoint (1 in N chance)
        // This amortizes checkpoint maintenance across writes
        if random::<u32>().is_multiple_of(CHECKPOINT_REFRESH_PROBABILITY) {
            trace!("Refreshing checkpoint for user_id {}", request.user_id);
            if let Err(e) = self.refresh_checkpoint(request.user_id).await {
                // Log but don't fail the transaction - checkpoint refresh is best-effort
                error!("Failed to refresh checkpoint for user_id {}: {}", request.user_id, e);
            }
        }

        Ok(CreditTransactionDBResponse::from(transaction))
    }

    /// Calculate balance using checkpoint + delta, returning both balance and latest transaction seq.
    ///
    /// This is the core calculation used by both `get_user_balance` and `refresh_checkpoint`.
    /// Returns (balance, latest_seq). If no transactions exist, returns (0, None).
    async fn calculate_balance_with_seq(&mut self, user_id: UserId) -> Result<(Decimal, Option<i64>)> {
        let result = sqlx::query!(
            r#"
            SELECT
                COALESCE(c.balance, 0) + COALESCE(SUM(
                    CASE WHEN t.transaction_type IN ('admin_grant', 'purchase') THEN t.amount ELSE -t.amount END
                ), 0) as "balance!",
                MAX(t.seq) as latest_seq
            FROM user_balance_checkpoints c
            FULL OUTER JOIN credits_transactions t
                ON t.user_id = c.user_id
                AND t.seq > c.checkpoint_seq
            WHERE c.user_id = $1 OR t.user_id = $1
            GROUP BY c.balance
            "#,
            user_id
        )
        .fetch_optional(&mut *self.db)
        .await?;

        match result {
            Some(row) => Ok((row.balance, row.latest_seq)),
            None => Ok((Decimal::ZERO, None)),
        }
    }

    /// Refresh the balance checkpoint for a user.
    ///
    /// This is called probabilistically during writes to keep checkpoints fresh.
    /// Uses the existing checkpoint as a base (if present) - only aggregates delta transactions.
    #[instrument(skip(self), fields(user_id = %abbrev_uuid(&user_id)), err)]
    pub async fn refresh_checkpoint(&mut self, user_id: UserId) -> Result<()> {
        let (balance, latest_seq) = self.calculate_balance_with_seq(user_id).await?;

        // Only update checkpoint if there are transactions
        if let Some(checkpoint_seq) = latest_seq {
            sqlx::query!(
                r#"
                INSERT INTO user_balance_checkpoints (user_id, checkpoint_seq, balance)
                VALUES ($1, $2, $3)
                ON CONFLICT (user_id) DO UPDATE SET
                    checkpoint_seq = EXCLUDED.checkpoint_seq,
                    balance = EXCLUDED.balance,
                    updated_at = NOW()
                "#,
                user_id,
                checkpoint_seq,
                balance
            )
            .execute(&mut *self.db)
            .await?;
        }

        Ok(())
    }

    /// Get current balance for a user using checkpoint + delta calculation.
    ///
    /// This reads the cached checkpoint balance and adds any transactions since the checkpoint.
    /// If no checkpoint exists, it falls back to aggregating all transactions.
    #[instrument(skip(self), fields(user_id = %abbrev_uuid(&user_id)), err)]
    pub async fn get_user_balance(&mut self, user_id: UserId) -> Result<Decimal> {
        let (balance, _) = self.calculate_balance_with_seq(user_id).await?;
        Ok(balance)
    }

    /// Get balances for multiple users using checkpoint + delta calculation.
    #[instrument(skip(self, user_ids), fields(count = user_ids.len()), err)]
    pub async fn get_users_balances_bulk(&mut self, user_ids: &[UserId]) -> Result<HashMap<UserId, f64>> {
        let rows = sqlx::query!(
            r#"
            SELECT
                COALESCE(c.user_id, t.user_id) as "user_id!",
                COALESCE(c.balance, 0) + COALESCE(SUM(
                    CASE WHEN t.transaction_type IN ('admin_grant', 'purchase') THEN t.amount ELSE -t.amount END
                ), 0) as "balance!"
            FROM user_balance_checkpoints c
            FULL OUTER JOIN credits_transactions t
                ON t.user_id = c.user_id
                AND t.seq > c.checkpoint_seq
            WHERE c.user_id = ANY($1) OR t.user_id = ANY($1)
            GROUP BY c.user_id, t.user_id, c.balance
            "#,
            user_ids
        )
        .fetch_all(&mut *self.db)
        .await?;

        let mut balances_map = HashMap::new();
        for row in rows {
            balances_map.insert(
                row.user_id,
                row.balance.to_f64().unwrap_or_else(|| {
                    error!("Failed to convert balance to f64 for user_id {}", row.user_id);
                    0.0
                }),
            );
        }

        Ok(balances_map)
    }

    /// List transactions for a specific user with pagination
    #[instrument(skip(self), fields(user_id = %abbrev_uuid(&user_id), skip = skip, limit = limit), err)]
    pub async fn list_user_transactions(&mut self, user_id: UserId, skip: i64, limit: i64) -> Result<Vec<CreditTransactionDBResponse>> {
        let transactions = sqlx::query_as!(
            CreditTransaction,
            r#"
            SELECT id, user_id, transaction_type as "transaction_type: CreditTransactionType", amount, balance_after, source_id, previous_transaction_id, description, created_at, seq
            FROM credits_transactions
            WHERE user_id = $1
            ORDER BY seq DESC
            OFFSET $2
            LIMIT $3
            "#,
            user_id,
            skip,
            limit
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(transactions.into_iter().map(CreditTransactionDBResponse::from).collect())
    }

    /// List all transactions across all users (admin view)
    #[instrument(skip(self), fields(skip = skip, limit = limit), err)]
    pub async fn list_all_transactions(&mut self, skip: i64, limit: i64) -> Result<Vec<CreditTransactionDBResponse>> {
        let transactions = sqlx::query_as!(
            CreditTransaction,
            r#"
            SELECT id, user_id, transaction_type as "transaction_type: CreditTransactionType", amount, balance_after, source_id, previous_transaction_id, description, created_at, seq
            FROM credits_transactions
            ORDER BY seq DESC
            OFFSET $1
            LIMIT $2
            "#,
            skip,
            limit
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(transactions.into_iter().map(CreditTransactionDBResponse::from).collect())
    }

    /// Get a single transaction by its ID
    #[instrument(skip(self), err)]
    pub async fn get_transaction_by_id(&mut self, transaction_id: Uuid) -> Result<Option<CreditTransactionDBResponse>> {
        let transaction = sqlx::query_as!(
            CreditTransaction,
            r#"
            SELECT id, user_id, transaction_type as "transaction_type: CreditTransactionType",
                amount, balance_after, previous_transaction_id, source_id, description, created_at, seq
            FROM credits_transactions
            WHERE id = $1
            "#,
            transaction_id
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(transaction.map(CreditTransactionDBResponse::from))
    }

    /// Count total transactions for a specific user
    #[instrument(skip(self), fields(user_id = %abbrev_uuid(&user_id)), err)]
    pub async fn count_user_transactions(&mut self, user_id: UserId) -> Result<i64> {
        let result = sqlx::query!("SELECT COUNT(*) as count FROM credits_transactions WHERE user_id = $1", user_id)
            .fetch_one(&mut *self.db)
            .await?;

        Ok(result.count.unwrap_or(0))
    }

    /// Count total transactions across all users
    #[instrument(skip(self), err)]
    pub async fn count_all_transactions(&mut self) -> Result<i64> {
        let result = sqlx::query!("SELECT COUNT(*) as count FROM credits_transactions")
            .fetch_one(&mut *self.db)
            .await?;

        Ok(result.count.unwrap_or(0))
    }

    /// Count transactions with batch grouping applied
    /// Returns the count of aggregated results (batches count as 1, not N)
    #[instrument(skip(self), fields(user_id = ?user_id.map(|id| abbrev_uuid(&id))), err)]
    pub async fn count_transactions_with_batches(&mut self, user_id: Option<UserId>) -> Result<i64> {
        let result = sqlx::query!(
            r#"
            SELECT COUNT(*) as count FROM (
                -- Batched transactions (grouped by batch_id)
                SELECT ha.fusillade_batch_id as id
                FROM credits_transactions ct
                JOIN http_analytics ha
                    ON ct.source_id = ha.id::text
                    AND ct.transaction_type = 'usage'
                WHERE ($1::uuid IS NULL OR ct.user_id = $1)
                    AND ha.fusillade_batch_id IS NOT NULL
                GROUP BY ha.fusillade_batch_id, ct.user_id

                UNION ALL

                -- Non-batched transactions
                SELECT ct.id
                FROM credits_transactions ct
                LEFT JOIN http_analytics ha ON ct.source_id = ha.id::text AND ct.transaction_type = 'usage'
                WHERE ($1::uuid IS NULL OR ct.user_id = $1)
                    AND (ct.transaction_type != 'usage' OR ha.fusillade_batch_id IS NULL)
            ) combined
            "#,
            user_id
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(result.count.unwrap_or(0))
    }

    /// Sum the signed amounts of the most recent N transactions for a user.
    /// Positive transactions (admin_grant, purchase) are positive, negative (usage, admin_removal) are negative.
    /// This is used to calculate the balance at a specific point in the transaction history.
    #[instrument(skip(self), fields(user_id = %abbrev_uuid(&user_id), count = count), err)]
    pub async fn sum_recent_transactions(&mut self, user_id: UserId, count: i64) -> Result<Decimal> {
        let result = sqlx::query!(
            r#"
            SELECT COALESCE(SUM(
                CASE WHEN transaction_type IN ('admin_grant', 'purchase') THEN amount ELSE -amount END
            ), 0) as "sum!"
            FROM (
                SELECT transaction_type, amount
                FROM credits_transactions
                WHERE user_id = $1
                ORDER BY seq DESC
                LIMIT $2
            ) recent
            "#,
            user_id,
            count
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(result.sum)
    }

    /// List transactions with batch grouping applied (all in SQL)
    /// Returns paginated results with batches already aggregated
    /// Pass None for user_id to get all transactions (admin view)
    #[instrument(skip(self), fields(user_id = ?user_id.map(|id| abbrev_uuid(&id)), skip = skip, limit = limit), err)]
    pub async fn list_transactions_with_batches(
        &mut self,
        user_id: Option<UserId>,
        skip: i64,
        limit: i64,
    ) -> Result<Vec<(CreditTransactionDBResponse, Option<Uuid>)>> {
        let rows = sqlx::query!(
            r#"
            SELECT * FROM (
                -- Part 1: Aggregated batch transactions
                -- Groups multiple batch requests into single rows (e.g., 100 requests → 1 row)
                SELECT
                    (array_agg(ct.id ORDER BY ct.seq))[1] as id,  -- Pick first transaction ID
                    ct.user_id,                          -- User who owns these transactions
                    'usage' as "transaction_type!: CreditTransactionType",
                    SUM(ct.amount) as amount,            -- Total cost of all requests in batch
                    MAX(ct.balance_after) as balance_after,  -- Final balance (after last request)
                    MIN(ct.source_id) as source_id,      -- Pick first source_id
                    (array_agg(ct.previous_transaction_id ORDER BY ct.seq))[1] as previous_transaction_id,
                    CASE
                        WHEN MIN(ha.model) IS NOT NULL THEN 'Batch - ' || MIN(ha.model)
                        ELSE 'Batch'
                    END as description,                  -- "Batch - gpt-4" or just "Batch"
                    MAX(ct.created_at) as created_at,    -- Most recent transaction time in batch
                    MAX(ct.seq) as max_seq,              -- Highest seq in batch (for ordering)
                    ha.fusillade_batch_id as batch_id    -- The batch ID
                FROM credits_transactions ct
                -- Filter by transaction_type before joining to reduce rows
                JOIN http_analytics ha
                    ON ct.source_id = ha.id::text
                    AND ct.transaction_type = 'usage'
                WHERE ($1::uuid IS NULL OR ct.user_id = $1)  -- Optional user filter (NULL = all users)
                    AND ha.fusillade_batch_id IS NOT NULL     -- Only requests with batch_id
                -- NOTE: fusillade_batch_id is user-specific; a batch never spans multiple users.
                -- We therefore group by both batch_id and user_id to get one row per (user, batch).
                GROUP BY ha.fusillade_batch_id, ct.user_id

                UNION ALL

                -- Part 2: Non-batched transactions
                -- Individual transactions: admin grants, purchases, removals, and non-batched usage
                SELECT
                    ct.id,
                    ct.user_id,
                    ct.transaction_type as "transaction_type!: CreditTransactionType",
                    ct.amount,
                    ct.balance_after,
                    ct.source_id,
                    ct.previous_transaction_id,
                    ct.description,
                    ct.created_at,
                    ct.seq as max_seq,                   -- Individual transaction seq
                    NULL::uuid as batch_id              -- Not a batch, so NULL
                FROM credits_transactions ct
                -- LEFT JOIN so non-usage transactions (grants, purchases) still appear
                LEFT JOIN http_analytics ha ON ct.source_id = ha.id::text AND ct.transaction_type = 'usage'
                WHERE ($1::uuid IS NULL OR ct.user_id = $1)  -- Optional user filter (NULL = all users)
                    AND (ct.transaction_type != 'usage'       -- All non-usage (grants, purchases, removals)
                         OR ha.fusillade_batch_id IS NULL)    -- OR usage without batch_id
            ) combined
            -- Sort by seq (most recent first), then paginate
            -- Pagination happens AFTER aggregation for correct results
            ORDER BY max_seq DESC
            LIMIT $2 OFFSET $3
            "#,
            user_id,
            limit,
            skip
        )
        .fetch_all(&mut *self.db)
        .await?;

        let mut results = Vec::new();
        for row in rows {
            // Aggregated fields are nullable, but should always have values for valid batches
            // Unwrap with helpful error messages
            let id = row
                .id
                .ok_or_else(|| sqlx::Error::Protocol("Batch aggregation returned NULL id".to_string()))?;
            let user_id = row
                .user_id
                .ok_or_else(|| sqlx::Error::Protocol("Batch aggregation returned NULL user_id".to_string()))?;
            let amount = row
                .amount
                .ok_or_else(|| sqlx::Error::Protocol("Batch aggregation returned NULL amount".to_string()))?;
            // balance_after is no longer stored for new transactions (checkpoint-based system)
            // It will be None for all new transactions
            let source_id = row
                .source_id
                .ok_or_else(|| sqlx::Error::Protocol("Batch aggregation returned NULL source_id".to_string()))?;
            let created_at = row
                .created_at
                .ok_or_else(|| sqlx::Error::Protocol("Batch aggregation returned NULL created_at".to_string()))?;

            let transaction = CreditTransactionDBResponse {
                id,
                user_id,
                transaction_type: row.transaction_type,
                amount,
                balance_after: row.balance_after, // Now optional
                previous_transaction_id: row.previous_transaction_id,
                description: row.description,
                source_id,
                created_at,
            };
            results.push((transaction, row.batch_id));
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::users::Role;
    use rust_decimal::Decimal;
    use sqlx::PgPool;
    use std::str::FromStr;
    use uuid::Uuid;

    async fn create_test_user(pool: &PgPool) -> UserId {
        let user_id = Uuid::new_v4();
        sqlx::query!(
            "INSERT INTO users (id, username, email, is_admin, auth_source) VALUES ($1, $2, $3, false, 'test')",
            user_id,
            format!("testuser_{}", user_id.simple()),
            format!("test_{}@example.com", user_id.simple())
        )
        .execute(pool)
        .await
        .expect("Failed to create test user");

        // Add StandardUser role
        let role = Role::StandardUser;
        sqlx::query!("INSERT INTO user_roles (user_id, role) VALUES ($1, $2)", user_id, role as Role)
            .execute(pool)
            .await
            .expect("Failed to add user role");

        user_id
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_user_balance_zero_for_new_user(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        let balance = credits.get_user_balance(user_id).await.expect("Failed to get balance");
        assert_eq!(balance, Decimal::ZERO);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_transaction_admin_grant(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        let request = CreditTransactionCreateDBRequest::admin_grant(
            user_id,
            user_id,
            Decimal::from_str("100.50").unwrap(),
            Some("Test grant".to_string()),
        );

        let transaction = credits.create_transaction(&request).await.expect("Failed to create transaction");

        assert_eq!(transaction.user_id, user_id);
        assert_eq!(transaction.transaction_type, CreditTransactionType::AdminGrant);
        assert_eq!(transaction.amount, Decimal::from_str("100.50").unwrap());
        assert_eq!(transaction.description, Some("Test grant".to_string()));

        // Verify balance via get_user_balance (balance_after is no longer stored for new transactions)
        let balance = credits.get_user_balance(user_id).await.expect("Failed to get balance");
        assert_eq!(balance, Decimal::from_str("100.50").unwrap());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_user_balance_after_transactions(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Add credits
        let request1 = CreditTransactionCreateDBRequest::admin_grant(user_id, user_id, Decimal::from_str("100.0").unwrap(), None);
        credits.create_transaction(&request1).await.expect("Failed to create transaction");

        let balance = credits.get_user_balance(user_id).await.expect("Failed to get balance");
        assert_eq!(balance, Decimal::from_str("100.0").unwrap());

        // Add more credits
        let request2 = CreditTransactionCreateDBRequest::admin_grant(user_id, user_id, Decimal::from_str("50.0").unwrap(), None);
        credits.create_transaction(&request2).await.expect("Failed to create transaction");

        let balance = credits.get_user_balance(user_id).await.expect("Failed to get balance");
        assert_eq!(balance, Decimal::from_str("150.0").unwrap());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_user_balance_after_transactions_negative_balance(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Add credits
        let request1 = CreditTransactionCreateDBRequest::admin_grant(user_id, user_id, Decimal::from_str("100.0").unwrap(), None);
        credits.create_transaction(&request1).await.expect("Failed to create transaction");

        let balance = credits.get_user_balance(user_id).await.expect("Failed to get balance");
        assert_eq!(balance, Decimal::from_str("100.0").unwrap());

        // Add more credits
        let request2 = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminRemoval,
            amount: Decimal::from_str("500.0").unwrap(),
            source_id: Uuid::new_v4().to_string(),
            description: None,
        };
        credits.create_transaction(&request2).await.expect("Failed to create transaction");

        let balance = credits.get_user_balance(user_id).await.expect("Failed to get balance");
        assert_eq!(balance, Decimal::from_str("-400.0").unwrap());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_transaction_balance_after_multiple_transactions(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Create first transaction
        let request1 = CreditTransactionCreateDBRequest::admin_grant(user_id, user_id, Decimal::from_str("100.50").unwrap(), None);
        let transaction1 = credits
            .create_transaction(&request1)
            .await
            .expect("Failed to create first transaction");

        assert_eq!(transaction1.user_id, user_id);
        assert_eq!(transaction1.transaction_type, CreditTransactionType::AdminGrant);
        assert_eq!(transaction1.amount, Decimal::from_str("100.50").unwrap());
        assert_eq!(transaction1.description, None);

        // Verify balance after first transaction
        let balance = credits.get_user_balance(user_id).await.expect("Failed to get balance");
        assert_eq!(balance, Decimal::from_str("100.50").unwrap());

        // Create second transaction
        let request2 = CreditTransactionCreateDBRequest::admin_grant(user_id, user_id, Decimal::from_str("50.0").unwrap(), None);

        let transaction2 = credits
            .create_transaction(&request2)
            .await
            .expect("Failed to create second transaction");

        assert_eq!(transaction2.user_id, user_id);
        assert_eq!(transaction2.transaction_type, CreditTransactionType::AdminGrant);
        assert_eq!(transaction2.amount, Decimal::from_str("50.0").unwrap());
        assert_eq!(transaction2.description, None);

        // Verify balance after second transaction
        let balance = credits.get_user_balance(user_id).await.expect("Failed to get balance");
        assert_eq!(balance, Decimal::from_str("150.50").unwrap());

        // Create third transaction that deducts credits
        let request3 = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminRemoval,
            amount: Decimal::from_str("30.0").unwrap(),
            source_id: Uuid::new_v4().to_string(),
            description: Some("Usage deduction".to_string()),
        };

        let transaction3 = credits
            .create_transaction(&request3)
            .await
            .expect("Failed to create third transaction");

        assert_eq!(transaction3.user_id, user_id);
        assert_eq!(transaction3.transaction_type, CreditTransactionType::AdminRemoval);
        assert_eq!(transaction3.amount, Decimal::from_str("30.0").unwrap());
        assert_eq!(transaction3.description, Some("Usage deduction".to_string()));

        // Verify final balance
        let balance = credits.get_user_balance(user_id).await.expect("Failed to get balance");
        assert_eq!(balance, Decimal::from_str("120.50").unwrap());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_user_transactions_ordering(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);
        let n_of_transactions = 10;

        for i in 1..n_of_transactions + 1 {
            let request = CreditTransactionCreateDBRequest::admin_grant(
                user_id,
                user_id,
                Decimal::from(i * 10),
                Some(format!("Transaction {}", i + 1)),
            );
            credits.create_transaction(&request).await.expect("Failed to create transaction");
            // Small delay to ensure unique timestamps in source_id
        }

        let transactions = credits
            .list_user_transactions(user_id, 0, n_of_transactions)
            .await
            .expect("Failed to list transactions");

        // Should be ordered by seq DESC (most recent first)
        // Since seq is monotonically increasing, this effectively orders by creation time
        assert_eq!(transactions.len(), n_of_transactions as usize);
        for i in 0..(transactions.len() - 1) {
            let t1 = &transactions[i];
            let t2 = &transactions[i + 1];
            // Higher seq means more recent, so created_at should be >= as well
            assert!(t1.created_at >= t2.created_at, "Transactions are not ordered correctly");
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_user_transaction(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);
        let n_of_transactions = 10;
        let mut transaction_ids = Vec::new();

        for i in 1..n_of_transactions + 1 {
            let request = CreditTransactionCreateDBRequest::admin_grant(
                user_id,
                user_id,
                Decimal::from(i * 10),
                Some(format!("Transaction {}", i + 1)),
            );
            transaction_ids.push(credits.create_transaction(&request).await.expect("Failed to create transaction").id);
        }

        for i in 1..n_of_transactions + 1 {
            match credits
                .get_transaction_by_id(transaction_ids[i - 1])
                .await
                .expect("Failed to get transaction by ID {transaction_id}")
            {
                Some(tx) => {
                    assert_eq!(tx.id, transaction_ids[i - 1]);
                    assert_eq!(tx.user_id, user_id);
                    assert_eq!(tx.transaction_type, CreditTransactionType::AdminGrant);
                    assert_eq!(tx.amount, Decimal::from(i * 10));
                    assert_eq!(tx.description, Some(format!("Transaction {}", i + 1)));
                    // balance_after is no longer stored for new transactions
                    assert!(tx.balance_after.is_none());
                }
                None => panic!("Transaction ID {} not found", transaction_ids[i - 1]),
            };
        }

        // Verify total balance via get_user_balance
        let total_balance = credits.get_user_balance(user_id).await.expect("Failed to get balance");
        // Sum of 10 + 20 + ... + 100 = 550
        assert_eq!(total_balance, Decimal::from(550));

        // Assert non existent transaction ID returns None
        assert!(
            credits
                .get_transaction_by_id(Uuid::new_v4())
                .await
                .expect("Failed to get transaction by ID 99999999999")
                .is_none()
        )
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_user_transactions_pagination(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Create 5 transactions with cumulative balances
        let mut cumulative_balance = Decimal::ZERO;
        for i in 1..=5 {
            let amount = Decimal::from(i * 10);
            cumulative_balance += amount;
            let request = CreditTransactionCreateDBRequest::admin_grant(user_id, user_id, amount, None);
            credits.create_transaction(&request).await.expect("Failed to create transaction");
        }

        // Test limit
        let transactions = credits
            .list_user_transactions(user_id, 0, 2)
            .await
            .expect("Failed to list transactions");
        assert_eq!(transactions.len(), 2);

        // Test skip
        let transactions = credits
            .list_user_transactions(user_id, 2, 2)
            .await
            .expect("Failed to list transactions");
        assert_eq!(transactions.len(), 2);

        // Test skip beyond available
        let transactions = credits
            .list_user_transactions(user_id, 10, 2)
            .await
            .expect("Failed to list transactions");
        assert_eq!(transactions.len(), 0);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_user_transactions_filters_by_user(pool: PgPool) {
        let user1_id = create_test_user(&pool).await;
        let user2_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Create transactions for user1
        let request1 = CreditTransactionCreateDBRequest::admin_grant(user1_id, user1_id, Decimal::from_str("100.0").unwrap(), None);
        credits.create_transaction(&request1).await.expect("Failed to create transaction");

        // Create transactions for user2
        let request2 = CreditTransactionCreateDBRequest::admin_grant(user2_id, user2_id, Decimal::from_str("200.0").unwrap(), None);
        credits.create_transaction(&request2).await.expect("Failed to create transaction");

        // List user1's transactions
        let transactions = credits
            .list_user_transactions(user1_id, 0, 10)
            .await
            .expect("Failed to list transactions");
        assert_eq!(transactions.len(), 1);
        assert_eq!(transactions[0].user_id, user1_id);
        // Verify balance via get_user_balance
        let balance = credits.get_user_balance(user1_id).await.expect("Failed to get balance");
        assert_eq!(balance, Decimal::from_str("100.0").unwrap());

        // List user2's transactions
        let transactions = credits
            .list_user_transactions(user2_id, 0, 10)
            .await
            .expect("Failed to list transactions");
        assert_eq!(transactions.len(), 1);
        assert_eq!(transactions[0].user_id, user2_id);
        // Verify balance via get_user_balance
        let balance = credits.get_user_balance(user2_id).await.expect("Failed to get balance");
        assert_eq!(balance, Decimal::from_str("200.0").unwrap());

        // List non existent user's transactions
        let non_existent_user_id = Uuid::new_v4();
        let transactions = credits
            .list_user_transactions(non_existent_user_id, 0, 10)
            .await
            .expect("Failed to list transactions");
        assert_eq!(transactions.len(), 0);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_all_transactions(pool: PgPool) {
        let user1_id = create_test_user(&pool).await;
        let user2_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Create transactions for both users
        let request1 = CreditTransactionCreateDBRequest::admin_grant(
            user1_id,
            user1_id,
            Decimal::from_str("100.0").unwrap(),
            Some("User 1 grant".to_string()),
        );
        credits.create_transaction(&request1).await.expect("Failed to create transaction");

        let request2 = CreditTransactionCreateDBRequest::admin_grant(
            user2_id,
            user2_id,
            Decimal::from_str("200.0").unwrap(),
            Some("User 2 grant".to_string()),
        );
        credits.create_transaction(&request2).await.expect("Failed to create transaction");

        let transactions = credits.list_all_transactions(0, 10).await.expect("Failed to list transactions");

        // Should have at least our 2 transactions
        assert!(transactions.len() >= 2);

        // Verify both users' transactions are present
        assert!(transactions.iter().any(|t| t.user_id == user1_id));
        assert!(transactions.iter().any(|t| t.user_id == user2_id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_all_transactions_pagination(pool: PgPool) {
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Create 10 transactions
        let mut cumulative_balance = Decimal::ZERO;
        for i in 1..10 {
            let amount = Decimal::from(i * 10);
            cumulative_balance += amount;
            let user_id = create_test_user(&pool).await;
            let request = CreditTransactionCreateDBRequest::admin_grant(user_id, user_id, amount, None);
            credits.create_transaction(&request).await.expect("Failed to create transaction");
        }

        // Test limit
        let transactions = credits.list_all_transactions(0, 2).await.expect("Failed to list transactions");
        assert_eq!(transactions.len(), 2);

        // Test skip
        let transactions = credits.list_all_transactions(2, 2).await.expect("Failed to list transactions");
        assert!(transactions.len() >= 2);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_transaction_with_all_transaction_types(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Test AdminGrant
        let request =
            CreditTransactionCreateDBRequest::admin_grant(user_id, user_id, Decimal::from_str("100.0").unwrap(), Some("Grant".to_string()));
        let tx = credits.create_transaction(&request).await.expect("Failed to create AdminGrant");
        assert_eq!(tx.transaction_type, CreditTransactionType::AdminGrant);

        // Test Purchase
        let request = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::Purchase,
            amount: Decimal::from_str("50.0").unwrap(),
            source_id: Uuid::new_v4().to_string(), // Mimics Stripe payment ID
            description: Some("Purchase".to_string()),
        };
        let tx = credits.create_transaction(&request).await.expect("Failed to create Purchase");
        assert_eq!(tx.transaction_type, CreditTransactionType::Purchase);

        // Test Usage
        let request = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::Usage,
            amount: Decimal::from_str("25.0").unwrap(),
            source_id: Uuid::new_v4().to_string(), // Mimics request ID from http_analytics
            description: Some("Usage".to_string()),
        };
        let tx = credits.create_transaction(&request).await.expect("Failed to create Usage");
        assert_eq!(tx.transaction_type, CreditTransactionType::Usage);

        // Test AdminRemoval
        let request = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminRemoval,
            amount: Decimal::from_str("25.0").unwrap(),
            source_id: Uuid::new_v4().to_string(),
            description: Some("Removal".to_string()),
        };
        let tx = credits.create_transaction(&request).await.expect("Failed to create AdminRemoval");
        assert_eq!(tx.transaction_type, CreditTransactionType::AdminRemoval);

        // Verify final balance
        let balance = credits.get_user_balance(user_id).await.expect("Failed to get balance");
        assert_eq!(balance, Decimal::from_str("100.0").unwrap());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_transaction_rollback_on_error(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Create a valid transaction
        let request1 = CreditTransactionCreateDBRequest::admin_grant(user_id, user_id, Decimal::from_str("100.0").unwrap(), None);
        credits.create_transaction(&request1).await.expect("Failed to create transaction");

        // Try to create an invalid transaction (insufficient balance for removal)
        let request2 = CreditTransactionCreateDBRequest::admin_grant(
            user_id,
            user_id,
            Decimal::from_str("-200.0").unwrap(), // Invalid negative amount
            None,
        );
        let result = credits.create_transaction(&request2).await;
        assert!(result.is_err());

        // Verify the balance hasn't changed (transaction was rolled back)
        let balance = credits.get_user_balance(user_id).await.expect("Failed to get balance");
        assert_eq!(balance, Decimal::from_str("100.0").unwrap());

        // Verify only one transaction exists
        let transactions = credits
            .list_user_transactions(user_id, 0, 10)
            .await
            .expect("Failed to list transactions");
        assert_eq!(transactions.len(), 1);
    }

    /// This test is to check the performance of creating transactions under concurrent load. If one thread
    /// reads the balance while another is writing, it could lead to incorrect balances as the first one that
    /// is committed wins and the second calculated its balance based on stale data.
    #[sqlx::test]
    #[test_log::test]
    async fn test_balance_threshold_notification_triggers(pool: PgPool) {
        use sqlx::postgres::PgListener;
        use std::time::Duration;
        use tokio::time::timeout;

        let user_id = create_test_user(&pool).await;

        // Setup a listener for auth_config_changed notifications
        let mut listener = PgListener::connect_with(&pool).await.expect("Failed to create listener");
        listener
            .listen("auth_config_changed")
            .await
            .expect("Failed to listen to auth_config_changed");

        // Test 1: Going from 0 to positive (SHOULD trigger - crossing zero threshold)
        // This enables API keys when user gets their first credits
        {
            let mut conn = pool.acquire().await.expect("Failed to acquire connection");
            let mut credits = Credits::new(&mut conn);

            let request = CreditTransactionCreateDBRequest::admin_grant(
                user_id,
                user_id,
                Decimal::from_str("100.0").unwrap(),
                Some("Initial grant".to_string()),
            );
            credits.create_transaction(&request).await.expect("Failed to create transaction");
        }

        // Should receive notification
        let notification = timeout(Duration::from_secs(2), listener.recv())
            .await
            .expect("Timeout waiting for notification")
            .expect("Failed to receive notification");

        assert_eq!(notification.channel(), "auth_config_changed");

        let payload: serde_json::Value = serde_json::from_str(notification.payload()).expect("Failed to parse notification payload");

        assert_eq!(payload["user_id"], user_id.to_string());
        assert_eq!(payload["threshold_crossed"], "zero");
        assert_eq!(payload["old_balance"].as_f64().unwrap(), 0.0);
        assert_eq!(payload["new_balance"].as_f64().unwrap(), 100.0);

        // Test 2: Going from positive to negative (crossing zero threshold downward - SHOULD trigger)
        {
            let mut conn = pool.acquire().await.expect("Failed to acquire connection");
            let mut credits = Credits::new(&mut conn);

            let request = CreditTransactionCreateDBRequest {
                user_id,
                transaction_type: CreditTransactionType::Usage,
                amount: Decimal::from_str("150.0").unwrap(),
                source_id: Uuid::new_v4().to_string(), // Mimics request ID from http_analytics
                description: Some("Usage that crosses zero".to_string()),
            };
            credits.create_transaction(&request).await.expect("Failed to create transaction");
        }

        // Should receive notification
        let notification = timeout(Duration::from_secs(2), listener.recv())
            .await
            .expect("Timeout waiting for notification")
            .expect("Failed to receive notification");

        assert_eq!(notification.channel(), "auth_config_changed");

        // Parse the JSON payload
        let payload: serde_json::Value = serde_json::from_str(notification.payload()).expect("Failed to parse notification payload");

        assert_eq!(payload["user_id"], user_id.to_string());
        assert_eq!(payload["threshold_crossed"], "zero");
        assert_eq!(payload["old_balance"].as_f64().unwrap(), 100.0);
        assert_eq!(payload["new_balance"].as_f64().unwrap(), -50.0);

        // Test 3: Going from negative to positive (crossing zero threshold upward - SHOULD trigger)
        {
            let mut conn = pool.acquire().await.expect("Failed to acquire connection");
            let mut credits = Credits::new(&mut conn);

            let request = CreditTransactionCreateDBRequest::admin_grant(
                user_id,
                user_id,
                Decimal::from_str("100.0").unwrap(),
                Some("Grant that crosses zero".to_string()),
            );
            credits.create_transaction(&request).await.expect("Failed to create transaction");
        }

        // Should receive notification
        let notification = timeout(Duration::from_secs(2), listener.recv())
            .await
            .expect("Timeout waiting for notification")
            .expect("Failed to receive notification");

        assert_eq!(notification.channel(), "auth_config_changed");

        let payload: serde_json::Value = serde_json::from_str(notification.payload()).expect("Failed to parse notification payload");

        assert_eq!(payload["user_id"], user_id.to_string());
        assert_eq!(payload["threshold_crossed"], "zero");
        assert_eq!(payload["old_balance"].as_f64().unwrap(), -50.0);
        assert_eq!(payload["new_balance"].as_f64().unwrap(), 50.0);

        // Test 4: Staying in positive range (should NOT trigger)
        {
            let mut conn = pool.acquire().await.expect("Failed to acquire connection");
            let mut credits = Credits::new(&mut conn);

            let request = CreditTransactionCreateDBRequest::admin_grant(
                user_id,
                user_id,
                Decimal::from_str("50.0").unwrap(),
                Some("Another grant".to_string()),
            );
            credits.create_transaction(&request).await.expect("Failed to create transaction");
        }

        // Try to receive notification with short timeout - should NOT receive one
        let result = timeout(Duration::from_millis(500), listener.recv()).await;
        assert!(result.is_err(), "Should NOT receive notification when staying in positive range");
    }

    /// Test that concurrent transactions correctly update the balance.
    /// With the checkpoint-based system, we verify that:
    /// 1. All concurrent transactions are created successfully
    /// 2. The final balance is correct after all transactions complete
    #[sqlx::test]
    #[test_log::test]
    async fn test_concurrent_transactions_balance_correctness(pool: PgPool) {
        use std::sync::Arc;
        use tokio::task;

        let user_id = create_test_user(&pool).await;

        // Create initial balance
        let mut conn: sqlx::pool::PoolConnection<sqlx::Postgres> = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);
        let initial_request = CreditTransactionCreateDBRequest::admin_grant(
            user_id,
            user_id,
            Decimal::from_str("1000.0").unwrap(),
            Some("Initial balance".to_string()),
        );
        credits
            .create_transaction(&initial_request)
            .await
            .expect("Failed to create initial transaction");
        drop(conn);

        // Spawn 100 concurrent transactions
        // 50 grants of 10.0 each = +500.0
        // 50 removals of 5.0 each = -250.0
        // Net change = +250.0
        let pool = Arc::new(pool);
        let mut handles = vec![];

        for i in 0..100 {
            let pool_clone = Arc::clone(&pool);
            let handle = task::spawn(async move {
                let mut conn = pool_clone.acquire().await.expect("Failed to acquire connection");
                let mut credits = Credits::new(&mut conn);

                let request = CreditTransactionCreateDBRequest {
                    user_id,
                    transaction_type: if i % 2 == 0 {
                        CreditTransactionType::AdminGrant
                    } else {
                        CreditTransactionType::AdminRemoval
                    },
                    amount: if i % 2 == 0 {
                        Decimal::from_str("10.0").unwrap()
                    } else {
                        Decimal::from_str("5.0").unwrap()
                    },
                    source_id: Uuid::new_v4().to_string(),
                    description: Some(format!("Concurrent transaction {}", i)),
                };

                credits.create_transaction(&request).await.expect("Failed to create transaction")
            });
            handles.push(handle);
        }

        // Wait for all transactions to complete
        for handle in handles {
            handle.await.expect("Task panicked");
        }

        // Verify we have exactly 101 transactions (1 initial + 100 concurrent)
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);
        let transactions = credits
            .list_user_transactions(user_id, 0, 1000)
            .await
            .expect("Failed to list transactions");

        assert_eq!(transactions.len(), 101, "Should have 101 transactions");

        // Verify final balance: 1000 + 500 - 250 = 1250
        let final_balance = credits.get_user_balance(user_id).await.expect("Failed to get balance");
        assert_eq!(
            final_balance,
            Decimal::from_str("1250.0").unwrap(),
            "Expected 1250.0 but got {}",
            final_balance
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_transaction_large_amounts(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Test with large credit amount
        let large_amount = Decimal::from_str("100000000.00").unwrap(); // 100 million
        let request = CreditTransactionCreateDBRequest::admin_grant(user_id, user_id, large_amount, Some("Large credit grant".to_string()));

        let transaction = credits
            .create_transaction(&request)
            .await
            .expect("Failed to create large transaction");

        assert_eq!(transaction.user_id, user_id);
        assert_eq!(transaction.amount, large_amount);

        // Verify balance after first transaction
        let balance = credits.get_user_balance(user_id).await.expect("Failed to get balance");
        assert_eq!(balance, large_amount);

        // Add another large amount
        let request2 =
            CreditTransactionCreateDBRequest::admin_grant(user_id, user_id, large_amount, Some("Second large grant".to_string()));

        credits
            .create_transaction(&request2)
            .await
            .expect("Failed to create second large transaction");

        // Verify final balance
        let balance = credits.get_user_balance(user_id).await.expect("Failed to get balance");
        assert_eq!(balance, Decimal::from_str("200000000.00").unwrap());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_transaction_preserves_high_precision(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Test with high precision amount (e.g., per-token micro-transaction)
        let request = CreditTransactionCreateDBRequest::admin_grant(
            user_id,
            user_id,
            Decimal::from_str("100.12345678").unwrap(),
            Some("High precision grant".to_string()),
        );

        let transaction = credits.create_transaction(&request).await.expect("Failed to create transaction");

        // Amount should preserve all decimal places (no rounding)
        assert_eq!(transaction.amount, Decimal::from_str("100.12345678").unwrap());

        // Verify balance preserves precision
        let balance = credits.get_user_balance(user_id).await.expect("Failed to get balance");
        assert_eq!(balance, Decimal::from_str("100.12345678").unwrap());

        // Test micro-transaction precision (like per-token costs)
        let micro_request = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::Usage,
            amount: Decimal::from_str("0.000000405").unwrap(), // ~1 input + 1 output token cost
            source_id: "micro-txn".to_string(),
            description: Some("Micro-transaction".to_string()),
        };

        let micro_transaction = credits
            .create_transaction(&micro_request)
            .await
            .expect("Failed to create micro-transaction");

        // Micro-transaction should preserve full precision
        assert_eq!(micro_transaction.amount, Decimal::from_str("0.000000405").unwrap());

        // Verify balance after micro-transaction: 100.12345678 - 0.000000405 = 100.123456375
        let balance = credits.get_user_balance(user_id).await.expect("Failed to get balance");
        assert_eq!(balance, Decimal::from_str("100.123456375").unwrap());
    }
}
