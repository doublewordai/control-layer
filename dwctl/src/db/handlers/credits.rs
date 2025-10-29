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
    /// This method validates the balance_after is correct based on the current balance
    pub async fn create_transaction(&mut self, request: &CreditTransactionCreateDBRequest) -> Result<CreditTransactionDBResponse> {
        let mut tx = self.db.begin().await?;

        // Get current balance for the user
        let current_balance = Self::get_user_current_balance_internal(&mut tx, request.user_id).await?;

        // Calculate what the new balance should be based on transaction type
        let expected_new_balance = match request.transaction_type {
            CreditTransactionType::AdminGrant | CreditTransactionType::Purchase => {
                current_balance + request.amount
            }
            CreditTransactionType::AdminRemoval | CreditTransactionType::Usage => {
                current_balance - request.amount
            }
        };

        // Validate that the provided balance_after matches our calculation
        if expected_new_balance != request.balance_after {
            return Err(DbError::Other(anyhow::anyhow!(
                "Balance mismatch: current balance is {}, transaction would result in {}, but balance_after is {}",
                current_balance,
                expected_new_balance,
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

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use sqlx::PgPool;
    use std::str::FromStr;
    use uuid::Uuid;

    async fn create_test_user(pool: &PgPool) -> UserId {
        use crate::api::models::users::Role;

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
        sqlx::query!(
            "INSERT INTO user_roles (user_id, role) VALUES ($1, $2)",
            user_id,
            role as Role
        )
        .execute(pool)
        .await
        .expect("Failed to add user role");

        user_id
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_transaction_admin_grant(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        let request = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("100.50").unwrap(),
            balance_after: Decimal::from_str("100.50").unwrap(),
            description: Some("Test grant".to_string()),
        };

        let transaction = credits.create_transaction(&request).await.expect("Failed to create transaction");

        assert_eq!(transaction.user_id, user_id);
        assert_eq!(transaction.transaction_type, CreditTransactionType::AdminGrant);
        assert_eq!(transaction.amount, Decimal::from_str("100.50").unwrap());
        assert_eq!(transaction.balance_after, Decimal::from_str("100.50").unwrap());
        assert_eq!(transaction.description, Some("Test grant".to_string()));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_transaction_balance_mismatch_error(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Create first transaction
        let request1 = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("100.0").unwrap(),
            balance_after: Decimal::from_str("100.0").unwrap(),
            description: None,
        };
        credits.create_transaction(&request1).await.expect("Failed to create first transaction");

        // Try to create second transaction with wrong balance_after
        let request2 = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("50.0").unwrap(),
            balance_after: Decimal::from_str("200.0").unwrap(), // Should be 150.0
            description: None,
        };

        let result = credits.create_transaction(&request2).await;
        assert!(result.is_err());
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
    async fn test_get_user_balance_after_transactions(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Add credits
        let request1 = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("100.0").unwrap(),
            balance_after: Decimal::from_str("100.0").unwrap(),
            description: None,
        };
        credits.create_transaction(&request1).await.expect("Failed to create transaction");

        let balance = credits.get_user_balance(user_id).await.expect("Failed to get balance");
        assert_eq!(balance, Decimal::from_str("100.0").unwrap());

        // Add more credits
        let request2 = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("50.0").unwrap(),
            balance_after: Decimal::from_str("150.0").unwrap(),
            description: None,
        };
        credits.create_transaction(&request2).await.expect("Failed to create transaction");

        let balance = credits.get_user_balance(user_id).await.expect("Failed to get balance");
        assert_eq!(balance, Decimal::from_str("150.0").unwrap());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_user_transactions_ordering(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Create multiple transactions with cumulative balances
        let amounts = vec!["10.0", "20.0", "30.0"];
        let mut cumulative_balance = Decimal::ZERO;
        for (i, amount) in amounts.iter().enumerate() {
            let amount_decimal = Decimal::from_str(amount).unwrap();
            cumulative_balance += amount_decimal;
            let request = CreditTransactionCreateDBRequest {
                user_id,
                transaction_type: CreditTransactionType::AdminGrant,
                amount: amount_decimal,
                balance_after: cumulative_balance,
                description: Some(format!("Transaction {}", i + 1)),
            };
            credits.create_transaction(&request).await.expect("Failed to create transaction");
        }

        let transactions = credits.list_user_transactions(user_id, 0, 10).await.expect("Failed to list transactions");

        // Should be ordered by created_at DESC, id DESC (most recent first)
        // Balances should be: 60.0 (10+20+30), 30.0 (10+20), 10.0
        assert_eq!(transactions.len(), 3);
        assert_eq!(transactions[0].balance_after, Decimal::from_str("60.0").unwrap());
        assert_eq!(transactions[1].balance_after, Decimal::from_str("30.0").unwrap());
        assert_eq!(transactions[2].balance_after, Decimal::from_str("10.0").unwrap());
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
            let request = CreditTransactionCreateDBRequest {
                user_id,
                transaction_type: CreditTransactionType::AdminGrant,
                amount,
                balance_after: cumulative_balance,
                description: None,
            };
            credits.create_transaction(&request).await.expect("Failed to create transaction");
        }

        // Test limit
        let transactions = credits.list_user_transactions(user_id, 0, 2).await.expect("Failed to list transactions");
        assert_eq!(transactions.len(), 2);

        // Test skip
        let transactions = credits.list_user_transactions(user_id, 2, 2).await.expect("Failed to list transactions");
        assert_eq!(transactions.len(), 2);

        // Test skip beyond available
        let transactions = credits.list_user_transactions(user_id, 10, 2).await.expect("Failed to list transactions");
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
        let request1 = CreditTransactionCreateDBRequest {
            user_id: user1_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("100.0").unwrap(),
            balance_after: Decimal::from_str("100.0").unwrap(),
            description: None,
        };
        credits.create_transaction(&request1).await.expect("Failed to create transaction");

        // Create transactions for user2
        let request2 = CreditTransactionCreateDBRequest {
            user_id: user2_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("200.0").unwrap(),
            balance_after: Decimal::from_str("200.0").unwrap(),
            description: None,
        };
        credits.create_transaction(&request2).await.expect("Failed to create transaction");

        // List user1's transactions
        let transactions = credits.list_user_transactions(user1_id, 0, 10).await.expect("Failed to list transactions");
        assert_eq!(transactions.len(), 1);
        assert_eq!(transactions[0].user_id, user1_id);
        assert_eq!(transactions[0].balance_after, Decimal::from_str("100.0").unwrap());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_all_transactions(pool: PgPool) {
        let user1_id = create_test_user(&pool).await;
        let user2_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Create transactions for both users
        let request1 = CreditTransactionCreateDBRequest {
            user_id: user1_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("100.0").unwrap(),
            balance_after: Decimal::from_str("100.0").unwrap(),
            description: Some("User 1 grant".to_string()),
        };
        credits.create_transaction(&request1).await.expect("Failed to create transaction");

        let request2 = CreditTransactionCreateDBRequest {
            user_id: user2_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("200.0").unwrap(),
            balance_after: Decimal::from_str("200.0").unwrap(),
            description: Some("User 2 grant".to_string()),
        };
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
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Create 5 transactions with cumulative balances
        let mut cumulative_balance = Decimal::ZERO;
        for i in 1..=5 {
            let amount = Decimal::from(i * 10);
            cumulative_balance += amount;
            let request = CreditTransactionCreateDBRequest {
                user_id,
                transaction_type: CreditTransactionType::AdminGrant,
                amount,
                balance_after: cumulative_balance,
                description: None,
            };
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
    async fn test_list_all_user_balances(pool: PgPool) {
        let user1_id = create_test_user(&pool).await;
        let user2_id = create_test_user(&pool).await;
        let user3_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Create transactions for each user with different balances
        let request1 = CreditTransactionCreateDBRequest {
            user_id: user1_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("100.0").unwrap(),
            balance_after: Decimal::from_str("100.0").unwrap(),
            description: None,
        };
        credits.create_transaction(&request1).await.expect("Failed to create transaction");

        let request2 = CreditTransactionCreateDBRequest {
            user_id: user2_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("200.0").unwrap(),
            balance_after: Decimal::from_str("200.0").unwrap(),
            description: None,
        };
        credits.create_transaction(&request2).await.expect("Failed to create transaction");

        let request3 = CreditTransactionCreateDBRequest {
            user_id: user3_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("300.0").unwrap(),
            balance_after: Decimal::from_str("300.0").unwrap(),
            description: None,
        };
        credits.create_transaction(&request3).await.expect("Failed to create transaction");

        let balances = credits.list_all_user_balances().await.expect("Failed to list balances");

        // Should have at least our 3 users
        assert!(balances.len() >= 3);

        // Verify all users are present with correct balances
        assert!(balances.iter().any(|b| b.user_id == user1_id && b.current_balance == Decimal::from_str("100.0").unwrap()));
        assert!(balances.iter().any(|b| b.user_id == user2_id && b.current_balance == Decimal::from_str("200.0").unwrap()));
        assert!(balances.iter().any(|b| b.user_id == user3_id && b.current_balance == Decimal::from_str("300.0").unwrap()));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_all_user_balances_returns_latest_balance(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Create multiple transactions for the same user
        let request1 = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("100.0").unwrap(),
            balance_after: Decimal::from_str("100.0").unwrap(),
            description: None,
        };
        credits.create_transaction(&request1).await.expect("Failed to create transaction");

        let request2 = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("50.0").unwrap(),
            balance_after: Decimal::from_str("150.0").unwrap(),
            description: None,
        };
        credits.create_transaction(&request2).await.expect("Failed to create transaction");

        let balances = credits.list_all_user_balances().await.expect("Failed to list balances");

        // Should only have one entry per user with the latest balance
        let user_balances: Vec<_> = balances.iter().filter(|b| b.user_id == user_id).collect();
        assert_eq!(user_balances.len(), 1);
        assert_eq!(user_balances[0].current_balance, Decimal::from_str("150.0").unwrap());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_transaction_with_all_transaction_types(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Test AdminGrant
        let request = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("100.0").unwrap(),
            balance_after: Decimal::from_str("100.0").unwrap(),
            description: Some("Grant".to_string()),
        };
        let tx = credits.create_transaction(&request).await.expect("Failed to create AdminGrant");
        assert_eq!(tx.transaction_type, CreditTransactionType::AdminGrant);

        // Test Purchase
        let request = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::Purchase,
            amount: Decimal::from_str("50.0").unwrap(),
            balance_after: Decimal::from_str("150.0").unwrap(),
            description: Some("Purchase".to_string()),
        };
        let tx = credits.create_transaction(&request).await.expect("Failed to create Purchase");
        assert_eq!(tx.transaction_type, CreditTransactionType::Purchase);

        // Test Usage
        let request = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::Usage,
            amount: Decimal::from_str("25.0").unwrap(),
            balance_after: Decimal::from_str("125.0").unwrap(),
            description: Some("Usage".to_string()),
        };
        let tx = credits.create_transaction(&request).await.expect("Failed to create Usage");
        assert_eq!(tx.transaction_type, CreditTransactionType::Usage);

        // Test AdminRemoval
        let request = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminRemoval,
            amount: Decimal::from_str("25.0").unwrap(),
            balance_after: Decimal::from_str("100.0").unwrap(),
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
        let request1 = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("100.0").unwrap(),
            balance_after: Decimal::from_str("100.0").unwrap(),
            description: None,
        };
        credits.create_transaction(&request1).await.expect("Failed to create transaction");

        // Try to create an invalid transaction (wrong balance_after)
        let request2 = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("50.0").unwrap(),
            balance_after: Decimal::from_str("200.0").unwrap(), // Wrong balance
            description: None,
        };
        let result = credits.create_transaction(&request2).await;
        assert!(result.is_err());

        // Verify the balance hasn't changed (transaction was rolled back)
        let balance = credits.get_user_balance(user_id).await.expect("Failed to get balance");
        assert_eq!(balance, Decimal::from_str("100.0").unwrap());

        // Verify only one transaction exists
        let transactions = credits.list_user_transactions(user_id, 0, 10).await.expect("Failed to list transactions");
        assert_eq!(transactions.len(), 1);
    }
}
