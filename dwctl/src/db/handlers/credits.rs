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

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgConnection};
use std::collections::HashMap;
use tracing::{instrument, trace};
use uuid::Uuid;

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
    pub api_key_id: Option<Uuid>,
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
            api_key_id: tx.api_key_id,
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

    /// Create a new credit transaction.
    ///
    /// The row is folded into the user_balance_checkpoints read model in the
    /// same statement, so callers observe the new balance immediately (a user
    /// who just paid must not wait for anything asynchronous). Batched rows
    /// are also aggregated into batch_aggregates in the same statement. This
    /// path is low-volume by construction - purchases, admin adjustments,
    /// promo grants; high-volume usage charging goes through the
    /// request-logging batcher, which folds the same way per flush.
    ///
    /// If the balance crosses zero in either direction, sends a pg_notify so
    /// onwards re-evaluates key eligibility.
    ///
    /// Locking: this takes a row lock on the user's checkpoint row, which the
    /// analytics batcher's flush fold also updates. When calling inside an
    /// enclosing transaction, keep that transaction short and DB-local -
    /// holding it across external I/O would stall flushes for its duration.
    #[instrument(skip(self, request), fields(user_id = %abbrev_uuid(&request.user_id), transaction_type = ?request.transaction_type, amount = %request.amount), err)]
    pub async fn create_transaction(&mut self, request: &CreditTransactionCreateDBRequest) -> Result<CreditTransactionDBResponse> {
        let signed_amount = match request.transaction_type {
            CreditTransactionType::AdminGrant | CreditTransactionType::Purchase => request.amount,
            CreditTransactionType::Usage | CreditTransactionType::AdminRemoval => -request.amount,
        };

        // Insert + read-model fold + batch aggregation in one atomic
        // statement (callers may or may not be inside an enclosing
        // transaction). The checkpoint INSERT arm only fires if the user's
        // row is missing, which the user-creation trigger normally prevents.
        let row = sqlx::query!(
            r#"
            WITH inserted AS (
                INSERT INTO credits_transactions (user_id, transaction_type, amount, source_id, description, fusillade_batch_id, api_key_id, is_aggregated)
                VALUES ($1, $2, $3, $4, $5, $6::uuid, $7, $6::uuid IS NOT NULL)
                RETURNING id, user_id, transaction_type, amount, source_id, description, created_at, seq, api_key_id
            ),
            bumped AS (
                INSERT INTO user_balance_checkpoints (user_id, checkpoint_seq, balance)
                SELECT user_id, seq, $8 FROM inserted
                ON CONFLICT (user_id) DO UPDATE SET
                    balance = user_balance_checkpoints.balance + EXCLUDED.balance,
                    checkpoint_seq = GREATEST(user_balance_checkpoints.checkpoint_seq, EXCLUDED.checkpoint_seq),
                    updated_at = NOW()
                RETURNING balance
            ),
            aggregated AS (
                INSERT INTO batch_aggregates (fusillade_batch_id, user_id, total_amount, transaction_count, max_seq, created_at, updated_at)
                SELECT $6::uuid, user_id, amount, 1, seq, created_at, NOW()
                FROM inserted
                WHERE $6::uuid IS NOT NULL
                ON CONFLICT (fusillade_batch_id) DO UPDATE SET
                    total_amount = batch_aggregates.total_amount + EXCLUDED.total_amount,
                    transaction_count = batch_aggregates.transaction_count + EXCLUDED.transaction_count,
                    max_seq = GREATEST(batch_aggregates.max_seq, EXCLUDED.max_seq),
                    updated_at = NOW()
            )
            SELECT
                i.id AS "id!",
                i.user_id AS "user_id!",
                i.transaction_type AS "transaction_type!: CreditTransactionType",
                i.amount AS "amount!",
                i.source_id AS "source_id!",
                i.description,
                i.created_at AS "created_at!",
                i.seq AS "seq!",
                i.api_key_id,
                b.balance AS "new_balance!"
            FROM inserted i, bumped b
            "#,
            request.user_id,
            &request.transaction_type as &CreditTransactionType,
            request.amount,
            request.source_id,
            request.description,
            request.fusillade_batch_id,
            request.api_key_id,
            signed_amount,
        )
        .fetch_one(&mut *self.db)
        .await?;

        trace!("Created transaction {} for user_id {}", row.id, request.user_id);

        let new_balance = row.new_balance;
        let old_balance = new_balance - signed_amount;
        if old_balance <= Decimal::ZERO && new_balance > Decimal::ZERO {
            trace!("Balance crossed zero upward for user_id {}, notifying onwards", request.user_id);
            self.notify_balance_crossing().await?;
        } else if old_balance > Decimal::ZERO && new_balance <= Decimal::ZERO {
            trace!("Balance crossed zero downward for user_id {}, notifying onwards", request.user_id);
            self.notify_balance_crossing().await?;
        }

        Ok(CreditTransactionDBResponse {
            id: row.id,
            user_id: row.user_id,
            transaction_type: row.transaction_type,
            amount: row.amount,
            description: row.description,
            source_id: row.source_id,
            created_at: row.created_at,
            api_key_id: row.api_key_id,
        })
    }

    /// Grant a first-payment match bonus to `payee` if eligible.
    ///
    /// The promotion matches a user's first ever payment with bonus credits, up
    /// to `match_up_to` (in dollars); `match_up_to <= 0` disables it.
    ///
    /// Eligibility is derived from the ledger by ordering, not by counting:
    /// the triggering purchase (identified by `purchase_source_id`) is "first"
    /// iff no purchase exists for the payee with a lower `seq`. Using the
    /// monotonic `seq` is concurrency-safe in a way that "any other purchase by
    /// source_id" is not - with two simultaneous first payments, exactly the
    /// lowest-seq one qualifies, so we never silently grant zero matches (and
    /// existing paying customers are still never matched). It also requires the
    /// purchase to actually exist before we grant.
    ///
    /// The bonus is recorded as an `admin_grant` with a derived `source_id`, so a
    /// webhook+poll double-fire or a webhook retry cannot double-grant (the
    /// `source_id` unique constraint makes the second insert a no-op). Reuses
    /// `create_transaction`, so the bonus also triggers the balance-restored
    /// notify like any other credit.
    #[instrument(skip(self), fields(payee = %abbrev_uuid(&payee), match_up_to = %match_up_to), err)]
    pub async fn grant_first_payment_match(
        &mut self,
        match_up_to: Decimal,
        payee: UserId,
        payment_amount: Decimal,
        purchase_source_id: &str,
    ) -> Result<()> {
        if match_up_to <= Decimal::ZERO {
            return Ok(());
        }
        let match_amount = payment_amount.min(match_up_to);
        if match_amount <= Decimal::ZERO {
            return Ok(());
        }

        // Locate the triggering purchase's ordering key. If it doesn't exist
        // (shouldn't happen - the caller records it first), there's nothing to
        // match against, so skip rather than guess.
        let Some(purchase_seq) = sqlx::query_scalar!(
            "SELECT seq FROM credits_transactions WHERE source_id = $1 AND transaction_type = 'purchase'",
            purchase_source_id
        )
        .fetch_optional(&mut *self.db)
        .await?
        else {
            return Ok(());
        };

        // First payment = no earlier purchase (lower seq) for this payee.
        let is_first = sqlx::query_scalar!(
            r#"
            SELECT NOT EXISTS (
                SELECT 1 FROM credits_transactions
                WHERE user_id = $1
                  AND transaction_type = 'purchase'
                  AND seq < $2
            ) AS "is_first!"
            "#,
            payee,
            purchase_seq
        )
        .fetch_one(&mut *self.db)
        .await?;

        if !is_first {
            return Ok(());
        }

        let request = CreditTransactionCreateDBRequest {
            user_id: payee,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: match_amount,
            source_id: format!("{purchase_source_id}:first-payment-match"),
            description: Some(format!("First payment match bonus (matched {})", match_amount)),
            fusillade_batch_id: None,
            api_key_id: None,
        };

        match self.create_transaction(&request).await {
            Ok(_) => {
                trace!("Granted first-payment match of {} to payee {}", match_amount, payee);
                Ok(())
            }
            // Another concurrent payment path already granted the match for this
            // source: idempotent no-op.
            Err(crate::db::errors::DbError::UniqueViolation { constraint, .. })
                if constraint.as_deref() == Some("credits_transactions_source_id_unique") =>
            {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Send a pg_notify so the onwards config sync re-evaluates key
    /// eligibility. Only called on zero crossings (edge-triggered), so the
    /// resulting full reloads are rare.
    /// Format: "credits_transactions:{epoch_micros}" to match other triggers
    /// and enable lag metrics.
    async fn notify_balance_crossing(&mut self) -> Result<()> {
        let epoch_micros = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros();

        let payload = format!("credits_transactions:{}", epoch_micros);

        sqlx::query("SELECT pg_notify('auth_config_changed', $1)")
            .bind(&payload)
            .execute(&mut *self.db)
            .await?;

        Ok(())
    }

    /// Get current balance for a user: a point read of the
    /// user_balance_checkpoints read model.
    ///
    /// The read model is total (a row is created with the user and maintained
    /// by the inline credit path and the background applier); a missing row
    /// can only mean a user that has never existed, so it reads as zero.
    #[instrument(skip(self), fields(user_id = %abbrev_uuid(&user_id)), err)]
    pub async fn get_user_balance(&mut self, user_id: UserId) -> Result<Decimal> {
        let balance = sqlx::query_scalar!(r#"SELECT balance FROM user_balance_checkpoints WHERE user_id = $1"#, user_id)
            .fetch_optional(&mut *self.db)
            .await?;

        Ok(balance.unwrap_or(Decimal::ZERO))
    }

    /// Get balances for multiple users: point reads of the read model.
    #[instrument(skip(self, user_ids), fields(count = user_ids.len()), err)]
    pub async fn get_users_balances_bulk(&mut self, user_ids: &[UserId]) -> Result<HashMap<UserId, Decimal>> {
        if user_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let rows = sqlx::query!(
            r#"
            SELECT
                u.user_id as "user_id!",
                COALESCE(c.balance, 0) as "balance!"
            FROM unnest($1::uuid[]) AS u(user_id)
            LEFT JOIN user_balance_checkpoints c ON c.user_id = u.user_id
            "#,
            user_ids
        )
        .fetch_all(&mut *self.db)
        .await?;

        let mut balances_map = HashMap::with_capacity(rows.len());
        for row in rows {
            balances_map.insert(row.user_id, row.balance);
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
            SELECT id, user_id, transaction_type as "transaction_type: CreditTransactionType", amount, source_id, description, created_at, seq, api_key_id
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
            SELECT id, user_id, transaction_type as "transaction_type: CreditTransactionType", amount, source_id, description, created_at, seq, api_key_id
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
                amount, source_id, description, created_at, seq, api_key_id
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

    /// Get the total amount of auto top-up charges for a user in the current calendar month (UTC).
    #[instrument(skip(self), err)]
    pub async fn get_monthly_auto_topup_spend(&mut self, user_id: UserId) -> Result<rust_decimal::Decimal> {
        let row = sqlx::query!(
            r#"
            SELECT COALESCE(SUM(amount), 0)::decimal(20, 9) as "total!"
            FROM credits_transactions
            WHERE user_id = $1
              AND source_id LIKE 'auto_topup_%'
              AND created_at >= date_trunc('month', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'
            "#,
            user_id
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(row.total)
    }

    /// Get the total auto top-up spend for multiple users in the current calendar month (UTC).
    /// Returns a map of user_id → total spend. Users with no auto-topup transactions this month
    /// will be absent from the map; callers should treat missing entries as zero.
    #[instrument(skip(self, user_ids), fields(count = user_ids.len()), err)]
    pub async fn get_monthly_auto_topup_spend_bulk(&mut self, user_ids: &[UserId]) -> Result<HashMap<UserId, rust_decimal::Decimal>> {
        if user_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let rows = sqlx::query!(
            r#"
            SELECT user_id, COALESCE(SUM(amount), 0)::decimal(20, 9) as "total!"
            FROM credits_transactions
            WHERE user_id = ANY($1)
              AND source_id LIKE 'auto_topup_%'
              AND created_at >= date_trunc('month', now() AT TIME ZONE 'UTC') AT TIME ZONE 'UTC'
            GROUP BY user_id
            "#,
            user_ids
        )
        .fetch_all(&mut *self.db)
        .await?;

        let mut map = HashMap::with_capacity(rows.len());
        for row in rows {
            map.insert(row.user_id, row.total);
        }
        Ok(map)
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
                api_key_id: None,
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

        // The read model is total: user creation itself (DB trigger) must
        // have created the zero checkpoint row, not just a zero fallback.
        let row_balance = sqlx::query_scalar!("SELECT balance FROM user_balance_checkpoints WHERE user_id = $1", user_id)
            .fetch_one(&pool)
            .await
            .expect("checkpoint row must exist from the user-creation trigger");
        assert_eq!(row_balance, Decimal::ZERO);
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
            api_key_id: None,
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
            api_key_id: None,
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
        let skip_page = credits
            .list_all_transactions(2, 2, &TransactionFilters::default())
            .await
            .expect("Failed to list transactions");
        assert!(skip_page.len() >= 2);
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
            api_key_id: None,
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
            api_key_id: None,
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
            api_key_id: None,
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
                    api_key_id: None,
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

    /// Test that admin_grant crossing zero upward sends pg_notify
    #[sqlx::test]
    #[test_log::test]
    async fn test_balance_restored_notification_on_admin_grant(pool: PgPool) {
        use sqlx::postgres::PgListener;
        use std::time::Duration;
        use tokio::time::timeout;

        let user_id = create_test_user(&pool).await;

        // Set up listener for auth_config_changed notifications
        let mut listener = PgListener::connect_with(&pool).await.expect("Failed to create listener");
        listener.listen("auth_config_changed").await.expect("Failed to listen");

        // Create initial negative balance by granting then using more
        {
            let mut conn = pool.acquire().await.expect("Failed to acquire connection");
            let mut credits = Credits::new(&mut conn);

            // Grant 10 credits
            let grant = CreditTransactionCreateDBRequest::admin_grant(
                user_id,
                user_id,
                Decimal::from_str("10.0").unwrap(),
                Some("Initial grant".to_string()),
            );
            credits.create_transaction(&grant).await.expect("Failed to grant");
        }

        // Drain any notifications from the initial grant (user went from 0 to positive)
        tokio::time::sleep(Duration::from_millis(50)).await;
        while timeout(Duration::from_millis(10), listener.try_recv()).await.is_ok() {}

        // Use 15 credits to go negative
        {
            let mut conn = pool.acquire().await.expect("Failed to acquire connection");
            let mut credits = Credits::new(&mut conn);

            let usage = CreditTransactionCreateDBRequest {
                user_id,
                transaction_type: CreditTransactionType::Usage,
                amount: Decimal::from_str("15.0").unwrap(),
                source_id: Uuid::new_v4().to_string(),
                description: Some("Usage to go negative".to_string()),
                fusillade_batch_id: None,
                api_key_id: None,
            };
            credits.create_transaction(&usage).await.expect("Failed to use");
        }

        // Drain any notifications (usage doesn't trigger notification via this path)
        tokio::time::sleep(Duration::from_millis(50)).await;
        while timeout(Duration::from_millis(10), listener.try_recv()).await.is_ok() {}

        // Now grant credits to cross zero upward - this SHOULD trigger notification
        {
            let mut conn = pool.acquire().await.expect("Failed to acquire connection");
            let mut credits = Credits::new(&mut conn);

            let grant = CreditTransactionCreateDBRequest::admin_grant(
                user_id,
                user_id,
                Decimal::from_str("20.0").unwrap(),
                Some("Grant to restore balance".to_string()),
            );
            credits.create_transaction(&grant).await.expect("Failed to grant");
        }

        // Should receive notification for crossing zero upward
        let notification = timeout(Duration::from_secs(2), listener.recv())
            .await
            .expect("Timeout waiting for notification")
            .expect("Failed to receive notification");

        assert_eq!(notification.channel(), "auth_config_changed");

        // Verify payload format: "credits_transactions:{epoch_micros}"
        let payload = notification.payload();
        assert!(
            payload.starts_with("credits_transactions:"),
            "Expected payload to start with 'credits_transactions:', got: {}",
            payload
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
            api_key_id: None,
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
                    api_key_id: None,
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
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_monthly_auto_topup_spend_zero_for_new_user(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        let spend = credits.get_monthly_auto_topup_spend(user_id).await.expect("Failed to get spend");
        assert_eq!(spend, Decimal::ZERO, "New user should have zero monthly auto-topup spend");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_monthly_auto_topup_spend_sums_only_auto_topup(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Create an auto top-up transaction (source_id starts with "auto_topup_")
        credits
            .create_transaction(&CreditTransactionCreateDBRequest {
                user_id,
                transaction_type: CreditTransactionType::Purchase,
                amount: Decimal::from_str("25.0").unwrap(),
                source_id: format!("auto_topup_{}_2026-03-01T10:00", user_id),
                description: Some("Auto top-up".to_string()),
                fusillade_batch_id: None,
                api_key_id: None,
            })
            .await
            .unwrap();

        // Create a non-auto-topup transaction (should be excluded)
        credits
            .create_transaction(&CreditTransactionCreateDBRequest {
                user_id,
                transaction_type: CreditTransactionType::Purchase,
                amount: Decimal::from_str("100.0").unwrap(),
                source_id: format!("manual_topup_{}", Uuid::new_v4()),
                description: Some("Manual purchase".to_string()),
                fusillade_batch_id: None,
                api_key_id: None,
            })
            .await
            .unwrap();

        // Create a second auto top-up transaction
        credits
            .create_transaction(&CreditTransactionCreateDBRequest {
                user_id,
                transaction_type: CreditTransactionType::Purchase,
                amount: Decimal::from_str("25.0").unwrap(),
                source_id: format!("auto_topup_{}_2026-03-02T10:00", user_id),
                description: Some("Auto top-up 2".to_string()),
                fusillade_batch_id: None,
                api_key_id: None,
            })
            .await
            .unwrap();

        let spend = credits.get_monthly_auto_topup_spend(user_id).await.expect("Failed to get spend");
        assert_eq!(
            spend,
            Decimal::from_str("50.0").unwrap(),
            "Should sum only auto_topup_ transactions"
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_monthly_auto_topup_spend_excludes_other_users(pool: PgPool) {
        let user_a = create_test_user(&pool).await;
        let user_b = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Create auto top-up for user A
        credits
            .create_transaction(&CreditTransactionCreateDBRequest {
                user_id: user_a,
                transaction_type: CreditTransactionType::Purchase,
                amount: Decimal::from_str("30.0").unwrap(),
                source_id: format!("auto_topup_{}_2026-03-01T10:00", user_a),
                description: None,
                fusillade_batch_id: None,
                api_key_id: None,
            })
            .await
            .unwrap();

        // Create auto top-up for user B
        credits
            .create_transaction(&CreditTransactionCreateDBRequest {
                user_id: user_b,
                transaction_type: CreditTransactionType::Purchase,
                amount: Decimal::from_str("50.0").unwrap(),
                source_id: format!("auto_topup_{}_2026-03-01T10:00", user_b),
                description: None,
                fusillade_batch_id: None,
                api_key_id: None,
            })
            .await
            .unwrap();

        let spend_a = credits.get_monthly_auto_topup_spend(user_a).await.unwrap();
        assert_eq!(
            spend_a,
            Decimal::from_str("30.0").unwrap(),
            "User A should only see their own spend"
        );

        let spend_b = credits.get_monthly_auto_topup_spend(user_b).await.unwrap();
        assert_eq!(
            spend_b,
            Decimal::from_str("50.0").unwrap(),
            "User B should only see their own spend"
        );
    }

    /// Insert a purchase transaction for a user (test helper).
    async fn insert_purchase(credits: &mut Credits<'_>, user: UserId, amount: &str, source_id: &str) {
        let request = CreditTransactionCreateDBRequest {
            user_id: user,
            transaction_type: CreditTransactionType::Purchase,
            amount: Decimal::from_str(amount).unwrap(),
            source_id: source_id.to_string(),
            description: None,
            fusillade_batch_id: None,
            api_key_id: None,
        };
        credits.create_transaction(&request).await.expect("insert purchase");
    }

    /// Fetch the bonus amount granted for a given purchase source_id, if any.
    async fn match_bonus_amount(pool: &PgPool, source_id: &str) -> Option<Decimal> {
        sqlx::query_scalar!(
            "SELECT amount FROM credits_transactions WHERE source_id = $1",
            format!("{source_id}:first-payment-match")
        )
        .fetch_optional(pool)
        .await
        .expect("query bonus")
    }

    #[sqlx::test]
    async fn test_first_payment_match_grants_on_first_purchase(pool: PgPool) {
        let user = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut credits = Credits::new(&mut conn);

        insert_purchase(&mut credits, user, "30.0", "sess-1").await;
        credits
            .grant_first_payment_match(
                Decimal::from_str("50.0").unwrap(),
                user,
                Decimal::from_str("30.0").unwrap(),
                "sess-1",
            )
            .await
            .unwrap();

        // First payment of 30 is under the 50 cap, so it is matched in full.
        assert_eq!(match_bonus_amount(&pool, "sess-1").await, Some(Decimal::from_str("30.0").unwrap()));
    }

    #[sqlx::test]
    async fn test_first_payment_match_caps_at_match_up_to(pool: PgPool) {
        let user = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut credits = Credits::new(&mut conn);

        insert_purchase(&mut credits, user, "100.0", "sess-1").await;
        credits
            .grant_first_payment_match(
                Decimal::from_str("50.0").unwrap(),
                user,
                Decimal::from_str("100.0").unwrap(),
                "sess-1",
            )
            .await
            .unwrap();

        // First payment of 100 is capped at the 50 match limit.
        assert_eq!(match_bonus_amount(&pool, "sess-1").await, Some(Decimal::from_str("50.0").unwrap()));
    }

    #[sqlx::test]
    async fn test_first_payment_match_skips_when_not_first(pool: PgPool) {
        let user = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut credits = Credits::new(&mut conn);

        // A prior purchase exists, so the current one is not the user's first.
        insert_purchase(&mut credits, user, "20.0", "sess-0").await;
        insert_purchase(&mut credits, user, "30.0", "sess-1").await;
        credits
            .grant_first_payment_match(
                Decimal::from_str("50.0").unwrap(),
                user,
                Decimal::from_str("30.0").unwrap(),
                "sess-1",
            )
            .await
            .unwrap();

        assert_eq!(match_bonus_amount(&pool, "sess-1").await, None);
    }

    #[sqlx::test]
    async fn test_first_payment_match_disabled_when_zero(pool: PgPool) {
        let user = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut credits = Credits::new(&mut conn);

        insert_purchase(&mut credits, user, "30.0", "sess-1").await;
        credits
            .grant_first_payment_match(Decimal::ZERO, user, Decimal::from_str("30.0").unwrap(), "sess-1")
            .await
            .unwrap();

        assert_eq!(match_bonus_amount(&pool, "sess-1").await, None);
    }

    #[sqlx::test]
    async fn test_first_payment_match_idempotent(pool: PgPool) {
        let user = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut credits = Credits::new(&mut conn);

        insert_purchase(&mut credits, user, "30.0", "sess-1").await;
        for _ in 0..2 {
            credits
                .grant_first_payment_match(
                    Decimal::from_str("50.0").unwrap(),
                    user,
                    Decimal::from_str("30.0").unwrap(),
                    "sess-1",
                )
                .await
                .unwrap();
        }

        // The derived source_id's unique constraint means only one bonus exists.
        let count = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM credits_transactions WHERE source_id = $1",
            "sess-1:first-payment-match"
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, Some(1));
    }
}
