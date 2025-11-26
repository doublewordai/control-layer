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
    async fn create_checkout_session(&self, _db_pool: &PgPool, user: &CurrentUser, _cancel_url: &str, success_url: &str) -> Result<String> {
        // Generate a unique session ID that includes the user ID
        // This allows us to retrieve the user ID in process_payment_session
        let session_id = format!("dummy_session_{}_{}", user.id, uuid::Uuid::new_v4());

        // Build success URL with session ID
        let redirect_url = success_url.replace("{CHECKOUT_SESSION_ID}", &session_id);

        tracing::info!("Dummy provider created checkout session {} for user {}", session_id, user.id);

        // Return the success URL - payment is instantly "complete" for dummy provider
        Ok(redirect_url)
    }

    async fn get_payment_session(&self, session_id: &str) -> Result<PaymentSession> {
        // Parse the user ID from the session_id
        // Format: dummy_session_{user_id}_{uuid}
        if !session_id.starts_with("dummy_session_") {
            return Err(PaymentError::InvalidData("Invalid dummy session ID format".to_string()));
        }

        // Extract user_id from session_id
        let parts: Vec<&str> = session_id.split('_').collect();
        if parts.len() < 4 {
            return Err(PaymentError::InvalidData("Invalid dummy session ID format".to_string()));
        }

        let user_id = parts[2];

        Ok(PaymentSession {
            user_id: user_id.to_string(),
            amount: self.amount,
            is_paid: true, // Dummy sessions are always "paid"
        })
    }

    async fn process_payment_session(&self, db_pool: &PgPool, session_id: &str) -> Result<()> {
        // Fast path: Check if we've already processed this payment
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
            tracing::trace!("Transaction for session_id {} already exists, skipping (fast path)", session_id);
            return Ok(());
        }

        // Get payment session details to extract user_id
        let payment_session = self.get_payment_session(session_id).await?;

        // Verify payment status
        if !payment_session.is_paid {
            tracing::trace!("Transaction for session_id {} has not been paid, skipping.", session_id);
            return Err(PaymentError::PaymentNotCompleted);
        }

        // Create the credit transaction
        let mut conn = db_pool.acquire().await?;
        let mut credits = Credits::new(&mut conn);

        let user_id: crate::types::UserId = payment_session.user_id.parse().map_err(|e| {
            tracing::error!("Failed to parse user ID: {:?}", e);
            PaymentError::InvalidData(format!("Invalid user ID: {}", e))
        })?;

        let request = CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: CreditTransactionType::Purchase,
            amount: payment_session.amount,
            source_id: session_id.to_string(),
            description: Some("Dummy payment (test)".to_string()),
        };

        match credits.create_transaction(&request).await {
            Ok(_) => {
                tracing::info!("Successfully fulfilled checkout session {} for user {}", session_id, user_id);
                Ok(())
            }
            Err(crate::db::errors::DbError::UniqueViolation { constraint, .. }) => {
                // Check if this is a unique constraint violation on source_id
                if constraint.as_deref() == Some("credits_transactions_source_id_unique") {
                    tracing::trace!(
                        "Transaction for session_id {} already processed (caught unique constraint violation), returning success (idempotent)",
                        session_id
                    );
                    Ok(())
                } else {
                    tracing::error!("Unexpected unique constraint violation: {:?}", constraint);
                    Err(PaymentError::Database(sqlx::Error::RowNotFound))
                }
            }
            Err(e) => {
                tracing::error!("Failed to create credit transaction: {:?}", e);
                Err(PaymentError::Database(sqlx::Error::RowNotFound))
            }
        }
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
    use crate::api::models::users::Role;
    use rust_decimal::Decimal;
    use sqlx::PgPool;

    /// Helper to create a test user in the database
    async fn create_test_user(pool: &PgPool) -> CurrentUser {
        let user = crate::test_utils::create_test_user(pool, Role::StandardUser).await;

        CurrentUser {
            id: user.id,
            username: user.username,
            email: user.email,
            display_name: user.display_name,
            roles: user.roles,
            payment_provider_id: None,
            is_admin: false,
            avatar_url: None,
        }
    }

    #[test]
    fn test_dummy_provider_creation() {
        let provider = DummyProvider::new(Decimal::new(100, 0));
        assert_eq!(provider.amount, Decimal::new(100, 0));
    }

    #[sqlx::test]
    async fn test_dummy_full_payment_flow(pool: PgPool) {
        let provider = DummyProvider::new(Decimal::new(5000, 2)); // $50.00
        let user = create_test_user(&pool).await;

        let cancel_url = "http://localhost:3001/cost-management?payment=cancelled&session_id={CHECKOUT_SESSION_ID}";
        let success_url = "http://localhost:3001/cost-management?payment=success&session_id={CHECKOUT_SESSION_ID}";

        // Step 1: Create checkout session
        let checkout_url = provider
            .create_checkout_session(&pool, &user, cancel_url, success_url)
            .await
            .unwrap();

        // Verify it returns the success URL with session_id
        assert!(checkout_url.contains("payment=success"));
        assert!(checkout_url.contains(&format!("session_id=dummy_session_{}", user.id)));

        // Extract session_id (simulating frontend receiving redirect)
        let url = url::Url::parse(&checkout_url).unwrap();
        let query_pairs: std::collections::HashMap<_, _> = url.query_pairs().collect();
        let session_id = query_pairs.get("session_id").unwrap();

        // Verify NO transaction was created yet (matches Stripe flow)
        let count_before = sqlx::query!(
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
        assert_eq!(count_before.count.unwrap(), 0, "Transaction should not exist before processing");

        // Step 2: Frontend calls backend to process payment
        let result = provider.process_payment_session(&pool, session_id).await;
        assert!(result.is_ok(), "Payment processing should succeed");

        // Step 3: Verify transaction was created
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

        // Create checkout session
        let checkout_url = provider
            .create_checkout_session(&pool, &user, cancel_url, success_url)
            .await
            .unwrap();

        // Extract session_id
        let url = url::Url::parse(&checkout_url).unwrap();
        let query_pairs: std::collections::HashMap<_, _> = url.query_pairs().collect();
        let session_id = query_pairs.get("session_id").unwrap();

        // Process payment multiple times (simulating retries, webhook + manual, etc.)
        let result1 = provider.process_payment_session(&pool, session_id).await;
        let result2 = provider.process_payment_session(&pool, session_id).await;
        let result3 = provider.process_payment_session(&pool, session_id).await;

        assert!(result1.is_ok());
        assert!(result2.is_ok());
        assert!(result3.is_ok());

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
