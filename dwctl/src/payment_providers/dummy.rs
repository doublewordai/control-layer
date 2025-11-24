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
            id: session_id.to_string(),
            user_id: "unknown".to_string(), // This will be overridden by database lookup
            amount: self.amount,
            is_paid: true, // Dummy sessions are always "paid"
            customer_id: None,
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
