//! Stripe payment provider implementation
//!
//! The Stripe API calls and webhook verification live in the `dwctl-stripe`
//! crate, which hands back plain types. This module holds what dwctl does with
//! them: persistence, idempotency and the crediting and verification rules.

use async_trait::async_trait;
use dwctl_stripe::{
    CheckoutMode, CheckoutSession, DeclineKind, Error as StripeError, Payer, PaymentCheckout, SetupCheckout, SetupPurpose, StripeApi,
    StripeClient, verify_webhook,
};
use rust_decimal::Decimal;
use sqlx::{Connection, PgPool};

use crate::{
    db::{
        handlers::{credits::Credits, repository::Repository},
        models::credits::{CreditTransactionCreateDBRequest, CreditTransactionType},
    },
    payment_providers::{
        AutoTopupDeclineKind, AutoTopupSetupResult, CheckoutPayer, PaymentError, PaymentProvider, PaymentSession, Result, WebhookEvent,
    },
    types::UserId,
};

impl From<StripeError> for PaymentError {
    fn from(error: StripeError) -> Self {
        match error {
            StripeError::Api(message) => PaymentError::ProviderApi(message),
            StripeError::InvalidData(message) => PaymentError::InvalidData(message),
            StripeError::AlreadyProcessed => PaymentError::AlreadyProcessed,
            StripeError::Declined(DeclineKind::Soft) => PaymentError::AutoTopupDeclined(AutoTopupDeclineKind::Soft),
            StripeError::Declined(DeclineKind::Hard) => PaymentError::AutoTopupDeclined(AutoTopupDeclineKind::Hard),
        }
    }
}

/// Stripe payment provider
pub struct StripeProvider {
    config: crate::config::StripeConfig,
    client: StripeClient,
}

impl From<crate::config::StripeConfig> for StripeProvider {
    fn from(config: crate::config::StripeConfig) -> Self {
        // `Config::validate` rejects a bad base URL at startup.
        let api_base = config.api_base().expect("Stripe api_base_url is validated at config load");
        let client = StripeClient::new(&config.api_key, api_base).expect("invalid Stripe API key or base URL");
        Self { config, client }
    }
}

fn stripe_payer<'a>(payer: &'a CheckoutPayer, reference_id: &'a str) -> Payer<'a> {
    Payer {
        reference_id,
        email: &payer.email,
        customer_id: payer.payment_provider_id.as_deref(),
    }
}

impl StripeProvider {
    /// Fulfil a completed `setup`-mode session: save the payment method as the
    /// customer's default, then, if it carries a fingerprint that no other
    /// account has verified with, mark the billing target verified and pay out
    /// signup credits.
    ///
    /// When the instrument is held by another account (or has no fingerprint),
    /// a verification session fails and an auto top-up enrolment returns `Ok`
    /// with only the payment method saved.
    ///
    /// No money moved, so there is no purchase to record - the only ledger entry
    /// is the (optional, once-per-target) verification grant.
    async fn fulfil_setup_session(
        &self,
        conn: &mut sqlx::PgConnection,
        session: &CheckoutSession,
        session_id: &str,
        credits_config: &crate::config::CreditsConfig,
    ) -> Result<()> {
        if !session.complete {
            tracing::trace!("Setup session {} is not complete yet, skipping", session_id);
            return Err(PaymentError::PaymentNotCompleted);
        }

        // Who was being verified. Set from `payer.id` at creation, i.e. the
        // resolved billing target (the org when acting as an org, else self).
        let target_id: UserId = session
            .client_reference_id
            .as_deref()
            .ok_or_else(|| {
                tracing::error!("Setup session {} missing client_reference_id", session_id);
                PaymentError::InvalidData("Missing client_reference_id".to_string())
            })?
            .parse()
            .map_err(|e| {
                tracing::error!("Failed to parse setup session target ID: {:?}", e);
                PaymentError::InvalidData(format!("Invalid target user ID: {}", e))
            })?;

        let customer_id = session.customer_id.as_deref();

        let Some(setup_intent) = &session.setup_intent else {
            tracing::error!("Setup session {} has no expanded setup_intent", session_id);
            return Err(PaymentError::InvalidData("Setup intent not found or not expanded".to_string()));
        };

        // A complete session whose SetupIntent didn't succeed means the card was
        // not actually verified - never treat that as proof of payment ability.
        if setup_intent.status != "succeeded" {
            tracing::warn!(
                session_id,
                status = setup_intent.status.as_str(),
                "Setup session completed but the SetupIntent did not succeed"
            );
            return Err(PaymentError::InvalidData("Payment method setup failed".to_string()));
        }

        // Checkout attaches the payment method but doesn't make it the default;
        // auto top-up charges look it up via invoice_settings, so set it here.
        // Best-effort: the card is verified either way, and
        // `get_default_payment_method` falls back to listing attached methods.
        if let (Some(cust_id), Some(pm_id)) = (customer_id, setup_intent.payment_method_id.as_deref()) {
            self.client.set_default_payment_method(cust_id, pm_id).await;
        }

        {
            let mut users = crate::db::handlers::users::Users::new(&mut *conn);

            if users.get_by_id(target_id).await?.is_none() {
                tracing::error!(
                    "Target user {} not found for setup session {}. This indicates a data integrity issue.",
                    target_id,
                    session_id
                );
                return Err(PaymentError::InvalidData("Setup session target user not found".to_string()));
            }

            // Stripe may have created the customer during checkout; persist it so
            // the billing portal and auto top-up can find it later.
            if let Some(provider_id) = customer_id
                && users.set_payment_provider_id_if_empty(target_id, provider_id).await?
            {
                tracing::debug!("Saved newly created stripe ID {} for user ID {}", provider_id, target_id);
            }
        }

        // One instrument verifies one account. Stripe's fingerprint identifies
        // the card or bank account across customers, and is only present on
        // the expanded payment method (`get_session_for_processing` requests
        // it). The first account to complete a setup session with an
        // instrument claims it; any other account presenting the same
        // instrument is not verified and gets no credits, whatever the session
        // was for. The instrument itself stays attached to their customer and
        // can pay for top-ups.
        let purpose = setup_intent.purpose;
        // No fingerprint means the claim cannot be made, so verification
        // cannot be granted: fail closed. The verification Checkout only
        // offers cards, which always carry one, so this is a malformed or
        // unexpanded session (or an auto top-up enrolment with Link or SEPA,
        // which is saved but does not verify).
        let Some(fingerprint) = setup_intent.fingerprint.as_deref() else {
            tracing::warn!(
                session_id,
                target_id = %target_id,
                purpose = purpose.as_str(),
                "Setup session payment method has no fingerprint; not verifying"
            );
            return match purpose {
                SetupPurpose::Verification => Err(PaymentError::InvalidData(
                    "Payment method has no fingerprint; verification requires a card".to_string(),
                )),
                SetupPurpose::AutoTopup => Ok(()),
            };
        };
        // The claim and the verified flag land together: a claim without the
        // flag would leave the account holding an instrument it was never
        // verified by, and a concurrent second claimant waits on this commit
        // rather than on a bare insert. A losing claim is rolled back, though
        // there is nothing of ours to undo in that case.
        let mut tx = conn.begin().await?;
        let owner = Credits::new(&mut tx).claim_verification_instrument(fingerprint, target_id).await?;
        let claimed_by_other = (owner != target_id).then_some(owner);
        if let Some(owner) = claimed_by_other {
            tx.rollback().await?;
            tracing::warn!(
                session_id,
                target_id = %target_id,
                owner = %owner,
                purpose = purpose.as_str(),
                "Payment instrument has already verified another account; not verifying this one"
            );
            return match purpose {
                // The user asked to be verified and was not: tell them.
                SetupPurpose::Verification => Err(PaymentError::InstrumentAlreadyUsed),
                // Enrolment succeeded (the instrument is saved and default);
                // only the verification side effects are withheld.
                SetupPurpose::AutoTopup => Ok(()),
            };
        }

        // A verified card clears the unverified rate-limit tier in onwards,
        // same as a completed purchase does.
        crate::db::handlers::users::Users::new(&mut tx).set_verified(target_id).await?;
        tx.commit().await?;

        // Signup credits. Best-effort by the same reasoning as the first-payment
        // match: verification is the thing that must stick, and a failed freebie
        // is not worth undoing it. This path isn't retried once verification has
        // landed, so the error log is the signal to grant manually.
        if let Err(e) = Credits::new(&mut *conn)
            .grant_verification_credits(credits_config.verification_credits, target_id, session_id)
            .await
        {
            tracing::error!(
                session_id,
                target_id = %target_id,
                error = %e,
                "Verification credits grant failed; card verification unaffected, grant manually if needed"
            );
        }

        tracing::debug!("Successfully fulfilled setup session {} for user {}", session_id, target_id);
        Ok(())
    }
}

/// Parse a retrieved `payment`-mode checkout session into a `PaymentSession`.
///
/// Expects `line_items` expanded (falls back to the session-level subtotal).
fn parse_payment_session(checkout_session: &CheckoutSession) -> Result<PaymentSession> {
    // Parse creditor ID from client_reference_id
    let creditor_id: UserId = checkout_session
        .client_reference_id
        .as_deref()
        .ok_or_else(|| {
            tracing::error!("Checkout session missing client_reference_id");
            PaymentError::InvalidData("Missing client_reference_id".to_string())
        })?
        .parse()
        .map_err(|e| {
            tracing::error!("Failed to parse creditor ID: {:?}", e);
            PaymentError::InvalidData(format!("Invalid creditor user ID: {}", e))
        })?;

    // Parse creditee ID from metadata, or use creditor_id if not present (self-payment)
    let creditee_id: UserId = checkout_session
        .creditee_id
        .as_ref()
        .map(|s| s.parse())
        .transpose()
        .map_err(|e| {
            tracing::error!("Failed to parse creditee ID: {:?}", e);
            PaymentError::InvalidData(format!("Invalid creditee user ID: {}", e))
        })?
        .unwrap_or(creditor_id);

    // The pre-tax subtotal: see `pretax_credit_cents` in dwctl-stripe.
    let cents = checkout_session.pretax_cents.ok_or_else(|| {
        tracing::error!("Checkout session missing both line_items and amount_subtotal");
        PaymentError::InvalidData("Missing payment amount".to_string())
    })?;

    // Scale the cents rather than dividing: integer division truncated the
    // remainder, so a $10.50 checkout credited $10 and silently pocketed the
    // 50c. Credits are stored as DECIMAL, so there's no reason to lose them.
    let amount = Decimal::new(cents, 2);

    Ok(PaymentSession {
        creditee_id,
        amount,
        is_paid: checkout_session.paid,
        creditor_id,
        payment_provider_id: checkout_session.customer_id.clone(),
    })
}

#[async_trait]
impl PaymentProvider for StripeProvider {
    async fn create_checkout_session(
        &self,
        payer: &CheckoutPayer,
        creditee_id: Option<&str>,
        cancel_url: &str,
        success_url: &str,
    ) -> Result<String> {
        let payer_id = payer.id.to_string();
        Ok(self
            .client
            .create_payment_checkout_session(PaymentCheckout {
                payer: stripe_payer(payer, &payer_id),
                creditee_id,
                cancel_url,
                success_url,
                price_id: &self.config.price_id,
                invoice_creation: self.config.enable_invoice_creation,
            })
            .await?)
    }

    async fn get_payment_session(&self, session_id: &str) -> Result<PaymentSession> {
        let checkout_session = self.client.get_payment_session(session_id).await?;

        parse_payment_session(&checkout_session)
    }

    async fn process_payment_session(
        &self,
        db_pool: &PgPool,
        session_id: &str,
        credits_config: &crate::config::CreditsConfig,
    ) -> Result<()> {
        // Acquire connection early for idempotency check
        let mut conn = db_pool.acquire().await?;

        // Fast path: Check if we've already processed this payment
        // This avoids expensive Stripe API calls for duplicate webhook deliveries,
        // user retries, etc. The unique constraint below handles race conditions.
        {
            let mut credits = Credits::new(&mut conn);
            if credits.transaction_exists_by_source_id(session_id).await? {
                tracing::trace!("Transaction for session_id {} already exists, skipping (fast path)", session_id);
                return Ok(());
            }
        }

        // One retrieve, then dispatch on mode. Callers hand us a bare session id
        // (the front-channel `PATCH /payments/{id}` and `checkout.session.completed`
        // webhooks alike) and cannot tell a top-up from a card verification, so the
        // mode has to be read off the session itself.
        let session = self.client.get_session_for_processing(session_id).await?;

        if session.mode == CheckoutMode::Setup {
            return self.fulfil_setup_session(&mut conn, &session, session_id, credits_config).await;
        }

        let payment_session = parse_payment_session(&session)?;

        // Verify payment status
        if !payment_session.is_paid {
            tracing::trace!("Transaction for session_id {} has not been paid, skipping.", session_id);
            return Err(PaymentError::PaymentNotCompleted);
        }

        // Look up creditor user and build description + set creditor stripe ID in db.
        // This is one block to scope user repo lifetime properly
        let description = {
            let mut users = crate::db::handlers::users::Users::new(&mut conn);

            // Bail if either party is unknown to this plane.
            //
            // With one Stripe account serving both regional planes, every plane
            // receives every plane's events, so unknown users are the *expected*
            // shape of a foreign region's session rather than a rarity.
            //
            // The creditee is checked first because it is the one that gets
            // written: the credit insert below uses it as user_id, and a row
            // that does not exist fails the foreign key. Today that surfaces as
            // DbError -> InvalidData, which the webhook's catch-all already
            // turns into a 200 while logging at ERROR — so the present symptom
            // is a misleading error-level log and a wasted Stripe round trip
            // rather than a retry storm. Relying on that is still wrong: it
            // depends on a constraint rather than a decision, it would credit a
            // nonexistent user if the constraint were ever relaxed, and it
            // buries a routine multi-region event in the error stream.
            //
            // The creditor is checked too, because it is read for the
            // description and written by set_verified further down.
            //
            // A genuinely orphaned local session is indistinguishable from here
            // and gets the same treatment; the webhook handler's log line is
            // what keeps it visible.
            if users.get_by_id(payment_session.creditee_id).await?.is_none() {
                return Err(PaymentError::UnknownReference(payment_session.creditee_id.to_string()));
            }
            let creditor_user = users.get_by_id(payment_session.creditor_id).await?;
            if creditor_user.is_none() {
                return Err(PaymentError::UnknownReference(payment_session.creditor_id.to_string()));
            }

            // Build description with payer information
            let description = if payment_session.creditor_id == payment_session.creditee_id {
                // Self-payment
                "Stripe payment".to_string()
            } else if let Some(creditor) = creditor_user.as_ref() {
                let creditor_name = creditor.display_name.as_ref().unwrap_or(&creditor.email);
                format!("Stripe payment from {}", creditor_name)
            } else {
                "Stripe payment".to_string()
            };

            // Save the customer ID if we don't have one yet, so we can offer the billing portal
            if let Some(ref provider_id) = payment_session.payment_provider_id
                && users
                    .set_payment_provider_id_if_empty(payment_session.creditor_id, provider_id)
                    .await?
            {
                tracing::debug!(
                    "Saved newly created stripe ID {} for user ID {}",
                    provider_id,
                    payment_session.creditor_id
                );
            }

            description
        };

        // Create the credit transaction
        let request = CreditTransactionCreateDBRequest {
            user_id: payment_session.creditee_id,
            transaction_type: CreditTransactionType::Purchase,
            amount: payment_session.amount,
            source_id: session_id.to_string(),
            description: Some(description),
            fusillade_batch_id: None,
            api_key_id: None,
        };

        // Record the purchase first. This is the critical write (real money moved)
        // and is never made contingent on the secondary effects below.
        let mut credits = Credits::new(&mut conn);
        credits.create_transaction(&request).await?;

        // First-payment match (no-op unless enabled and this is the payee's first
        // ever payment). The bonus lands on the creditee (whose balance was just
        // topped up), deliberately the credited account rather than the payer we
        // verify below, since the promo rewards whoever receives the credits.
        // Best-effort: a freebie must never undo the recorded purchase, so we log
        // and continue on failure rather than failing payment processing. Note
        // such a failure is not retried (the webhook retry's fast-path sees the
        // purchase already recorded), so the error log is the signal to grant it
        // manually.
        if let Err(e) = credits
            .grant_first_payment_match(
                credits_config.first_payment_match_up_to,
                payment_session.creditee_id,
                payment_session.amount,
                session_id,
            )
            .await
        {
            tracing::error!(session_id, creditee_id = %payment_session.creditee_id, error = %e, "First-payment match failed; purchase unaffected, grant manually if needed");
        }

        // Real money moved: mark the payer as verified for the onwards rate-limit
        // tier. `creditor_id` is the resolved billing target (org when paying as an
        // org, otherwise self), so this naturally verifies whichever entity owns
        // the keys we care about in the common case. For the rare admin
        // pay-on-behalf flow (explicit `creditee_id` query param) the payer is
        // verified rather than the recipient, which we accept as the right
        // semantic for "this entity can pay".
        crate::db::handlers::users::Users::new(&mut conn)
            .set_verified(payment_session.creditor_id)
            .await?;

        tracing::debug!(
            "Successfully fulfilled checkout session {} for user {}",
            session_id,
            payment_session.creditee_id
        );
        Ok(())
    }

    async fn validate_webhook(&self, headers: &axum::http::HeaderMap, body: &str) -> Result<Option<WebhookEvent>> {
        // Get the Stripe signature from headers
        let signature = headers
            .get("stripe-signature")
            .ok_or_else(|| {
                tracing::error!("Missing stripe-signature header");
                PaymentError::InvalidData("Missing stripe-signature header".to_string())
            })?
            .to_str()
            .map_err(|e| {
                tracing::error!("Invalid stripe-signature header: {:?}", e);
                PaymentError::InvalidData("Invalid stripe-signature header".to_string())
            })?;

        // Validate the webhook signature and convert the Stripe event to our
        // generic WebhookEvent
        let event = verify_webhook(body, signature, &self.config.webhook_secret)?;

        Ok(Some(WebhookEvent {
            event_type: event.event_type,
            session_id: event.session_id,
        }))
    }

    async fn process_webhook_event(
        &self,
        db_pool: &PgPool,
        event: &WebhookEvent,
        credits_config: &crate::config::CreditsConfig,
    ) -> Result<()> {
        // Only process checkout session completion events — ignore all others silently.
        // Stripe may send events like charge.updated, payment_intent.succeeded, etc.
        // that we don't need to act on.
        if event.event_type != "checkout.session.completed" && event.event_type != "checkout.session.async_payment_succeeded" {
            tracing::trace!("Ignoring webhook event type: {}", event.event_type);
            return Ok(());
        }

        // Extract session ID
        let session_id = event.session_id.as_ref().ok_or_else(|| {
            tracing::error!("Webhook event missing session_id");
            PaymentError::InvalidData("Missing session_id in webhook event".to_string())
        })?;

        tracing::trace!("Processing webhook event {} for session: {}", event.event_type, session_id);

        // Use the existing process_payment_session method
        self.process_payment_session(db_pool, session_id, credits_config).await
    }

    async fn create_setup_checkout_session(&self, payer: &CheckoutPayer, cancel_url: &str, success_url: &str) -> Result<String> {
        let payer_id = payer.id.to_string();
        Ok(self
            .client
            .create_setup_checkout_session(SetupCheckout {
                payer: stripe_payer(payer, &payer_id),
                cancel_url,
                success_url,
                purpose: SetupPurpose::Verification,
                terms_of_service_text: self
                    .config
                    .setup_terms_of_service_text
                    .as_deref()
                    .or(self.config.auto_topup_terms_of_service_text.as_deref()),
            })
            .await?)
    }

    async fn create_auto_topup_checkout_session(&self, payer: &CheckoutPayer, cancel_url: &str, success_url: &str) -> Result<String> {
        let payer_id = payer.id.to_string();
        Ok(self
            .client
            .create_setup_checkout_session(SetupCheckout {
                payer: stripe_payer(payer, &payer_id),
                cancel_url,
                success_url,
                purpose: SetupPurpose::AutoTopup,
                terms_of_service_text: self.config.auto_topup_terms_of_service_text.as_deref(),
            })
            .await?)
    }

    async fn process_auto_topup_session(&self, _db_pool: &PgPool, session_id: &str) -> Result<AutoTopupSetupResult> {
        let session = self.client.get_setup_session(session_id).await?;

        // Check if setup was completed
        if !session.complete {
            return Err(PaymentError::PaymentNotCompleted);
        }

        // The expanded SetupIntent
        let Some(setup_intent) = &session.setup_intent else {
            return Err(PaymentError::InvalidData("Setup intent not found or not expanded".to_string()));
        };

        // Check if the SetupIntent succeeded
        if setup_intent.status != "succeeded" {
            return Err(PaymentError::InvalidData("Payment method setup failed".to_string()));
        }

        // Set the payment method as the customer's default for invoices,
        // so get_default_payment_method can find it later for auto top-up charges.
        // Checkout setup mode attaches the PM but doesn't set it as the default.
        // Non-fatal on failure: the payment method is still attached.
        if let (Some(cust_id), Some(pm_id)) = (session.customer_id.as_deref(), setup_intent.payment_method_id.as_deref()) {
            self.client.set_default_payment_method(cust_id, pm_id).await;
        }

        Ok(AutoTopupSetupResult {
            // May be newly created by Stripe during checkout
            customer_id: session.customer_id,
            user_id: session.client_reference_id,
        })
    }

    async fn accrue_invoice_item(&self, amount_cents: i64, customer_id: &str, description: &str, idempotency_key: &str) -> Result<String> {
        Ok(self
            .client
            .accrue_invoice_item(amount_cents, customer_id, description, idempotency_key)
            .await?)
    }

    async fn charge_auto_topup(
        &self,
        amount_cents: i64,
        customer_id: &str,
        payment_method_id: &str,
        idempotency_key: &str,
    ) -> Result<String> {
        Ok(self
            .client
            .charge_auto_topup(
                amount_cents,
                customer_id,
                payment_method_id,
                idempotency_key,
                self.config.tax_code.as_deref(),
            )
            .await?)
    }

    async fn get_default_payment_method(&self, customer_id: &str) -> Result<Option<String>> {
        Ok(self.client.get_default_payment_method(customer_id).await?)
    }

    async fn customer_has_address(&self, customer_id: &str) -> Result<bool> {
        Ok(self.client.customer_has_address(customer_id).await?)
    }

    async fn create_customer(&self, email: &str, name: Option<&str>) -> Result<String> {
        Ok(self.client.create_customer(email, name).await?)
    }

    async fn create_billing_portal_session(&self, customer_id: &str, return_url: &str) -> Result<String> {
        Ok(self.client.create_billing_portal_session(customer_id, return_url).await?)
    }
}

#[cfg(test)]
mod mock_api_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use sqlx::PgPool;
    use uuid::Uuid;

    /// Helper to create a test user in the database
    async fn create_test_user(pool: &PgPool) -> Uuid {
        let user = crate::test::utils::create_test_user(pool, crate::api::models::users::Role::StandardUser).await;
        user.id
    }

    #[test]
    fn test_instrument_already_used_is_a_conflict_for_direct_callers() {
        // The front-channel caller must learn that this card cannot verify
        // them; the webhook acks it separately so Stripe stops redelivering.
        let status: axum::http::StatusCode = PaymentError::InstrumentAlreadyUsed.into();
        assert_eq!(status, axum::http::StatusCode::CONFLICT);
    }

    // The whole point of COR-594: a foreign region's event must not be retried
    // forever. Stripe fans account-level events out to every endpoint, so with
    // one account and two planes each sees the other's sessions; anything but a
    // 2xx here is an infinite retry loop, because retrying cannot make a user
    // this plane does not own appear.
    #[test]
    fn test_unknown_reference_is_not_found_for_direct_callers() {
        // The webhook acks these explicitly in its own arm. This mapping serves
        // the front-channel PATCH /payments/{id}, where claiming success for a
        // session we cannot process would mislead the caller.
        let status: axum::http::StatusCode = PaymentError::UnknownReference("some-user-id".to_string()).into();
        assert_eq!(
            status,
            axum::http::StatusCode::NOT_FOUND,
            "direct callers must be told the session is not ours"
        );
    }

    // The other half of the same guarantee. Widening the ack to cover genuine
    // database failures would silently drop real payments, so the distinction
    // between "not ours" and "we could not process it" has to hold.
    #[test]
    fn test_database_errors_are_still_retried() {
        let status: axum::http::StatusCode = PaymentError::Database(sqlx::Error::PoolClosed).into();
        assert_eq!(
            status,
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "transient database failures must keep asking Stripe to retry"
        );
    }

    /// The Stripe crate classifies API failures; this is where they become
    /// the provider-neutral errors the handlers and the auto top-up loop match on.
    #[test]
    fn test_stripe_errors_map_onto_payment_errors() {
        // Another replica is mid-charge on the same key: a no-op, not a failure.
        let err = PaymentError::from(StripeError::AlreadyProcessed);
        assert!(matches!(err, PaymentError::AlreadyProcessed), "got {err:?}");

        let err = PaymentError::from(StripeError::Declined(DeclineKind::Hard));
        assert!(
            matches!(err, PaymentError::AutoTopupDeclined(AutoTopupDeclineKind::Hard)),
            "got {err:?}"
        );
        let err = PaymentError::from(StripeError::Declined(DeclineKind::Soft));
        assert!(
            matches!(err, PaymentError::AutoTopupDeclined(AutoTopupDeclineKind::Soft)),
            "got {err:?}"
        );

        let err = PaymentError::from(StripeError::Api("boom".to_string()));
        assert!(
            matches!(&err, PaymentError::ProviderApi(message) if message == "boom"),
            "got {err:?}"
        );
        let err = PaymentError::from(StripeError::InvalidData("bad id".to_string()));
        assert!(
            matches!(&err, PaymentError::InvalidData(message) if message == "bad id"),
            "got {err:?}"
        );
    }

    #[test]
    fn test_stripe_provider_from_config() {
        let config = crate::config::StripeConfig {
            api_key: "sk_test_fake".to_string(),
            price_id: "price_fake".to_string(),
            webhook_secret: "whsec_fake".to_string(),
            enable_invoice_creation: false,
            auto_topup_terms_of_service_text: None,
            setup_terms_of_service_text: None,
            tax_code: None,
            api_base_url: None,
        };
        let provider = StripeProvider::from(config);

        assert_eq!(provider.config.api_key, "sk_test_fake");
        assert_eq!(provider.config.price_id, "price_fake");
        assert_eq!(provider.config.webhook_secret, "whsec_fake");
        assert!(!provider.config.enable_invoice_creation);
    }

    #[test]
    fn test_stripe_provider_with_invoice_creation() {
        let config = crate::config::StripeConfig {
            api_key: "sk_test_fake".to_string(),
            price_id: "price_fake".to_string(),
            webhook_secret: "whsec_fake".to_string(),
            enable_invoice_creation: true,
            auto_topup_terms_of_service_text: None,
            setup_terms_of_service_text: None,
            tax_code: None,
            api_base_url: None,
        };
        let provider = StripeProvider::from(config);

        assert!(provider.config.enable_invoice_creation);
    }

    #[dwctl_test_macros::test]
    async fn test_stripe_idempotency_fast_path(pool: PgPool) {
        // Test the fast path: transaction already exists in DB
        let user_id = create_test_user(&pool).await;
        let session_id = "cs_test_fake_session_123";

        // Create a transaction using the Credits repository (handles balance_after properly)
        let mut conn = pool.acquire().await.unwrap();
        let mut credits = crate::db::handlers::Credits::new(&mut conn);

        let request = crate::db::models::credits::CreditTransactionCreateDBRequest {
            user_id,
            transaction_type: crate::db::models::credits::CreditTransactionType::Purchase,
            amount: Decimal::new(5000, 2),
            source_id: session_id.to_string(),
            description: Some("Test Stripe payment".to_string()),
            fusillade_batch_id: None,
            api_key_id: None,
        };

        credits.create_transaction(&request).await.unwrap();

        let config = crate::config::StripeConfig {
            api_key: "sk_test_fake".to_string(),
            price_id: "price_fake".to_string(),
            webhook_secret: "whsec_fake".to_string(),
            enable_invoice_creation: false,
            auto_topup_terms_of_service_text: None,
            setup_terms_of_service_text: None,
            tax_code: None,
            api_base_url: None,
        };
        let provider = StripeProvider::from(config);

        // Process the same session - should hit fast path and succeed
        let result = provider
            .process_payment_session(&pool, session_id, &crate::config::CreditsConfig::default())
            .await;
        assert!(result.is_ok(), "Should succeed via fast path (transaction already exists)");

        // Verify only one transaction exists
        let count = sqlx::query!(
            r#"
            SELECT COUNT(*) as count
            FROM credits_transactions
            WHERE source_id = $1
            "#,
            session_id
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(count.count.unwrap(), 1, "Should still have exactly one transaction");
    }

    /// A paid `payment`-mode session as dwctl-stripe hands it back.
    fn paid_session(pretax_cents: Option<i64>) -> CheckoutSession {
        CheckoutSession {
            mode: CheckoutMode::Payment,
            complete: true,
            paid: true,
            client_reference_id: Some("550e8400-e29b-41d4-a716-446655440001".to_string()),
            customer_id: Some("cus_test123".to_string()),
            creditee_id: None,
            pretax_cents,
            setup_intent: None,
        }
    }

    /// Regression test for the tax-inclusive crediting bug: Stripe's
    /// `amount_total` includes sales tax, so we must credit `amount_subtotal`.
    /// dwctl-stripe reduces the session to its pre-tax subtotal (tested there
    /// against a real session with subtotal 2500, tax 500, total 3000); the
    /// subtotal must survive the conversion to dollars intact.
    #[test]
    fn test_parse_payment_session_credits_the_pretax_subtotal() {
        assert_eq!(
            parse_payment_session(&paid_session(Some(2500))).expect("session parses").amount,
            Decimal::new(2500, 2)
        );
    }

    /// Integer division used to truncate the cents here, so a $10.50 checkout
    /// credited $10 and quietly kept the remaining 50c.
    #[test]
    fn test_parse_payment_session_keeps_partial_dollars() {
        let parsed = parse_payment_session(&paid_session(Some(1050))).expect("session parses");
        assert_eq!(parsed.amount, Decimal::new(1050, 2), "$10.50 must credit 10.50, not 10");
        assert_ne!(parsed.amount, Decimal::from(10));
    }

    #[test]
    fn test_payment_session_parsing() {
        // Test that PaymentSession structure is correct
        let creditee_id = "550e8400-e29b-41d4-a716-446655440000".parse().unwrap();
        let creditor_id = "550e8400-e29b-41d4-a716-446655440001".parse().unwrap();

        let session = PaymentSession {
            creditee_id,
            creditor_id,
            amount: Decimal::new(5000, 2),
            is_paid: true,
            payment_provider_id: Some("cus_test123".to_string()), // Stripe customer ID
        };

        assert_eq!(session.creditee_id, creditee_id);
        assert_eq!(session.creditor_id, creditor_id);
        assert_eq!(session.amount, Decimal::new(5000, 2));
        assert!(session.is_paid);
        assert_eq!(session.payment_provider_id, Some("cus_test123".to_string()));
    }

    #[test]
    fn test_webhook_event_parsing() {
        // Test WebhookEvent structure
        let event = WebhookEvent {
            event_type: "CheckoutSessionCompleted".to_string(),
            session_id: Some("cs_test_123".to_string()),
        };

        assert_eq!(event.event_type, "CheckoutSessionCompleted");
        assert_eq!(event.session_id, Some("cs_test_123".to_string()));
    }

    #[dwctl_test_macros::test]
    async fn test_payment_description_self(pool: PgPool) {
        // Test that when a user pays for themselves, description is just "Stripe payment"
        let user = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;

        // Set a Stripe customer ID for the user
        let customer_id = "cus_test_self_payment";
        sqlx::query!("UPDATE users SET payment_provider_id = $1 WHERE id = $2", customer_id, user.id)
            .execute(&pool)
            .await
            .unwrap();

        // Create a payment session where payer = recipient (self-payment)
        let payment_session = PaymentSession {
            creditee_id: user.id,
            creditor_id: user.id,
            amount: Decimal::new(5000, 2),
            is_paid: true,
            payment_provider_id: Some(customer_id.to_string()),
        };

        // Build description using the new logic (creditor_id comparison)
        let description = if payment_session.creditor_id == payment_session.creditee_id {
            "Stripe payment".to_string()
        } else {
            let mut conn = pool.acquire().await.unwrap();
            let mut users = crate::db::handlers::users::Users::new(&mut conn);

            if let Some(creditor) = users.get_by_id(payment_session.creditor_id).await.unwrap() {
                let creditor_name = creditor.display_name.unwrap_or(creditor.email);
                format!("Stripe payment from {}", creditor_name)
            } else {
                "Stripe payment".to_string()
            }
        };

        assert_eq!(description, "Stripe payment", "Self-payment should not include 'from' attribution");
    }

    #[dwctl_test_macros::test]
    async fn test_payment_description_other(pool: PgPool) {
        // Test that when a user pays for someone else, description includes "from {name}"
        let payer = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;
        let recipient = crate::test::utils::create_test_user(&pool, crate::api::models::users::Role::StandardUser).await;

        // Set a Stripe customer ID for the payer
        let customer_id = "cus_test_other_payment";
        sqlx::query!(
            "UPDATE users SET payment_provider_id = $1, display_name = $2 WHERE id = $3",
            customer_id,
            "John Admin",
            payer.id
        )
        .execute(&pool)
        .await
        .unwrap();

        // Create a payment session where payer != recipient
        let payment_session = PaymentSession {
            creditee_id: recipient.id,
            creditor_id: payer.id,
            amount: Decimal::new(5000, 2),
            is_paid: true,
            payment_provider_id: Some(customer_id.to_string()),
        };

        // Build description using the new logic (creditor_id comparison)
        let description = if payment_session.creditor_id == payment_session.creditee_id {
            "Stripe payment".to_string()
        } else {
            let mut conn = pool.acquire().await.unwrap();
            let mut users = crate::db::handlers::users::Users::new(&mut conn);

            if let Some(creditor) = users.get_by_id(payment_session.creditor_id).await.unwrap() {
                let creditor_name = creditor.display_name.unwrap_or(creditor.email);
                format!("Stripe payment from {}", creditor_name)
            } else {
                "Stripe payment".to_string()
            }
        };

        assert_eq!(
            description, "Stripe payment from John Admin",
            "Payment for others should include 'from' attribution"
        );
    }
}
