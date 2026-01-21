//! Database repository for credit transactions.

use crate::{
    api::models::transactions::TransactionFilters,
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

/// Extended transaction data with category information for display
#[derive(Debug, Clone)]
pub struct TransactionWithCategory {
    pub transaction: CreditTransactionDBResponse,
    pub batch_id: Option<Uuid>,
    pub request_origin: Option<String>,
    pub batch_sla: Option<String>,
    /// Number of requests in this batch (1 for non-batch transactions)
    pub batch_count: i32,
}

/// Convert CreditTransactionType to its snake_case string representation for SQL queries
fn transaction_type_to_string(t: &CreditTransactionType) -> String {
    match t {
        CreditTransactionType::Purchase => "purchase".to_string(),
        CreditTransactionType::AdminGrant => "admin_grant".to_string(),
        CreditTransactionType::AdminRemoval => "admin_removal".to_string(),
        CreditTransactionType::Usage => "usage".to_string(),
    }
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
        // Balance is calculated on read via checkpoints
        let transaction = sqlx::query_as!(
            CreditTransaction,
            r#"
            INSERT INTO credits_transactions (user_id, transaction_type, amount, source_id, description, fusillade_batch_id)
            VALUES ($1, $2, $3, $4, $5, $6)
            RETURNING id, user_id, transaction_type as "transaction_type: CreditTransactionType", amount, source_id,
                      description, created_at, seq
            "#,
            request.user_id,
            &request.transaction_type as &CreditTransactionType,
            request.amount,
            request.source_id,
            request.description,
            request.fusillade_batch_id
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
            WITH user_checkpoint AS (
                SELECT checkpoint_seq, balance
                FROM user_balance_checkpoints
                WHERE user_id = $1
            )
            SELECT
                COALESCE((SELECT balance FROM user_checkpoint), 0) +
                COALESCE((
                    SELECT SUM(
                        CASE WHEN transaction_type IN ('admin_grant', 'purchase') THEN amount ELSE -amount END
                    )
                    FROM credits_transactions
                    WHERE user_id = $1
                    AND seq > COALESCE((SELECT checkpoint_seq FROM user_checkpoint), 0)
                ), 0) as "balance!",
                (SELECT MAX(seq) FROM credits_transactions WHERE user_id = $1) as latest_seq
            "#,
            user_id
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok((result.balance, result.latest_seq))
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
                u.user_id as "user_id!",
                COALESCE(c.balance, 0) + COALESCE(delta.sum, 0) as "balance!"
            FROM unnest($1::uuid[]) AS u(user_id)
            LEFT JOIN user_balance_checkpoints c ON c.user_id = u.user_id
            LEFT JOIN LATERAL (
                SELECT SUM(
                    CASE WHEN transaction_type IN ('admin_grant', 'purchase') THEN amount ELSE -amount END
                ) as sum
                FROM credits_transactions t
                WHERE t.user_id = u.user_id
                AND t.seq > COALESCE(c.checkpoint_seq, 0)
            ) delta ON true
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

    /// List transactions for a specific user with pagination and optional filters
    #[instrument(skip(self, filters), fields(user_id = %abbrev_uuid(&user_id), skip = skip, limit = limit), err)]
    pub async fn list_user_transactions(
        &mut self,
        user_id: UserId,
        skip: i64,
        limit: i64,
        filters: &TransactionFilters,
    ) -> Result<Vec<CreditTransactionDBResponse>> {
        let transaction_types: Option<Vec<String>> = filters
            .transaction_types
            .as_ref()
            .map(|types| types.iter().map(transaction_type_to_string).collect());

        let transactions = sqlx::query_as!(
            CreditTransaction,
            r#"
            SELECT id, user_id, transaction_type as "transaction_type: CreditTransactionType", amount, source_id, description, created_at, seq
            FROM credits_transactions
            WHERE user_id = $1
              AND ($4::text IS NULL OR description ILIKE '%' || $4 || '%')
              AND ($5::text[] IS NULL OR transaction_type::text = ANY($5))
              AND ($6::timestamptz IS NULL OR created_at >= $6)
              AND ($7::timestamptz IS NULL OR created_at <= $7)
            ORDER BY seq DESC
            OFFSET $2
            LIMIT $3
            "#,
            user_id,
            skip,
            limit,
            filters.search.as_deref(),
            transaction_types.as_deref(),
            filters.start_date,
            filters.end_date,
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(transactions.into_iter().map(CreditTransactionDBResponse::from).collect())
    }

    /// List all transactions across all users (admin view) with optional filters
    #[instrument(skip(self, filters), fields(skip = skip, limit = limit), err)]
    pub async fn list_all_transactions(
        &mut self,
        skip: i64,
        limit: i64,
        filters: &TransactionFilters,
    ) -> Result<Vec<CreditTransactionDBResponse>> {
        let transaction_types: Option<Vec<String>> = filters
            .transaction_types
            .as_ref()
            .map(|types| types.iter().map(transaction_type_to_string).collect());

        let transactions = sqlx::query_as!(
            CreditTransaction,
            r#"
            SELECT id, user_id, transaction_type as "transaction_type: CreditTransactionType", amount, source_id, description, created_at, seq
            FROM credits_transactions
            WHERE ($3::text IS NULL OR description ILIKE '%' || $3 || '%')
              AND ($4::text[] IS NULL OR transaction_type::text = ANY($4))
              AND ($5::timestamptz IS NULL OR created_at >= $5)
              AND ($6::timestamptz IS NULL OR created_at <= $6)
            ORDER BY seq DESC
            OFFSET $1
            LIMIT $2
            "#,
            skip,
            limit,
            filters.search.as_deref(),
            transaction_types.as_deref(),
            filters.start_date,
            filters.end_date,
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
                amount, source_id, description, created_at, seq
            FROM credits_transactions
            WHERE id = $1
            "#,
            transaction_id
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(transaction.map(CreditTransactionDBResponse::from))
    }

    /// Check if a transaction exists by source_id
    /// Used for idempotency checks (e.g., duplicate webhook deliveries)
    pub async fn transaction_exists_by_source_id(&mut self, source_id: &str) -> Result<bool> {
        let result = sqlx::query!(
            r#"
            SELECT id FROM credits_transactions
            WHERE source_id = $1
            LIMIT 1
            "#,
            source_id
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(result.is_some())
    }

    /// Count total transactions for a specific user with optional filters
    #[instrument(skip(self, filters), fields(user_id = %abbrev_uuid(&user_id)), err)]
    pub async fn count_user_transactions(&mut self, user_id: UserId, filters: &TransactionFilters) -> Result<i64> {
        let transaction_types: Option<Vec<String>> = filters
            .transaction_types
            .as_ref()
            .map(|types| types.iter().map(transaction_type_to_string).collect());

        let result = sqlx::query!(
            r#"
            SELECT COUNT(*) as count
            FROM credits_transactions
            WHERE user_id = $1
              AND ($2::text IS NULL OR description ILIKE '%' || $2 || '%')
              AND ($3::text[] IS NULL OR transaction_type::text = ANY($3))
              AND ($4::timestamptz IS NULL OR created_at >= $4)
              AND ($5::timestamptz IS NULL OR created_at <= $5)
            "#,
            user_id,
            filters.search.as_deref(),
            transaction_types.as_deref(),
            filters.start_date,
            filters.end_date,
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(result.count.unwrap_or(0))
    }

    /// Count total transactions across all users with optional filters
    #[instrument(skip(self, filters), err)]
    pub async fn count_all_transactions(&mut self, filters: &TransactionFilters) -> Result<i64> {
        let transaction_types: Option<Vec<String>> = filters
            .transaction_types
            .as_ref()
            .map(|types| types.iter().map(transaction_type_to_string).collect());

        let result = sqlx::query!(
            r#"
            SELECT COUNT(*) as count
            FROM credits_transactions
            WHERE ($1::text IS NULL OR description ILIKE '%' || $1 || '%')
              AND ($2::text[] IS NULL OR transaction_type::text = ANY($2))
              AND ($3::timestamptz IS NULL OR created_at >= $3)
              AND ($4::timestamptz IS NULL OR created_at <= $4)
            "#,
            filters.search.as_deref(),
            transaction_types.as_deref(),
            filters.start_date,
            filters.end_date,
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(result.count.unwrap_or(0))
    }

    /// Count transactions with batch grouping applied for a specific user.
    /// Returns the count of aggregated results (batches count as 1, not N).
    /// Uses pre-aggregated batch_aggregates table for O(1) batch counting.
    #[instrument(skip(self, filters), fields(user_id = %abbrev_uuid(&user_id)), err)]
    pub async fn count_transactions_with_batches(&mut self, user_id: UserId, filters: &TransactionFilters) -> Result<i64> {
        let transaction_types: Option<Vec<String>> = filters
            .transaction_types
            .as_ref()
            .map(|types| types.iter().map(transaction_type_to_string).collect());

        // Check if we should include batch aggregates (they're always type 'usage')
        let include_batches = filters
            .transaction_types
            .as_ref()
            .map(|types| types.iter().any(|t| matches!(t, CreditTransactionType::Usage)))
            .unwrap_or(true);

        // Check if search term would match "Batch" description
        let search_matches_batch = filters
            .search
            .as_ref()
            .map(|s| "batch".contains(&s.to_lowercase()) || s.to_lowercase().contains("batch"))
            .unwrap_or(true);

        let result = sqlx::query!(
            r#"
            SELECT
                (CASE WHEN $4::bool AND $5::bool THEN
                    (SELECT COUNT(*) FROM batch_aggregates
                     WHERE user_id = $1
                       AND ($2::timestamptz IS NULL OR created_at >= $2)
                       AND ($3::timestamptz IS NULL OR created_at <= $3))
                ELSE 0 END)
                +
                (SELECT COUNT(*) FROM credits_transactions
                 WHERE user_id = $1
                   AND fusillade_batch_id IS NULL
                   AND ($6::text IS NULL OR description ILIKE '%' || $6 || '%')
                   AND ($7::text[] IS NULL OR transaction_type::text = ANY($7))
                   AND ($2::timestamptz IS NULL OR created_at >= $2)
                   AND ($3::timestamptz IS NULL OR created_at <= $3))
            as "count!"
            "#,
            user_id,
            filters.start_date,
            filters.end_date,
            include_batches,
            search_matches_batch,
            filters.search.as_deref(),
            transaction_types.as_deref(),
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(result.count)
    }

    /// Sum the signed amounts of the most recent N transactions for a user within the date-filtered set.
    /// Positive transactions (admin_grant, purchase) are positive, negative (usage, admin_removal) are negative.
    /// This is used to calculate the balance at a specific point in the transaction history.
    /// Only date filters are applied - search and type filters are excluded since they break
    /// chronological ordering which the frontend relies on for running balance calculation.
    #[instrument(skip(self, filters), fields(user_id = %abbrev_uuid(&user_id), count = count), err)]
    pub async fn sum_recent_transactions(&mut self, user_id: UserId, count: i64, filters: &TransactionFilters) -> Result<Decimal> {
        let result = sqlx::query!(
            r#"
            SELECT COALESCE(SUM(
                CASE WHEN transaction_type IN ('admin_grant', 'purchase') THEN amount ELSE -amount END
            ), 0) as "sum!"
            FROM (
                SELECT transaction_type, amount
                FROM credits_transactions
                WHERE user_id = $1
                  AND ($3::timestamptz IS NULL OR created_at >= $3)
                  AND ($4::timestamptz IS NULL OR created_at <= $4)
                ORDER BY seq DESC
                LIMIT $2
            ) recent
            "#,
            user_id,
            count,
            filters.start_date,
            filters.end_date,
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(result.sum)
    }

    /// Sum the signed amounts of all transactions after a given date for a user.
    /// This is used to calculate the balance at a specific point in time when date filtering.
    #[instrument(skip(self), fields(user_id = %abbrev_uuid(&user_id)), err)]
    pub async fn sum_transactions_after_date(&mut self, user_id: UserId, after_date: DateTime<Utc>) -> Result<Decimal> {
        let result = sqlx::query!(
            r#"
            SELECT COALESCE(SUM(
                CASE WHEN transaction_type IN ('admin_grant', 'purchase') THEN amount ELSE -amount END
            ), 0) as "sum!"
            FROM credits_transactions
            WHERE user_id = $1
              AND created_at > $2
            "#,
            user_id,
            after_date,
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(result.sum)
    }

    /// Sum the signed amounts of all grouped transaction items after a given date for a user.
    /// This operates on the same grouped view as `list_transactions_with_batches`.
    #[instrument(skip(self), fields(user_id = %abbrev_uuid(&user_id)), err)]
    pub async fn sum_transactions_after_date_grouped(&mut self, user_id: UserId, after_date: DateTime<Utc>) -> Result<Decimal> {
        // First ensure any pending batch transactions are aggregated
        self.aggregate_user_batches(user_id).await?;

        let result = sqlx::query!(
            r#"
            SELECT COALESCE(SUM(signed_amount), 0) as "sum!"
            FROM (
                -- Batch aggregates after the date
                SELECT -ba.total_amount as signed_amount
                FROM batch_aggregates ba
                WHERE ba.user_id = $1
                  AND ba.created_at > $2

                UNION ALL

                -- Non-batched transactions after the date
                SELECT
                    CASE WHEN ct.transaction_type IN ('admin_grant', 'purchase')
                        THEN ct.amount
                        ELSE -ct.amount
                    END as signed_amount
                FROM credits_transactions ct
                WHERE ct.user_id = $1
                  AND ct.fusillade_batch_id IS NULL
                  AND ct.created_at > $2
            ) after_date
            "#,
            user_id,
            after_date,
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(result.sum)
    }

    /// Sum the signed amounts of the most recent N grouped transaction items for a user.
    /// This operates on the same grouped view as `list_transactions_with_batches`:
    /// - Batch aggregates count as single items (with their total_amount)
    /// - Non-batched transactions count as single items
    /// This is used to calculate the balance at a specific point when batch grouping is enabled.
    /// Only date filters are applied - search and type filters are excluded since they break
    /// chronological ordering which the frontend relies on for running balance calculation.
    #[instrument(skip(self, filters), fields(user_id = %abbrev_uuid(&user_id), count = count), err)]
    pub async fn sum_recent_transactions_grouped(&mut self, user_id: UserId, count: i64, filters: &TransactionFilters) -> Result<Decimal> {
        // First ensure any pending batch transactions are aggregated
        self.aggregate_user_batches(user_id).await?;

        // Sum from the same UNION view used by list_transactions_with_batches
        // All batch aggregates are usage type (negative), non-batched follow normal signing rules
        // Only date filters are applied to maintain chronological ordering for balance calculation.
        let result = sqlx::query!(
            r#"
            SELECT COALESCE(SUM(signed_amount), 0) as "sum!"
            FROM (
                SELECT * FROM (
                    (SELECT
                        ba.max_seq,
                        -ba.total_amount as signed_amount
                    FROM batch_aggregates ba
                    WHERE ba.user_id = $1
                      AND ($3::timestamptz IS NULL OR ba.created_at >= $3)
                      AND ($4::timestamptz IS NULL OR ba.created_at <= $4)
                    ORDER BY ba.max_seq DESC
                    LIMIT $2)

                    UNION ALL

                    -- Non-batched transactions
                    (SELECT
                        ct.seq as max_seq,
                        CASE WHEN ct.transaction_type IN ('admin_grant', 'purchase')
                            THEN ct.amount
                            ELSE -ct.amount
                        END as signed_amount
                    FROM credits_transactions ct
                    WHERE ct.user_id = $1
                      AND ct.fusillade_batch_id IS NULL
                      AND ($3::timestamptz IS NULL OR ct.created_at >= $3)
                      AND ($4::timestamptz IS NULL OR ct.created_at <= $4)
                    ORDER BY ct.seq DESC
                    LIMIT $2)
                ) combined
                ORDER BY max_seq DESC
                LIMIT $2
            ) recent
            "#,
            user_id,            // $1
            count,              // $2
            filters.start_date, // $3
            filters.end_date,   // $4
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(result.sum)
    }

    /// Perform lazy aggregation for a user's unaggregated batched transactions.
    /// This aggregates new transactions into batch_aggregates and marks them as aggregated.
    /// Uses a single atomic UPDATE + aggregate approach to handle concurrent reads safely.
    #[instrument(skip(self), fields(user_id = %abbrev_uuid(&user_id)), err)]
    async fn aggregate_user_batches(&mut self, user_id: UserId) -> Result<()> {
        // Atomically mark transactions as aggregated and aggregate them in one query
        // This uses UPDATE ... RETURNING with aggregation via CTE to avoid race conditions
        let result = sqlx::query!(
            r#"
            WITH marked AS (
                UPDATE credits_transactions
                SET is_aggregated = true
                WHERE user_id = $1
                  AND fusillade_batch_id IS NOT NULL
                  AND is_aggregated = false
                RETURNING fusillade_batch_id, amount, seq, created_at
            ),
            aggregated AS (
                SELECT
                    fusillade_batch_id,
                    SUM(amount) as total_amount,
                    COUNT(*) as tx_count,
                    MAX(seq) as max_seq,
                    MIN(created_at) as created_at
                FROM marked
                GROUP BY fusillade_batch_id
            )
            INSERT INTO batch_aggregates (fusillade_batch_id, user_id, total_amount, transaction_count, max_seq, created_at, updated_at)
            SELECT fusillade_batch_id, $1, total_amount, tx_count::int, max_seq, created_at, NOW()
            FROM aggregated
            ON CONFLICT (fusillade_batch_id) DO UPDATE SET
                total_amount = batch_aggregates.total_amount + EXCLUDED.total_amount,
                transaction_count = batch_aggregates.transaction_count + EXCLUDED.transaction_count,
                max_seq = GREATEST(batch_aggregates.max_seq, EXCLUDED.max_seq),
                updated_at = NOW()
            RETURNING fusillade_batch_id
            "#,
            user_id
        )
        .fetch_all(&mut *self.db)
        .await?;

        if !result.is_empty() {
            trace!("Aggregated {} batches for user {}", result.len(), user_id);
        }

        Ok(())
    }

    /// List transactions with batch grouping applied using pre-aggregated batch_aggregates table.
    /// Uses optimized query with pre-limited UNION branches for O(limit) performance.
    #[instrument(skip(self, filters), fields(user_id = %abbrev_uuid(&user_id), skip = skip, limit = limit), err)]
    pub async fn list_transactions_with_batches(
        &mut self,
        user_id: UserId,
        skip: i64,
        limit: i64,
        filters: &TransactionFilters,
    ) -> Result<Vec<TransactionWithCategory>> {
        // Perform lazy aggregation for any new unaggregated transactions
        self.aggregate_user_batches(user_id).await?;

        let transaction_types: Option<Vec<String>> = filters
            .transaction_types
            .as_ref()
            .map(|types| types.iter().map(transaction_type_to_string).collect());

        // Check if we should include batch aggregates (they're always type 'usage')
        let include_batches = filters
            .transaction_types
            .as_ref()
            .map(|types| types.iter().any(|t| matches!(t, CreditTransactionType::Usage)))
            .unwrap_or(true);

        // Check if search term would match "Batch" description
        let search_matches_batch = filters
            .search
            .as_ref()
            .map(|s| "batch".contains(&s.to_lowercase()) || s.to_lowercase().contains("batch"))
            .unwrap_or(true);

        // Optimized query using pre-limited UNION branches for Merge Append
        // Each branch fetches skip+limit rows, then pagination applies to combined result
        // For batches: join with http_analytics to get batch_request_source and batch_sla
        let fetch_limit = skip + limit;
        let rows = sqlx::query!(
            r#"
            SELECT * FROM (
                -- Top N from batch_aggregates (index scan on idx_batch_agg_user_seq)
                -- Only included if transaction_types filter includes 'usage' or is not set
                -- and search term matches "Batch" description
                -- JOIN with http_analytics to get batch_request_source and batch_sla
                (SELECT
                    ba.fusillade_batch_id as id,
                    ba.user_id,
                    'usage' as "transaction_type!: CreditTransactionType",
                    ba.total_amount as amount,
                    ba.fusillade_batch_id::text as source_id,
                    'Batch'::text as description,
                    ba.created_at,
                    ba.max_seq,
                    ba.fusillade_batch_id as batch_id,
                    ba.transaction_count as batch_count,
                    COALESCE(NULLIF(sample_ha.batch_request_source, ''), 'fusillade') as request_origin,
                    COALESCE(sample_ha.batch_sla, '') as batch_sla
                FROM batch_aggregates ba
                LEFT JOIN LATERAL (
                    SELECT batch_request_source, batch_sla
                    FROM http_analytics ha
                    WHERE ha.fusillade_batch_id = ba.fusillade_batch_id
                    LIMIT 1
                ) sample_ha ON true
                WHERE ba.user_id = $1
                  AND $7::bool = true
                  AND $10::bool = true
                  AND ($5::text IS NULL OR 'Batch' ILIKE '%' || $5 || '%')
                  AND ($8::timestamptz IS NULL OR ba.created_at >= $8)
                  AND ($9::timestamptz IS NULL OR ba.created_at <= $9)
                ORDER BY ba.max_seq DESC
                LIMIT $2)

                UNION ALL

                -- Top N from non-batched transactions (index scan on idx_credits_tx_non_batched)
                -- JOIN with http_analytics to get request_origin for non-batch usage transactions
                (SELECT
                    ct.id,
                    ct.user_id,
                    ct.transaction_type as "transaction_type!: CreditTransactionType",
                    ct.amount,
                    ct.source_id,
                    ct.description,
                    ct.created_at,
                    ct.seq as max_seq,
                    NULL::uuid as batch_id,
                    1::int as batch_count,
                    ha.request_origin as request_origin,
                    ha.batch_sla as batch_sla
                FROM credits_transactions ct
                LEFT JOIN http_analytics ha ON ha.id::text = ct.source_id
                WHERE ct.user_id = $1
                  AND ct.fusillade_batch_id IS NULL
                  AND ($5::text IS NULL OR ct.description ILIKE '%' || $5 || '%')
                  AND ($6::text[] IS NULL OR ct.transaction_type::text = ANY($6))
                  AND ($8::timestamptz IS NULL OR ct.created_at >= $8)
                  AND ($9::timestamptz IS NULL OR ct.created_at <= $9)
                ORDER BY ct.seq DESC
                LIMIT $2)
            ) combined
            ORDER BY max_seq DESC
            LIMIT $3 OFFSET $4
            "#,
            user_id,                      // $1
            fetch_limit,                  // $2
            limit,                        // $3
            skip,                         // $4
            filters.search.as_deref(),    // $5
            transaction_types.as_deref(), // $6
            include_batches,              // $7
            filters.start_date,           // $8
            filters.end_date,             // $9
            search_matches_batch,         // $10
        )
        .fetch_all(&mut *self.db)
        .await?;

        let mut results = Vec::new();
        for row in rows {
            let id = row.id.ok_or_else(|| sqlx::Error::Protocol("Query returned NULL id".to_string()))?;
            let row_user_id = row
                .user_id
                .ok_or_else(|| sqlx::Error::Protocol("Query returned NULL user_id".to_string()))?;
            let amount = row
                .amount
                .ok_or_else(|| sqlx::Error::Protocol("Query returned NULL amount".to_string()))?;
            let source_id = row
                .source_id
                .ok_or_else(|| sqlx::Error::Protocol("Query returned NULL source_id".to_string()))?;
            let created_at = row
                .created_at
                .ok_or_else(|| sqlx::Error::Protocol("Query returned NULL created_at".to_string()))?;

            let transaction = CreditTransactionDBResponse {
                id,
                user_id: row_user_id,
                transaction_type: row.transaction_type,
                amount,
                description: row.description,
                source_id,
                created_at,
            };
            results.push(TransactionWithCategory {
                transaction,
                batch_id: row.batch_id,
                request_origin: row.request_origin,
                batch_sla: row.batch_sla,
                batch_count: row.batch_count.unwrap_or(1),
            });
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
            fusillade_batch_id: None,
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
            fusillade_batch_id: None,
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
            .list_user_transactions(user_id, 0, n_of_transactions, &TransactionFilters::default())
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
            .list_user_transactions(user_id, 0, 2, &TransactionFilters::default())
            .await
            .expect("Failed to list transactions");
        assert_eq!(transactions.len(), 2);

        // Test skip
        let transactions = credits
            .list_user_transactions(user_id, 2, 2, &TransactionFilters::default())
            .await
            .expect("Failed to list transactions");
        assert_eq!(transactions.len(), 2);

        // Test skip beyond available
        let transactions = credits
            .list_user_transactions(user_id, 10, 2, &TransactionFilters::default())
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
            .list_user_transactions(user1_id, 0, 10, &TransactionFilters::default())
            .await
            .expect("Failed to list transactions");
        assert_eq!(transactions.len(), 1);
        assert_eq!(transactions[0].user_id, user1_id);
        // Verify balance via get_user_balance
        let balance = credits.get_user_balance(user1_id).await.expect("Failed to get balance");
        assert_eq!(balance, Decimal::from_str("100.0").unwrap());

        // List user2's transactions
        let transactions = credits
            .list_user_transactions(user2_id, 0, 10, &TransactionFilters::default())
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
            .list_user_transactions(non_existent_user_id, 0, 10, &TransactionFilters::default())
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

        let transactions = credits
            .list_all_transactions(0, 10, &TransactionFilters::default())
            .await
            .expect("Failed to list transactions");

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
        let transactions = credits
            .list_all_transactions(0, 2, &TransactionFilters::default())
            .await
            .expect("Failed to list transactions");
        assert_eq!(transactions.len(), 2);

        // Test skip
        let transactions = credits
            .list_all_transactions(2, 2, &TransactionFilters::default())
            .await
            .expect("Failed to list transactions");
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
            fusillade_batch_id: None,
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
            fusillade_batch_id: None,
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
            fusillade_batch_id: None,
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
            .list_user_transactions(user_id, 0, 10, &TransactionFilters::default())
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
                fusillade_batch_id: None,
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
                    fusillade_batch_id: None,
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
            .list_user_transactions(user_id, 0, 1000, &TransactionFilters::default())
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
            fusillade_batch_id: None,
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

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_transactions_with_date_range_filter(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Create 3 transactions
        credits
            .create_transaction(&CreditTransactionCreateDBRequest::admin_grant(
                user_id,
                user_id,
                Decimal::from_str("100.0").unwrap(),
                Some("Transaction 1".to_string()),
            ))
            .await
            .expect("Failed to create transaction 1");

        let tx2 = credits
            .create_transaction(&CreditTransactionCreateDBRequest::admin_grant(
                user_id,
                user_id,
                Decimal::from_str("200.0").unwrap(),
                Some("Transaction 2".to_string()),
            ))
            .await
            .expect("Failed to create transaction 2");

        credits
            .create_transaction(&CreditTransactionCreateDBRequest::admin_grant(
                user_id,
                user_id,
                Decimal::from_str("300.0").unwrap(),
                Some("Transaction 3".to_string()),
            ))
            .await
            .expect("Failed to create transaction 3");

        // Filter: from tx2's timestamp onwards (should get tx2 and tx3)
        let filters = TransactionFilters {
            start_date: Some(tx2.created_at),
            end_date: Some(Utc::now() + chrono::Duration::hours(1)),
            ..Default::default()
        };

        let filtered_txs = credits
            .list_user_transactions(user_id, 0, 10, &filters)
            .await
            .expect("Failed to list filtered transactions");

        assert_eq!(filtered_txs.len(), 2, "Should return 2 transactions within date range");

        let count = credits
            .count_user_transactions(user_id, &filters)
            .await
            .expect("Failed to count filtered transactions");

        assert_eq!(count, 2, "Count should match filtered transactions");

        // Test: Filter with no dates (should get all 3)
        let all_txs = credits
            .list_user_transactions(user_id, 0, 10, &TransactionFilters::default())
            .await
            .expect("Failed to list all transactions");

        assert_eq!(all_txs.len(), 3, "Should return all 3 transactions with no date filter");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_transactions_with_only_start_date(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Create 3 transactions
        credits
            .create_transaction(&CreditTransactionCreateDBRequest::admin_grant(
                user_id,
                user_id,
                Decimal::from_str("100.0").unwrap(),
                Some("Transaction 1".to_string()),
            ))
            .await
            .expect("Failed to create transaction 1");

        let tx2 = credits
            .create_transaction(&CreditTransactionCreateDBRequest::admin_grant(
                user_id,
                user_id,
                Decimal::from_str("200.0").unwrap(),
                Some("Transaction 2".to_string()),
            ))
            .await
            .expect("Failed to create transaction 2");

        credits
            .create_transaction(&CreditTransactionCreateDBRequest::admin_grant(
                user_id,
                user_id,
                Decimal::from_str("300.0").unwrap(),
                Some("Transaction 3".to_string()),
            ))
            .await
            .expect("Failed to create transaction 3");

        // Filter from tx2's timestamp onwards (should get tx2 and tx3)
        let filters = TransactionFilters {
            start_date: Some(tx2.created_at),
            ..Default::default()
        };

        let filtered_txs = credits
            .list_user_transactions(user_id, 0, 10, &filters)
            .await
            .expect("Failed to list transactions with start_date");

        assert_eq!(filtered_txs.len(), 2, "Should return 2 transactions after cutoff");

        let count = credits
            .count_user_transactions(user_id, &filters)
            .await
            .expect("Failed to count transactions");

        assert_eq!(count as usize, filtered_txs.len(), "Count should match filtered results");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_transactions_with_only_end_date(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Create 3 transactions
        credits
            .create_transaction(&CreditTransactionCreateDBRequest::admin_grant(
                user_id,
                user_id,
                Decimal::from_str("100.0").unwrap(),
                Some("Transaction 1".to_string()),
            ))
            .await
            .expect("Failed to create transaction 1");

        let tx2 = credits
            .create_transaction(&CreditTransactionCreateDBRequest::admin_grant(
                user_id,
                user_id,
                Decimal::from_str("200.0").unwrap(),
                Some("Transaction 2".to_string()),
            ))
            .await
            .expect("Failed to create transaction 2");

        credits
            .create_transaction(&CreditTransactionCreateDBRequest::admin_grant(
                user_id,
                user_id,
                Decimal::from_str("300.0").unwrap(),
                Some("Transaction 3".to_string()),
            ))
            .await
            .expect("Failed to create transaction 3");

        // Filter up to tx2's timestamp (should get tx1 and tx2)
        let filters = TransactionFilters {
            end_date: Some(tx2.created_at),
            ..Default::default()
        };

        let filtered_txs = credits
            .list_user_transactions(user_id, 0, 10, &filters)
            .await
            .expect("Failed to list transactions with end_date");

        assert_eq!(filtered_txs.len(), 2, "Should return 2 transactions before cutoff");

        let count = credits
            .count_user_transactions(user_id, &filters)
            .await
            .expect("Failed to count transactions");

        assert_eq!(count as usize, filtered_txs.len(), "Count should match filtered results");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_all_transactions_with_date_filter(pool: PgPool) {
        let user1_id = create_test_user(&pool).await;
        let user2_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Create transaction for user 1
        credits
            .create_transaction(&CreditTransactionCreateDBRequest::admin_grant(
                user1_id,
                user1_id,
                Decimal::from_str("100.0").unwrap(),
                Some("User 1 transaction".to_string()),
            ))
            .await
            .expect("Failed to create transaction");

        // Create transaction for user 2
        let tx2 = credits
            .create_transaction(&CreditTransactionCreateDBRequest::admin_grant(
                user2_id,
                user2_id,
                Decimal::from_str("200.0").unwrap(),
                Some("User 2 transaction".to_string()),
            ))
            .await
            .expect("Failed to create transaction");

        // Filter from tx2's timestamp (should get user2's transaction only)
        let filters = TransactionFilters {
            start_date: Some(tx2.created_at),
            ..Default::default()
        };

        let filtered_txs = credits
            .list_all_transactions(0, 10, &filters)
            .await
            .expect("Failed to list all transactions with filter");

        assert_eq!(filtered_txs.len(), 1, "Should have 1 transaction after cutoff");

        let count = credits
            .count_all_transactions(&filters)
            .await
            .expect("Failed to count all transactions");

        assert_eq!(count as usize, filtered_txs.len(), "Count should match filtered results");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_transactions_with_batches_date_filter(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        let batch_id = Uuid::new_v4();

        // Create batch transactions
        let mut batch_txs = Vec::new();
        for i in 0..3 {
            let tx = credits
                .create_transaction(&CreditTransactionCreateDBRequest {
                    user_id,
                    transaction_type: CreditTransactionType::Usage,
                    amount: Decimal::from_str(&format!("{}.0", i + 1)).unwrap(),
                    source_id: format!("batch-{}", i),
                    description: Some(format!("Batch transaction {}", i)),
                    fusillade_batch_id: Some(batch_id),
                })
                .await
                .expect("Failed to create batch transaction");
            batch_txs.push(tx);
        }

        // Create non-batch transaction
        let non_batch_tx = credits
            .create_transaction(&CreditTransactionCreateDBRequest::admin_grant(
                user_id,
                user_id,
                Decimal::from_str("100.0").unwrap(),
                Some("Non-batch transaction".to_string()),
            ))
            .await
            .expect("Failed to create non-batch transaction");

        // Test with no filter - should get all (1 batch grouped + 1 non-batch = 2)
        let all_txs = credits
            .list_transactions_with_batches(user_id, 0, 10, &TransactionFilters::default())
            .await
            .expect("Failed to list all batched transactions");

        assert_eq!(all_txs.len(), 2, "Should have batch + non-batch");

        // Test with date filter from non_batch_tx timestamp (should get non-batch only)
        let filters = TransactionFilters {
            start_date: Some(non_batch_tx.created_at),
            ..Default::default()
        };

        let filtered_txs = credits
            .list_transactions_with_batches(user_id, 0, 10, &filters)
            .await
            .expect("Failed to list batched transactions with filter");

        assert_eq!(filtered_txs.len(), 1, "Should have only non-batch transaction");

        let count = credits
            .count_transactions_with_batches(user_id, &filters)
            .await
            .expect("Failed to count batched transactions");

        assert_eq!(count as usize, filtered_txs.len(), "Count should match filtered grouped results");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_date_filter_handles_empty_results(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Create a transaction now
        let request = CreditTransactionCreateDBRequest::admin_grant(
            user_id,
            user_id,
            Decimal::from_str("100.0").unwrap(),
            Some("Test transaction".to_string()),
        );
        credits.create_transaction(&request).await.expect("Failed to create transaction");

        // Filter for transactions from a week ago to 2 days ago (should return nothing)
        let filters = TransactionFilters {
            start_date: Some(Utc::now() - chrono::Duration::days(7)),
            end_date: Some(Utc::now() - chrono::Duration::days(2)),
            ..Default::default()
        };

        let filtered_txs = credits
            .list_user_transactions(user_id, 0, 10, &filters)
            .await
            .expect("Failed to list transactions");

        assert_eq!(filtered_txs.len(), 0, "Should return no transactions outside date range");

        let count = credits
            .count_user_transactions(user_id, &filters)
            .await
            .expect("Failed to count transactions");

        assert_eq!(count, 0, "Count should be 0 for empty results");
    }
}
