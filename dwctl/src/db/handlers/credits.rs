use crate::{
    db::{
        errors::Result,
        models::credits::{CreditTransactionCreateDBRequest, CreditTransactionDBResponse, CreditTransactionType},
    },
    types::UserId,
};
use chrono::{DateTime, Utc};
use rust_decimal::{prelude::ToPrimitive, Decimal};
use serde::{Deserialize, Serialize};
use sqlx::{Connection, FromRow, PgConnection};
use std::collections::HashMap;
use tracing::{error, trace};
use uuid::Uuid;

// Database entity model for credit transaction
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct CreditTransaction {
    pub id: Uuid,
    pub user_id: UserId,
    #[sqlx(rename = "transaction_type")]
    pub transaction_type: CreditTransactionType,
    pub amount: Decimal,
    pub balance_after: Decimal,
    pub previous_transaction_id: Option<Uuid>,
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
            previous_transaction_id: tx.previous_transaction_id,
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
        // Start the transaction
        let mut tx = self.db.begin().await?;

        // Convert UUID to int64 for advisory lock
        // We use the first 8 bytes of the UUID as the lock key
        let user_uuid_bytes = request.user_id.as_bytes();
        let lock_key = i64::from_be_bytes([
            user_uuid_bytes[0],
            user_uuid_bytes[1],
            user_uuid_bytes[2],
            user_uuid_bytes[3],
            user_uuid_bytes[4],
            user_uuid_bytes[5],
            user_uuid_bytes[6],
            user_uuid_bytes[7],
        ]);

        // Use pg_advisory_xact_lock which is transaction-scoped (auto-releases on commit/rollback)
        // This will BLOCK until the lock is available, ensuring serialization
        sqlx::query_scalar::<_, i32>("SELECT 1 FROM (SELECT pg_advisory_xact_lock($1)) AS _")
            .bind(lock_key)
            .fetch_one(&mut *tx)
            .await?;

        trace!("Acquired lock for user_id {}", request.user_id);

        // Now safely get the current balance - no race condition possible
        let (current_balance, last_transaction_id) = match sqlx::query!(
            r#"
            SELECT balance_after, id
            FROM credits_transactions
            WHERE user_id = $1
            ORDER BY created_at DESC, id DESC
            LIMIT 1
            "#,
            request.user_id
        )
        .fetch_one(&mut *tx)
        .await
        {
            Ok(record) => (record.balance_after, Some(record.id)),
            Err(sqlx::Error::RowNotFound) => (Decimal::ZERO, None),
            Err(e) => return Err(e.into()),
        };

        // Calculate what the new balance should be based on transaction type
        let new_balance = match request.transaction_type {
            CreditTransactionType::AdminGrant | CreditTransactionType::Purchase => current_balance + request.amount,
            CreditTransactionType::AdminRemoval | CreditTransactionType::Usage => current_balance - request.amount,
        };

        // Insert the transaction, there is protection on the DB so will return an error if balance goes negative which is why there isn't a check here.
        let transaction = sqlx::query_as!(
            CreditTransaction,
            r#"
            INSERT INTO credits_transactions (user_id, transaction_type, amount, balance_after, previous_transaction_id, description)
            VALUES ($1, $2, $3, $4, $5, $6)
            RETURNING id, user_id, transaction_type as "transaction_type: CreditTransactionType", amount, balance_after, previous_transaction_id, description, created_at
            "#,
            request.user_id,
            &request.transaction_type as &CreditTransactionType,
            request.amount,
            new_balance,
            last_transaction_id,
            request.description
        )
        .fetch_one(&mut *tx)
        .await?;

        tx.commit().await?;

        Ok(CreditTransactionDBResponse::from(transaction))
    }

    /// Get current balance for a user (latest balance_after from credits_transactions)
    /// This is a read-only operation without locking
    pub async fn get_user_balance(&mut self, user_id: UserId) -> Result<Decimal> {
        let result = sqlx::query!(
            r#"
            SELECT balance_after
            FROM credits_transactions
            WHERE user_id = $1
            ORDER BY created_at DESC, id DESC
            LIMIT 1
            "#,
            user_id
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(result.map(|r| r.balance_after).unwrap_or(Decimal::ZERO))
    }

    pub async fn get_users_balances_bulk(&mut self, user_ids: &[UserId]) -> Result<HashMap<UserId, f64>> {
        let rows = sqlx::query!(
            r#"
            SELECT DISTINCT ON (user_id) user_id, balance_after
            FROM credits_transactions
            WHERE user_id = ANY($1)
            ORDER BY user_id, created_at DESC, id DESC
            "#,
            user_ids
        )
        .fetch_all(&mut *self.db)
        .await?;

        let mut balances_map = HashMap::new();
        for row in rows {
            balances_map.insert(
                row.user_id,
                row.balance_after.to_f64().unwrap_or_else(|| {
                    error!("Failed to convert balance to f64 for user_id {}", row.user_id);
                    0.0
                }),
            );
        }

        Ok(balances_map)
    }

    /// List transactions for a specific user with pagination
    pub async fn list_user_transactions(&mut self, user_id: UserId, skip: i64, limit: i64) -> Result<Vec<CreditTransactionDBResponse>> {
        let transactions = sqlx::query_as!(
            CreditTransaction,
            r#"
            SELECT id, user_id, transaction_type as "transaction_type: CreditTransactionType", amount, balance_after, previous_transaction_id, description, created_at
            FROM credits_transactions
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
            SELECT id, user_id, transaction_type as "transaction_type: CreditTransactionType", amount, balance_after, previous_transaction_id, description, created_at
            FROM credits_transactions
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

    /// Get a single transaction by its ID
    pub async fn get_transaction_by_id(&mut self, transaction_id: Uuid) -> Result<Option<CreditTransactionDBResponse>> {
        let transaction = sqlx::query_as!(
            CreditTransaction,
            r#"
            SELECT id, user_id, transaction_type as "transaction_type: CreditTransactionType",
                amount, balance_after, previous_transaction_id, description, created_at
            FROM credits_transactions
            WHERE id = $1
            "#,
            transaction_id
        )
        .fetch_optional(&mut *self.db)
        .await?;

        Ok(transaction.map(CreditTransactionDBResponse::from))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{api::models::users::Role, db::errors::DbError};
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

        let request = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("100.50").unwrap(),
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
    async fn test_get_user_balance_after_transactions(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Add credits
        let request1 = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("100.0").unwrap(),
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
            description: None,
        };
        credits.create_transaction(&request2).await.expect("Failed to create transaction");

        let balance = credits.get_user_balance(user_id).await.expect("Failed to get balance");
        assert_eq!(balance, Decimal::from_str("150.0").unwrap());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_rejection_if_balance_is_insufficient(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Try to remove credits from admin removal when balance is zero
        let request1 = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminRemoval,
            amount: Decimal::from_str("100.0").unwrap(),
            description: None,
        };
        let result = credits.create_transaction(&request1).await;
        match result {
            Err(DbError::CheckViolation { .. }) => {
                // Expected error
            }
            _ => panic!("Expected CheckViolation error due to insufficient balance"),
        }

        // Create first transaction with positive balance
        let request1 = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("100.50").unwrap(),
            description: None,
        };
        let transaction1 = credits
            .create_transaction(&request1)
            .await
            .expect("Failed to create first transaction");

        assert_eq!(transaction1.balance_after, Decimal::from_str("100.50").unwrap());

        // Try to remove credits from usage that exceeds balance
        let request1 = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::Usage,
            amount: Decimal::from_str("1050.0").unwrap(),
            description: None,
        };
        let result = credits.create_transaction(&request1).await;
        match result {
            Err(DbError::CheckViolation { .. }) => {
                // Expected error
            }
            _ => panic!("Expected CheckViolation error due to insufficient balance"),
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_transaction_balance_after_multiple_transactions(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Create first transaction
        let request1 = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("100.50").unwrap(),
            description: None,
        };
        let transaction1 = credits
            .create_transaction(&request1)
            .await
            .expect("Failed to create first transaction");

        assert_eq!(transaction1.user_id, user_id);
        assert_eq!(transaction1.transaction_type, CreditTransactionType::AdminGrant);
        assert_eq!(transaction1.amount, Decimal::from_str("100.50").unwrap());
        assert_eq!(transaction1.balance_after, Decimal::from_str("100.50").unwrap());
        assert_eq!(transaction1.description, None);

        // Try to create second transaction with wrong balance_after
        let request2 = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("50.0").unwrap(),
            description: None,
        };

        let transaction2 = credits
            .create_transaction(&request2)
            .await
            .expect("Failed to create second transaction");

        assert_eq!(transaction2.user_id, user_id);
        assert_eq!(transaction2.transaction_type, CreditTransactionType::AdminGrant);
        assert_eq!(transaction2.amount, Decimal::from_str("50.0").unwrap());
        assert_eq!(transaction2.balance_after, Decimal::from_str("150.50").unwrap());
        assert_eq!(transaction2.description, None);

        // Create third transaction that deducts credits
        let request3 = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminRemoval,
            amount: Decimal::from_str("30.0").unwrap(),
            description: Some("Usage deduction".to_string()),
        };

        let transaction3 = credits
            .create_transaction(&request3)
            .await
            .expect("Failed to create third transaction");

        assert_eq!(transaction3.user_id, user_id);
        assert_eq!(transaction3.transaction_type, CreditTransactionType::AdminRemoval);
        assert_eq!(transaction3.amount, Decimal::from_str("30.0").unwrap());
        assert_eq!(transaction3.balance_after, Decimal::from_str("120.50").unwrap());
        assert_eq!(transaction3.description, Some("Usage deduction".to_string()));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_user_transactions_ordering(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);
        let n_of_transactions = 10;

        for i in 1..n_of_transactions + 1 {
            let request = CreditTransactionCreateDBRequest {
                user_id,
                transaction_type: CreditTransactionType::AdminGrant,
                amount: Decimal::from(i * 10),
                description: Some(format!("Transaction {}", i + 1)),
            };
            credits.create_transaction(&request).await.expect("Failed to create transaction");
        }

        let transactions = credits
            .list_user_transactions(user_id, 0, n_of_transactions)
            .await
            .expect("Failed to list transactions");

        // Should be ordered by created_at DESC, id DESC (most recent first)
        assert_eq!(transactions.len(), n_of_transactions as usize);
        for i in 0..(transactions.len() - 1) {
            let t1 = &transactions[i];
            let t2 = &transactions[i + 1];
            assert!(t1.created_at >= t2.created_at, "Transactions are not ordered by created_at DESC");
            if t1.created_at == t2.created_at {
                assert!(t1.id > t2.id, "Transactions with same created_at are not ordered by id DESC");
            }
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
            let request = CreditTransactionCreateDBRequest {
                user_id,
                transaction_type: CreditTransactionType::AdminGrant,
                amount: Decimal::from(i * 10),
                description: Some(format!("Transaction {}", i + 1)),
            };
            transaction_ids.push(credits.create_transaction(&request).await.expect("Failed to create transaction").id);
        }

        let mut total_balance: Decimal = Decimal::ZERO;
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
                    total_balance += tx.amount;
                    assert_eq!(tx.balance_after, total_balance);
                }
                None => panic!("Transaction ID {} not found", transaction_ids[i - 1]),
            };
        }
        // Assert non existent transaction ID returns None
        assert!(credits
            .get_transaction_by_id(Uuid::new_v4())
            .await
            .expect("Failed to get transaction by ID 99999999999")
            .is_none())
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
                description: None,
            };
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
        let request1 = CreditTransactionCreateDBRequest {
            user_id: user1_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("100.0").unwrap(),
            description: None,
        };
        credits.create_transaction(&request1).await.expect("Failed to create transaction");

        // Create transactions for user2
        let request2 = CreditTransactionCreateDBRequest {
            user_id: user2_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("200.0").unwrap(),
            description: None,
        };
        credits.create_transaction(&request2).await.expect("Failed to create transaction");

        // List user1's transactions
        let transactions = credits
            .list_user_transactions(user1_id, 0, 10)
            .await
            .expect("Failed to list transactions");
        assert_eq!(transactions.len(), 1);
        assert_eq!(transactions[0].user_id, user1_id);
        assert_eq!(transactions[0].balance_after, Decimal::from_str("100.0").unwrap());

        // List user2's transactions
        let transactions = credits
            .list_user_transactions(user2_id, 0, 10)
            .await
            .expect("Failed to list transactions");
        assert_eq!(transactions.len(), 1);
        assert_eq!(transactions[0].user_id, user2_id);
        assert_eq!(transactions[0].balance_after, Decimal::from_str("200.0").unwrap());

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
        let request1 = CreditTransactionCreateDBRequest {
            user_id: user1_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("100.0").unwrap(),
            description: Some("User 1 grant".to_string()),
        };
        credits.create_transaction(&request1).await.expect("Failed to create transaction");

        let request2 = CreditTransactionCreateDBRequest {
            user_id: user2_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("200.0").unwrap(),
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
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Create 10 transactions
        let mut cumulative_balance = Decimal::ZERO;
        for i in 1..10 {
            let amount = Decimal::from(i * 10);
            cumulative_balance += amount;
            let request = CreditTransactionCreateDBRequest {
                user_id: create_test_user(&pool).await,
                transaction_type: CreditTransactionType::AdminGrant,
                amount,
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
    async fn test_create_transaction_with_all_transaction_types(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);

        // Test AdminGrant
        let request = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("100.0").unwrap(),
            description: Some("Grant".to_string()),
        };
        let tx = credits.create_transaction(&request).await.expect("Failed to create AdminGrant");
        assert_eq!(tx.transaction_type, CreditTransactionType::AdminGrant);

        // Test Purchase
        let request = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::Purchase,
            amount: Decimal::from_str("50.0").unwrap(),
            description: Some("Purchase".to_string()),
        };
        let tx = credits.create_transaction(&request).await.expect("Failed to create Purchase");
        assert_eq!(tx.transaction_type, CreditTransactionType::Purchase);

        // Test Usage
        let request = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::Usage,
            amount: Decimal::from_str("25.0").unwrap(),
            description: Some("Usage".to_string()),
        };
        let tx = credits.create_transaction(&request).await.expect("Failed to create Usage");
        assert_eq!(tx.transaction_type, CreditTransactionType::Usage);

        // Test AdminRemoval
        let request = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminRemoval,
            amount: Decimal::from_str("25.0").unwrap(),
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
            description: None,
        };
        credits.create_transaction(&request1).await.expect("Failed to create transaction");

        // Try to create an invalid transaction (insufficient balance for removal)
        let request2 = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminRemoval,
            amount: Decimal::from_str("200.0").unwrap(), // More than available balance
            description: None,
        };
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
    async fn test_concurrent_transactions_no_race_condition(pool: PgPool) {
        use std::sync::Arc;
        use tokio::task;

        let user_id = create_test_user(&pool).await;

        // Create initial balance
        let mut conn: sqlx::pool::PoolConnection<sqlx::Postgres> = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);
        let initial_request = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount: Decimal::from_str("1000.0").unwrap(),
            description: Some("Initial balance".to_string()),
        };
        credits
            .create_transaction(&initial_request)
            .await
            .expect("Failed to create initial transaction");
        drop(conn);

        // Spawn 10 concurrent transactions that each add 10 credits
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

        // Verify we have exactly 11 transactions (1 initial + 10 concurrent)
        let mut conn = pool.acquire().await.expect("Failed to acquire connection");
        let mut credits = Credits::new(&mut conn);
        let transactions = credits
            .list_user_transactions(user_id, 0, 1000)
            .await
            .expect("Failed to list transactions");

        // Build a HashMap of transaction ID -> transaction for O(1) lookups
        println!("Total transactions: {}", transactions.len());

        use std::collections::HashMap;
        let tx_map: HashMap<Uuid, &CreditTransactionDBResponse> = transactions.iter().map(|tx| (tx.id, tx)).collect();

        // Find the head of the chain (the most recent transaction with no successor)
        // This is the transaction that isn't referenced as a previous_transaction_id by any other
        let mut is_previous = std::collections::HashSet::new();
        for tx in &transactions {
            if let Some(prev_id) = tx.previous_transaction_id {
                is_previous.insert(prev_id);
            }
        }

        let head = transactions
            .iter()
            .find(|tx| !is_previous.contains(&tx.id))
            .expect("Failed to find head of transaction chain");

        println!(
            "Head transaction: id={}, balance={}, type={:?}",
            head.id, head.balance_after, head.transaction_type
        );

        // Walk the chain backwards from head to tail, validating each transaction
        let mut current = Some(head);
        let mut visited = std::collections::HashSet::new();
        let mut chain_valid = true;

        while let Some(tx) = current {
            // Check for cycles
            if !visited.insert(tx.id) {
                panic!("Cycle detected in transaction chain at id={}", tx.id);
            }

            // Helpful debug output if test is failing
            // println!(
            //     "Chain: id={}, prev_id={:?}, amount={}, balance_after={}, type={:?}",
            //     tx.id, tx.previous_transaction_id, tx.amount, tx.balance_after, tx.transaction_type
            // );

            // Validate this transaction's balance based on the previous one
            if let Some(prev_id) = tx.previous_transaction_id {
                let prev_tx = tx_map
                    .get(&prev_id)
                    .unwrap_or_else(|| panic!("Previous transaction {} not found in map", prev_id));

                let expected_balance = match tx.transaction_type {
                    CreditTransactionType::AdminGrant | CreditTransactionType::Purchase => prev_tx.balance_after + tx.amount,
                    CreditTransactionType::AdminRemoval | CreditTransactionType::Usage => prev_tx.balance_after - tx.amount,
                };

                if tx.balance_after != expected_balance {
                    println!(
                        "ERROR: Transaction {} has balance {} but expected {} (prev={}, amount={}, type={:?})",
                        tx.id, tx.balance_after, expected_balance, prev_tx.balance_after, tx.amount, tx.transaction_type
                    );
                    chain_valid = false;
                }

                current = Some(prev_tx);
            } else {
                // This is the initial transaction, should have balance = amount
                assert_eq!(
                    tx.balance_after, tx.amount,
                    "Initial transaction should have balance_after == amount"
                );
                current = None;
            }
        }

        assert!(chain_valid, "Transaction chain validation failed - race condition detected!");

        // Verify final balance agrees with last transaction's balance_after
        let final_balance = credits.get_user_balance(user_id).await.expect("Failed to get balance");
        assert_eq!(
            final_balance, transactions[0].balance_after,
            "Expected {} but got {}",
            transactions[0].balance_after, final_balance
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_multiple_users_concurrent_transactions(pool: PgPool) {
        use std::sync::Arc;
        use tokio::task;

        // Create 10 users, each will have their own transaction chain
        let mut user_ids = Vec::new();
        for _ in 0..10 {
            user_ids.push(create_test_user(&pool).await);
        }

        // Create initial balance for each user
        for user_id in &user_ids {
            let mut conn = pool.acquire().await.expect("Failed to acquire connection");
            let mut credits = Credits::new(&mut conn);
            let initial_request = CreditTransactionCreateDBRequest {
                user_id: *user_id,
                transaction_type: CreditTransactionType::AdminGrant,
                amount: Decimal::from_str("1000.0").unwrap(),
                description: Some("Initial balance".to_string()),
            };
            credits
                .create_transaction(&initial_request)
                .await
                .expect("Failed to create initial transaction");
        }

        // Spawn 100 concurrent transactions (10 transactions per user)
        // This tests that different users can transact concurrently without interfering
        let pool = Arc::new(pool);
        let mut handles = vec![];

        for i in 0..1000 {
            let pool_clone = Arc::clone(&pool);
            let user_id = user_ids[i % 10]; // Distribute transactions across users
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

        // Verify each user's transaction chain independently
        for (user_idx, user_id) in user_ids.iter().enumerate() {
            println!("\n=== Validating user {} (index {}) ===", user_id, user_idx);

            let mut conn = pool.acquire().await.expect("Failed to acquire connection");
            let mut credits = Credits::new(&mut conn);
            let transactions = credits
                .list_user_transactions(*user_id, 0, 10000)
                .await
                .expect("Failed to list transactions");

            println!("User {} has {} transactions", user_id, transactions.len());
            assert_eq!(
                transactions.len(),
                101,
                "Each user should have 101 transactions (1 initial + 100 concurrent)"
            );

            // Build transaction map for this user
            use std::collections::HashMap;
            let tx_map: HashMap<Uuid, &CreditTransactionDBResponse> = transactions.iter().map(|tx| (tx.id, tx)).collect();

            // Find the head of the chain
            let mut is_previous = std::collections::HashSet::new();
            for tx in &transactions {
                if let Some(prev_id) = tx.previous_transaction_id {
                    is_previous.insert(prev_id);
                }
            }

            let head = transactions
                .iter()
                .find(|tx| !is_previous.contains(&tx.id))
                .expect("Failed to find head of transaction chain");

            // Walk the chain backwards, validating each transaction
            let mut current = Some(head);
            let mut visited = std::collections::HashSet::new();
            let mut transaction_count = 0;

            while let Some(tx) = current {
                // Check for cycles
                if !visited.insert(tx.id) {
                    panic!("Cycle detected in transaction chain for user {} at tx id={}", user_id, tx.id);
                }

                transaction_count += 1;

                // Validate this transaction's balance based on the previous one
                if let Some(prev_id) = tx.previous_transaction_id {
                    let prev_tx = tx_map
                        .get(&prev_id)
                        .expect(&format!("Previous transaction {} not found for user {}", prev_id, user_id));

                    let expected_balance = match tx.transaction_type {
                        CreditTransactionType::AdminGrant | CreditTransactionType::Purchase => prev_tx.balance_after + tx.amount,
                        CreditTransactionType::AdminRemoval | CreditTransactionType::Usage => prev_tx.balance_after - tx.amount,
                    };

                    assert_eq!(
                        tx.balance_after, expected_balance,
                        "User {} transaction {} has balance {} but expected {} (prev={}, amount={}, type={:?})",
                        user_id, tx.id, tx.balance_after, expected_balance, prev_tx.balance_after, tx.amount, tx.transaction_type
                    );

                    current = Some(prev_tx);
                } else {
                    // This is the initial transaction
                    assert_eq!(
                        tx.balance_after, tx.amount,
                        "User {} initial transaction should have balance_after == amount",
                        user_id
                    );
                    current = None;
                }
            }

            assert_eq!(
                transaction_count, 101,
                "User {} should have exactly 101 transactions in chain",
                user_id
            );

            // Verify final balance
            let final_balance = credits.get_user_balance(*user_id).await.expect("Failed to get balance");
            assert_eq!(final_balance, head.balance_after, "User {} final balance mismatch", user_id);

            println!("User {} validation complete. Final balance: {}", user_id, final_balance);
        }

        println!("\n=== All users validated successfully! ===");
    }
}
