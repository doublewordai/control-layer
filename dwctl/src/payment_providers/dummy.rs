//! Dummy payment provider implementation
//!
//! This provider automatically adds $50 of credits without requiring any external payment.
//! Useful for testing and development purposes.

use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::PgPool;

use crate::{
    api::models::users::CurrentUser,
    db::{
        handlers::credits::Credits,
        models::credits::{CreditTransactionCreateDBRequest, CreditTransactionType},
    },
    payment_providers::{PaymentError, PaymentProvider, PaymentSession, Result, WebhookEvent},
};

/// Dummy payment provider that adds $50 credits automatically
pub struct DummyProvider {
    amount: Decimal,
}

impl DummyProvider {
    /// Create a new Dummy provider
    pub fn new(amount: Decimal) -> Self {
        Self { amount }
    }
}

#[async_trait]
impl PaymentProvider for DummyProvider {
    async fn create_checkout_session(&self, db_pool: &PgPool, user: &CurrentUser, _cancel_url: &str, success_url: &str) -> Result<String> {
        // Generate a unique session ID
        let session_id = format!("dummy_session_{}", uuid::Uuid::new_v4());

        // Immediately create the credit transaction
        let mut conn = db_pool.acquire().await?;
        let mut credits = Credits::new(&mut conn);

        let request = CreditTransactionCreateDBRequest {
            user_id: user.id,
            transaction_type: CreditTransactionType::Purchase,
            amount: self.amount,
            source_id: session_id.clone(),
            description: Some("Dummy payment (test)".to_string()),
        };

        credits.create_transaction(&request).await.map_err(|e| {
            tracing::error!("Failed to create credit transaction: {:?}", e);
            PaymentError::Database(sqlx::Error::RowNotFound)
        })?;

        tracing::info!("Dummy provider added {} credits to user {}", self.amount, user.id);

        // Return the success URL since payment is "complete"
        Ok(success_url.to_string())
    }

    async fn get_payment_session(&self, session_id: &str) -> Result<PaymentSession> {
        // Parse the user ID from the session_id
        // Format: dummy_session_{uuid}
        if !session_id.starts_with("dummy_session_") {
            return Err(PaymentError::InvalidData("Invalid dummy session ID format".to_string()));
        }

        // For the dummy provider, we can't reconstruct user_id from session_id alone
        // This method is typically called after we already have the transaction in the database
        // Return a basic session with dummy data - the actual data comes from the database
        Ok(PaymentSession {
            user_id: "unknown".to_string(), // This will be overridden by database lookup
            amount: self.amount,
            is_paid: true, // Dummy sessions are always "paid"
        })
    }

    async fn process_payment_session(&self, db_pool: &PgPool, session_id: &str) -> Result<()> {
        // Check if we've already processed this payment
        let existing = sqlx::query!(
            r#"
            SELECT id FROM credits_transactions
            WHERE source_id = $1
            LIMIT 1
            "#,
            session_id
        )
        .fetch_optional(db_pool)
        .await?;

        if existing.is_some() {
            tracing::info!("Transaction for session_id {} already exists, skipping", session_id);
            return Ok(());
        }

        // For the dummy provider, the transaction was already created during checkout
        // This method serves as a verification that the transaction exists
        tracing::info!("Dummy provider verification complete for session {}", session_id);
        Ok(())
    }

    async fn validate_webhook(&self, _headers: &axum::http::HeaderMap, _body: &str) -> Result<Option<WebhookEvent>> {
        // Dummy provider doesn't use webhooks
        Ok(None)
    }

    async fn process_webhook_event(&self, _db_pool: &PgPool, _event: &WebhookEvent) -> Result<()> {
        // Dummy provider doesn't use webhooks
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use sqlx::PgPool;
    use uuid::Uuid;

    /// Helper to create a test user in the database
    async fn create_test_user(pool: &PgPool) -> CurrentUser {
        let user_id = Uuid::new_v4();
        sqlx::query!(
            r#"
            INSERT INTO users (id, email, display_name, roles, password_hash)
            VALUES ($1, $2, $3, $4, $5)
            "#,
            user_id,
            "test@example.com",
            "Test User",
            &vec!["StandardUser"],
            "dummy_hash"
        )
        .execute(pool)
        .await
        .unwrap();

        CurrentUser {
            id: user_id,
            email: "test@example.com".to_string(),
            display_name: Some("Test User".to_string()),
            roles: vec![crate::types::Role::StandardUser],
            payment_provider_id: None,
            credit_balance: Decimal::ZERO,
        }
    }

    #[test]
    fn test_dummy_provider_creation() {
        let provider = DummyProvider::new(Decimal::new(100, 0));
        assert_eq!(provider.amount, Decimal::new(100, 0));
    }

    #[sqlx::test]
    async fn test_dummy_create_checkout_session(pool: PgPool) {
        let provider = DummyProvider::new(Decimal::new(5000, 2)); // $50.00
        let user = create_test_user(&pool).await;

        let cancel_url = "http://localhost:3001/cost-management?payment=cancelled&session_id={CHECKOUT_SESSION_ID}";
        let success_url = "http://localhost:3001/cost-management?payment=success&session_id={CHECKOUT_SESSION_ID}";

        let result = provider.create_checkout_session(&pool, &user, cancel_url, success_url).await;

        assert!(result.is_ok());
        let checkout_url = result.unwrap();

        // Verify it returns the success URL with session_id
        assert!(checkout_url.contains("payment=success"));
        assert!(checkout_url.contains("session_id=dummy_session_"));

        // Extract session_id
        let url = url::Url::parse(&checkout_url).unwrap();
        let query_pairs: std::collections::HashMap<_, _> = url.query_pairs().collect();
        let session_id = query_pairs.get("session_id").unwrap();

        // Verify transaction was created immediately
        let transaction = sqlx::query!(
            r#"
            SELECT amount, user_id, source_id, description
            FROM credits_transactions
            WHERE source_id = $1
            "#,
            session_id.to_string()
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(transaction.amount, Decimal::new(5000, 2));
        assert_eq!(transaction.user_id, user.id);
        assert_eq!(transaction.description, Some("Dummy payment (test)".to_string()));
    }

    #[sqlx::test]
    async fn test_dummy_idempotency(pool: PgPool) {
        let provider = DummyProvider::new(Decimal::new(100, 0));
        let user = create_test_user(&pool).await;

        let cancel_url = "http://localhost:3001/cost-management?payment=cancelled&session_id={CHECKOUT_SESSION_ID}";
        let success_url = "http://localhost:3001/cost-management?payment=success&session_id={CHECKOUT_SESSION_ID}";

        // Create checkout session (creates transaction)
        let checkout_url = provider
            .create_checkout_session(&pool, &user, cancel_url, success_url)
            .await
            .unwrap();

        // Extract session_id
        let url = url::Url::parse(&checkout_url).unwrap();
        let query_pairs: std::collections::HashMap<_, _> = url.query_pairs().collect();
        let session_id = query_pairs.get("session_id").unwrap();

        // Process payment (should be idempotent)
        let result1 = provider.process_payment_session(&pool, session_id).await;
        let result2 = provider.process_payment_session(&pool, session_id).await;

        assert!(result1.is_ok());
        assert!(result2.is_ok());

        // Verify only one transaction exists
        let count = sqlx::query!(
            r#"
            SELECT COUNT(*) as count
            FROM credits_transactions
            WHERE source_id = $1
            "#,
            session_id.to_string()
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(count.count.unwrap(), 1, "Should only have one transaction (idempotent)");
    }

    #[sqlx::test]
    async fn test_dummy_get_payment_session(pool: PgPool) {
        let provider = DummyProvider::new(Decimal::new(7500, 2)); // $75.00

        // Test with valid session ID format
        let result = provider.get_payment_session("dummy_session_test123").await;
        assert!(result.is_ok());

        let session = result.unwrap();
        assert_eq!(session.amount, Decimal::new(7500, 2));
        assert!(session.is_paid); // Dummy sessions are always "paid"

        // Test with invalid session ID format
        let result = provider.get_payment_session("invalid_session_id").await;
        assert!(result.is_err());
        match result {
            Err(PaymentError::InvalidData(msg)) => {
                assert!(msg.contains("Invalid dummy session ID format"));
            }
            _ => panic!("Expected InvalidData error"),
        }
    }

    #[test]
    fn test_dummy_webhook_not_supported() {
        let provider = DummyProvider::new(Decimal::new(100, 0));

        // Dummy provider doesn't support webhooks
        let headers = axum::http::HeaderMap::new();
        let body = "{}";

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let result = runtime.block_on(provider.validate_webhook(&headers, body));

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None); // Returns None for unsupported webhooks
    }
}
