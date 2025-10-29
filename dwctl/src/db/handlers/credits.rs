use crate::types::UserId;
use crate::{
    db::{
        errors::{DbError, Result},
        models::credits::{
            CreditTransactionCreateDBRequest, CreditTransactionDBResponse, CreditTransactionType,
            UserCreditBalanceDBResponse,
        },
    },
};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sqlx::{Connection, FromRow, PgConnection};

/// Filter for listing credit transactions
#[derive(Debug, Clone)]
pub struct CreditTransactionFilter {
    pub user_id: Option<UserId>,
    pub skip: i64,
    pub limit: i64,
}

impl CreditTransactionFilter {
    pub fn new(user_id: Option<UserId>, skip: i64, limit: i64) -> Self {
        Self { user_id, skip, limit }
    }
}

// Database entity model for credit transaction
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
struct CreditTransaction {
    pub id: i64,
    pub user_id: UserId,
    #[sqlx(rename = "transaction_type")]
    pub transaction_type: CreditTransactionType,
    pub amount: Decimal,
    pub balance_after: Decimal,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl From<CreditTransaction> for CreditTransactionDBResponse {
    fn from(tx: CreditTransaction) -> Self {
        Self {
            id: tx.id,
            user_id: tx.user_id,
            transaction_type: tx.transaction_type,
            amount: tx.amount,
            balance_after: tx.balance_after,
            description: tx.description,
            created_at: tx.created_at,
        }
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
    /// This method calculates the new balance based on the transaction type and amount
    pub async fn create_transaction(&mut self, request: &CreditTransactionCreateDBRequest) -> Result<CreditTransactionDBResponse> {
        let mut tx = self.db.begin().await?;

        // Get current balance for the user
        let current_balance = Self::get_user_current_balance_internal(&mut tx, request.user_id).await?;

        // Validate that the calculated balance matches the provided balance_after
        if current_balance != request.balance_after {
            return Err(DbError::Other(anyhow::anyhow!(
                "Balance mismatch: current balance is {}, but balance_after is {}",
                current_balance,
                request.balance_after
            )));
        }

        // Insert the transaction
        let transaction = sqlx::query_as!(
            CreditTransaction,
            r#"
            INSERT INTO credit_transactions (user_id, transaction_type, amount, balance_after, description)
            VALUES ($1, $2, $3, $4, $5)
            RETURNING id, user_id, transaction_type as "transaction_type: CreditTransactionType", amount, balance_after, description, created_at
            "#,
            request.user_id,
            &request.transaction_type as &CreditTransactionType,
            request.amount,
            request.balance_after,
            request.description
        )
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(CreditTransactionDBResponse::from(transaction))
    }

    /// Get current balance for a user (latest balance_after from credit_transactions)
    pub async fn get_user_balance(&mut self, user_id: UserId) -> Result<Decimal> {
        let balance = Self::get_user_current_balance_internal(&mut *self.db, user_id).await?;
        Ok(balance)
    }

    /// Internal helper to get current balance within an existing transaction
    async fn get_user_current_balance_internal(tx: &mut PgConnection, user_id: UserId) -> Result<Decimal> {
        let result = sqlx::query!(
            r#"
            SELECT balance_after
            FROM credit_transactions
            WHERE user_id = $1
            ORDER BY created_at DESC, id DESC
            LIMIT 1
            "#,
            user_id
        )
        .fetch_optional(tx)
        .await?;

        Ok(result.map(|r| r.balance_after).unwrap_or(Decimal::ZERO))
    }

    /// List transactions for a specific user with pagination
    pub async fn list_user_transactions(
        &mut self,
        user_id: UserId,
        skip: i64,
        limit: i64,
    ) -> Result<Vec<CreditTransactionDBResponse>> {
        let transactions = sqlx::query_as!(
            CreditTransaction,
            r#"
            SELECT id, user_id, transaction_type as "transaction_type: CreditTransactionType", amount, balance_after, description, created_at
            FROM credit_transactions
            WHERE user_id = $1
            ORDER BY created_at DESC, id DESC
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
    pub async fn list_all_transactions(&mut self, skip: i64, limit: i64) -> Result<Vec<CreditTransactionDBResponse>> {
        let transactions = sqlx::query_as!(
            CreditTransaction,
            r#"
            SELECT id, user_id, transaction_type as "transaction_type: CreditTransactionType", amount, balance_after, description, created_at
            FROM credit_transactions
            ORDER BY created_at DESC, id DESC
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

    /// Get all users with their current credit balances
    pub async fn list_all_user_balances(&mut self) -> Result<Vec<UserCreditBalanceDBResponse>> {
        let balances = sqlx::query!(
            r#"
            SELECT DISTINCT ON (user_id) user_id, balance_after as current_balance
            FROM credit_transactions
            ORDER BY user_id, created_at DESC, id DESC
            "#
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(balances
            .into_iter()
            .map(|b| UserCreditBalanceDBResponse {
                user_id: b.user_id,
                current_balance: b.current_balance,
            })
            .collect())
    }
}
