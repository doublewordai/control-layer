//! Dummy payment provider implementation
//!
//! This provider automatically adds credits without requiring any external payment.
//! Useful for testing and development purposes.

use async_trait::async_trait;
use sqlx::PgPool;

use crate::{
    api::models::users::CurrentUser,
    db::{
        handlers::{credits::Credits, repository::Repository},
        models::credits::{CreditTransactionCreateDBRequest, CreditTransactionType},
    },
    payment_providers::{PaymentError, PaymentProvider, PaymentSession, Result, WebhookEvent},
};

/// Dummy payment provider that adds credits automatically
pub struct DummyProvider {
    config: crate::config::DummyConfig,
}

impl From<crate::config::DummyConfig> for DummyProvider {
    fn from(config: crate::config::DummyConfig) -> Self {
        Self { config }
    }
}

#[async_trait]
impl PaymentProvider for DummyProvider {
    async fn create_checkout_session(
        &self,
        _db_pool: &PgPool,
        user: &CurrentUser,
        creditee_id: Option<&str>,
        _cancel_url: &str,
        success_url: &str,
    ) -> Result<String> {
        // Determine which user will receive the credits
        // If creditee_id is provided, use that; otherwise use the authenticated user
        let user_id_string = user.id.to_string();
        let recipient_id = creditee_id.unwrap_or(&user_id_string);

        // Generate a unique session ID that includes both payer and recipient user IDs
        // Format: dummy_session_{recipient_id}_{payer_id}_{uuid}
        let session_id = format!("dummy_session_{}_{}_{}", recipient_id, user.id, uuid::Uuid::new_v4());

        // Build success URL with session ID
        let redirect_url = success_url.replace("{CHECKOUT_SESSION_ID}", &session_id);

        tracing::info!(
            "Dummy provider created checkout session {} for user {} (payer: {})",
            session_id,
            recipient_id,
            user.id
        );

        // Return the success URL - payment is instantly "complete" for dummy provider
        Ok(redirect_url)
    }

    async fn get_payment_session(&self, session_id: &str) -> Result<PaymentSession> {
        // Parse the user IDs from the session_id
        // Format: dummy_session_{recipient_id}_{payer_id}_{uuid}
        if !session_id.starts_with("dummy_session_") {
            return Err(PaymentError::InvalidData("Invalid dummy session ID format".to_string()));
        }

        // Extract recipient_id and payer_id from session_id
        let parts: Vec<&str> = session_id.split('_').collect();
        if parts.len() < 5 {
            return Err(PaymentError::InvalidData("Invalid dummy session ID format".to_string()));
        }

        // Parse IDs directly from parts
        let creditee_id: crate::types::UserId = parts[2].parse().map_err(|e| {
            tracing::error!("Failed to parse recipient ID: {:?}", e);
            PaymentError::InvalidData(format!("Invalid recipient user ID: {}", e))
        })?;

        let creditor_id: crate::types::UserId = parts[3].parse().map_err(|e| {
            tracing::error!("Failed to parse payer ID: {:?}", e);
            PaymentError::InvalidData(format!("Invalid payer user ID: {}", e))
        })?;

        Ok(PaymentSession {
            creditee_id,
            creditor_id,
            amount: self.config.amount,
            is_paid: true, // Dummy sessions are always "paid"
            payment_provider_id: Some(parts[3].to_string()),
        })
    }

    async fn process_payment_session(&self, db_pool: &PgPool, session_id: &str) -> Result<()> {
        // Acquire connection early for idempotency check
        let mut conn = db_pool.acquire().await?;

        // Fast path: Check if we've already processed this payment
        {
            let mut credits = Credits::new(&mut conn);
            if credits.transaction_exists_by_source_id(session_id).await? {
                tracing::trace!("Transaction for session_id {} already exists, skipping (fast path)", session_id);
                return Ok(());
            }
        }

        // Get payment session details to extract user_id
        let payment_session = self.get_payment_session(session_id).await?;

        // Verify payment status
        if !payment_session.is_paid {
            tracing::trace!("Transaction for session_id {} has not been paid, skipping.", session_id);
            return Err(PaymentError::PaymentNotCompleted);
        }

        // Build description with creditor information (same pattern as Stripe)
        let description = {
            let mut users = crate::db::handlers::users::Users::new(&mut conn);

            // Verify creditor user exists
            let creditor_user = users.get_by_id(payment_session.creditor_id).await?;
            if creditor_user.is_none() {
                tracing::error!(
                    "Creditor user {} not found for payment session {}. This indicates a data integrity issue.",
                    payment_session.creditor_id,
                    session_id
                );
            }

            // Build description with payer information
            if payment_session.creditor_id == payment_session.creditee_id {
                // Self-payment
                "Dummy payment (test)".to_string()
            } else if let Some(creditor) = creditor_user.as_ref() {
                let creditor_name = creditor.display_name.as_ref().unwrap_or(&creditor.email);
                format!("Dummy payment (test) from {}", creditor_name)
            } else {
                "Dummy payment (test)".to_string()
            }
        };

        let request = CreditTransactionCreateDBRequest {
            user_id: payment_session.creditee_id,
            transaction_type: CreditTransactionType::Purchase,
            amount: payment_session.amount,
            source_id: session_id.to_string(),
            description: Some(description),
            fusillade_batch_id: None,
        };

        let mut credits = Credits::new(&mut conn);
        credits.create_transaction(&request).await?;

        tracing::info!("Successfully fulfilled checkout session {} for user {}", session_id, payment_session.creditee_id);
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
    use crate::api::models::users::Role;
    use rust_decimal::Decimal;
    use sqlx::PgPool;

    /// Helper to create a test user in the database
    async fn create_test_user(pool: &PgPool) -> CurrentUser {
        let user = crate::test::utils::create_test_user(pool, Role::StandardUser).await;

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
    fn test_dummy_provider_from_config() {
        let config = crate::config::DummyConfig {
            amount: Decimal::new(100, 0),
            host_url: None,
        };
        let provider = DummyProvider::from(config);
        assert_eq!(provider.config.amount, Decimal::new(100, 0));
    }

    #[sqlx::test]
    async fn test_dummy_full_payment_flow(pool: PgPool) {
        let config = crate::config::DummyConfig {
            amount: Decimal::new(5000, 2), // $50.00
            host_url: None,
        };
        let provider = DummyProvider::from(config);
        let user = create_test_user(&pool).await;

        let cancel_url = "http://localhost:3001/cost-management?payment=cancelled&session_id={CHECKOUT_SESSION_ID}";
        let success_url = "http://localhost:3001/cost-management?payment=success&session_id={CHECKOUT_SESSION_ID}";

        // Step 1: Create checkout session
        let checkout_url = provider
            .create_checkout_session(&pool, &user, None, cancel_url, success_url)
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
        let config = crate::config::DummyConfig {
            amount: Decimal::new(100, 0),
            host_url: None,
        };
        let provider = DummyProvider::from(config);
        let user = create_test_user(&pool).await;

        let cancel_url = "http://localhost:3001/cost-management?payment=cancelled&session_id={CHECKOUT_SESSION_ID}";
        let success_url = "http://localhost:3001/cost-management?payment=success&session_id={CHECKOUT_SESSION_ID}";

        // Create checkout session
        let checkout_url = provider
            .create_checkout_session(&pool, &user, None, cancel_url, success_url)
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
        let config = crate::config::DummyConfig {
            amount: Decimal::new(100, 0),
            host_url: None,
        };
        let provider = DummyProvider::from(config);

        // Dummy provider doesn't support webhooks
        let headers = axum::http::HeaderMap::new();
        let body = "{}";

        let runtime = tokio::runtime::Runtime::new().unwrap();
        let result = runtime.block_on(provider.validate_webhook(&headers, body));

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), None); // Returns None for unsupported webhooks
    }
}
