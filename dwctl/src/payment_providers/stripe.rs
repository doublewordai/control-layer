//! Stripe payment provider implementation

use async_trait::async_trait;
use rust_decimal::Decimal;
use sqlx::PgPool;
use std::collections::HashMap;
use stripe::{ApiErrorsCode, Client};
use stripe_billing::billing_portal_session::CreateBillingPortalSession;
use stripe_checkout::checkout_session::{
    CreateCheckoutSessionConsentCollection, CreateCheckoutSessionConsentCollectionPaymentMethodReuseAgreement,
    CreateCheckoutSessionConsentCollectionPaymentMethodReuseAgreementPosition, CreateCheckoutSessionConsentCollectionTermsOfService,
    CreateCheckoutSessionCustomText, CreateCheckoutSessionCustomerUpdate, CreateCheckoutSessionCustomerUpdateAddress,
    CreateCheckoutSessionCustomerUpdateName, CreateCheckoutSessionInvoiceCreation, CreateCheckoutSessionNameCollection,
    CreateCheckoutSessionNameCollectionBusiness, CreateCheckoutSessionPaymentMethodTypes, CreateCheckoutSessionSavedPaymentMethodOptions,
    CreateCheckoutSessionSavedPaymentMethodOptionsPaymentMethodRemove, CreateCheckoutSessionSavedPaymentMethodOptionsPaymentMethodSave,
    CreateCheckoutSessionSetupIntentData, CustomTextPositionParam,
};
use stripe_checkout::{
    CheckoutSessionId, CheckoutSessionMode, CheckoutSessionPaymentStatus, CheckoutSessionStatus, CheckoutSessionUiMode,
    checkout_session::{
        CreateCheckoutSession, CreateCheckoutSessionAutomaticTax, CreateCheckoutSessionCustomerCreation, CreateCheckoutSessionLineItems,
        CreateCheckoutSessionTaxIdCollection, RetrieveCheckoutSession,
    },
};
use stripe_types::Currency;
use stripe_webhook::{EventObject, Webhook};

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

fn classify_card_decline(advice_code: Option<&str>, decline_code: Option<&str>) -> AutoTopupDeclineKind {
    const HARD_DECLINE_CODES: &[&str] = &[
        "do_not_honor",
        "fraudulent",
        "lost_card",
        "merchant_blacklist",
        "pickup_card",
        "restricted_card",
        "revocation_of_all_authorizations",
        "revocation_of_authorization",
        "security_violation",
        "stolen_card",
        "stop_payment_order",
        "transaction_not_allowed",
    ];

    if advice_code == Some("do_not_try_again") || decline_code.is_some_and(|code| HARD_DECLINE_CODES.contains(&code)) {
        AutoTopupDeclineKind::Hard
    } else {
        AutoTopupDeclineKind::Soft
    }
}

fn map_auto_topup_charge_error(error: stripe::StripeError) -> PaymentError {
    match error {
        // Another caller is already performing this exact charge. Checked first:
        // it arrives as an invalid_request_error, not a CardError, so it would
        // otherwise fall through and be counted as a charge failure.
        stripe::StripeError::Stripe(api_error, _) if api_error.code == Some(ApiErrorsCode::IdempotencyKeyInUse) => {
            PaymentError::AlreadyProcessed
        }
        stripe::StripeError::Stripe(api_error, _) if matches!(api_error.type_, stripe::ApiErrorsType::CardError) => {
            PaymentError::AutoTopupDeclined(classify_card_decline(
                api_error.advice_code.as_deref(),
                api_error.decline_code.as_deref(),
            ))
        }
        stripe::StripeError::Stripe(api_error, status) => {
            tracing::error!(
                status,
                error_type = %api_error.type_,
                "Stripe rejected the auto top-up payment request"
            );
            PaymentError::ProviderApi(format!("Stripe {} error (HTTP {status})", api_error.type_))
        }
        other => {
            tracing::error!(error = %other, "Failed to create auto top-up payment intent");
            PaymentError::ProviderApi(other.to_string())
        }
    }
}

/// Stripe payment provider
pub struct StripeProvider {
    config: crate::config::StripeConfig,
    client: Client,
}

impl From<crate::config::StripeConfig> for StripeProvider {
    fn from(config: crate::config::StripeConfig) -> Self {
        let client = Client::new(&config.api_key);
        Self { config, client }
    }
}

/// The only things that differ between our two setup-mode checkouts.
///
/// Both save a card for later off-session use and are otherwise identical
/// (hosted page, tax id + business name collection, ToS + reuse consent, the
/// same payment method types, the same customer handling), so the shape of the
/// session lives in `create_setup_mode_session` and only the wording varies.
struct SetupSessionCopy<'a> {
    /// Custom terms-of-service acceptance text. `None` shows no ToS copy.
    terms_of_service_text: Option<&'a str>,
    /// Label on the submit button.
    submit_label: &'a str,
    /// Description recorded on the SetupIntent, for reconciliation in Stripe.
    setup_intent_description: &'a str,
}

impl StripeProvider {
    /// Build a `mode: setup` hosted Checkout Session: verifies and saves a
    /// payment method without charging it.
    ///
    /// Shared by onboarding card verification and auto-top-up enrolment, which
    /// want the same session with different copy.
    async fn create_setup_mode_session(
        &self,
        payer: &CheckoutPayer,
        cancel_url: &str,
        success_url: &str,
        copy: SetupSessionCopy<'_>,
    ) -> Result<String> {
        let mut checkout_params = CreateCheckoutSession::new()
            .cancel_url(cancel_url)
            .success_url(success_url)
            // Read back as the billing target when the session is fulfilled.
            .client_reference_id(payer.id.to_string())
            .mode(CheckoutSessionMode::Setup)
            .ui_mode(CheckoutSessionUiMode::HostedPage)
            // Collected here rather than later: automatic tax on the first real
            // charge needs an address on the customer, and the billing portal
            // detour to add one is exactly the friction this removes.
            .tax_id_collection(CreateCheckoutSessionTaxIdCollection::new(true))
            .name_collection(CreateCheckoutSessionNameCollection {
                business: Some(CreateCheckoutSessionNameCollectionBusiness::new(true)),
                individual: None,
            })
            .consent_collection(CreateCheckoutSessionConsentCollection {
                terms_of_service: Some(CreateCheckoutSessionConsentCollectionTermsOfService::Required),
                payment_method_reuse_agreement: Some(CreateCheckoutSessionConsentCollectionPaymentMethodReuseAgreement::new(
                    CreateCheckoutSessionConsentCollectionPaymentMethodReuseAgreementPosition::Auto,
                )),
                promotions: None,
            })
            .custom_text(CreateCheckoutSessionCustomText {
                terms_of_service_acceptance: copy.terms_of_service_text.map(CustomTextPositionParam::new),
                submit: Some(CustomTextPositionParam::new(copy.submit_label)),
                after_submit: None,
                shipping_address: None,
            })
            .payment_method_types(vec![
                CreateCheckoutSessionPaymentMethodTypes::Card,
                CreateCheckoutSessionPaymentMethodTypes::Link,
                CreateCheckoutSessionPaymentMethodTypes::SepaDebit,
            ])
            .setup_intent_data(CreateCheckoutSessionSetupIntentData {
                description: Some(copy.setup_intent_description.to_string()),
                metadata: None,
                on_behalf_of: None,
            });

        if let Some(existing_id) = &payer.payment_provider_id {
            tracing::debug!("Using existing Stripe customer ID {} for payer {}", existing_id, payer.id);
            checkout_params = checkout_params
                .customer(existing_id)
                .customer_update(CreateCheckoutSessionCustomerUpdate {
                    address: Some(CreateCheckoutSessionCustomerUpdateAddress::Auto),
                    name: Some(CreateCheckoutSessionCustomerUpdateName::Auto),
                    shipping: None,
                })
        } else {
            tracing::debug!("No customer ID found for payer {}, Stripe will create one", payer.id);
            checkout_params = checkout_params
                .customer_email(&payer.email)
                .customer_creation(CreateCheckoutSessionCustomerCreation::Always);
        }

        let checkout_session = checkout_params.send(&self.client).await.map_err(|e| {
            tracing::error!(
                purpose = copy.setup_intent_description,
                "Failed to create Stripe setup checkout session: {:?}",
                e
            );
            PaymentError::ProviderApi(e.to_string())
        })?;

        tracing::debug!(
            purpose = copy.setup_intent_description,
            "Created setup checkout session {} for payer {}",
            checkout_session.id,
            payer.id
        );

        checkout_session.url.ok_or_else(|| {
            tracing::error!("Setup checkout session missing URL");
            PaymentError::ProviderApi("Checkout session missing URL".to_string())
        })
    }

    /// Internal Stripe implementation of charge_auto_topup that returns the full PaymentIntent.
    async fn charge_auto_topup_internal(
        &self,
        amount_cents: i64,
        customer_id: &str,
        payment_method_id: &str,
        idempotency_key: &str,
    ) -> Result<stripe_core::PaymentIntent> {
        use stripe::{IdempotencyKey, RequestStrategy, StripeRequest};
        use stripe_core::payment_intent::{
            AsyncWorkflowsInputsParam, AsyncWorkflowsInputsTaxParam, AsyncWorkflowsParam, CreatePaymentIntent,
            CreatePaymentIntentOffSession,
        };
        use stripe_misc::tax_calculation::{CreateTaxCalculation, CreateTaxCalculationLineItems};

        // Calculate tax (with idempotency key so retries within the same minute
        // get the same tax calculation ID back, preventing PaymentIntent conflicts)
        let mut line_item = CreateTaxCalculationLineItems::new(amount_cents);
        line_item.reference = Some("auto_topup".to_string());
        // When None, Stripe falls back to the account-level default tax code.
        line_item.tax_code = self.config.tax_code.clone();

        let tax_idem_key = IdempotencyKey::new(format!("{}_tax", idempotency_key))
            .map_err(|e| PaymentError::InvalidData(format!("Invalid tax idempotency key: {e}")))?;

        let tax_calc = CreateTaxCalculation::new(Currency::USD, vec![line_item])
            .customer(customer_id)
            .customize()
            .request_strategy(RequestStrategy::Idempotent(tax_idem_key))
            .send(&self.client)
            .await
            .map_err(map_auto_topup_charge_error)?;

        let tax_calc_id = tax_calc
            .id
            .ok_or_else(|| PaymentError::ProviderApi("Tax calculation missing ID".to_string()))?;

        let idem_key =
            IdempotencyKey::new(idempotency_key).map_err(|e| PaymentError::InvalidData(format!("Invalid idempotency key: {e}")))?;

        // Create PaymentIntent with tax calculation linked and idempotency key
        CreatePaymentIntent::new(tax_calc.amount_total, Currency::USD)
            .customer(customer_id)
            .payment_method(payment_method_id)
            .off_session(CreatePaymentIntentOffSession::Bool(true))
            .confirm(true)
            .description("Automatic credit top-up")
            .statement_descriptor_suffix("AUTO-TOPUP")
            .hooks(AsyncWorkflowsParam {
                inputs: Some(AsyncWorkflowsInputsParam {
                    tax: Some(AsyncWorkflowsInputsTaxParam::new(tax_calc_id.to_string())),
                }),
            })
            .customize()
            .request_strategy(RequestStrategy::Idempotent(idem_key))
            .send(&self.client)
            .await
            .map_err(map_auto_topup_charge_error)
    }

    async fn get_setup_session(&self, session_id: &str) -> Result<stripe_checkout::CheckoutSession> {
        let session_id: CheckoutSessionId = session_id
            .parse()
            .map_err(|_| PaymentError::InvalidData("Invalid Stripe session ID".to_string()))?;

        RetrieveCheckoutSession::new(session_id)
            .expand(vec!["setup_intent".to_string(), "setup_intent.payment_method".to_string()])
            .send(&self.client)
            .await
            .map_err(|e| {
                tracing::error!("Failed to retrieve Stripe setup session: {:?}", e);
                PaymentError::ProviderApi(e.to_string())
            })
    }

    /// Retrieve a session with the expansions both checkout modes need.
    ///
    /// `process_payment_session` doesn't know the mode until it has the session,
    /// so it fetches once with the union of the expansions rather than paying a
    /// second Stripe round trip after dispatching. Stripe ignores expansions that
    /// don't apply to the session's mode.
    async fn get_session_for_processing(&self, session_id: &str) -> Result<stripe_checkout::CheckoutSession> {
        let session_id: CheckoutSessionId = session_id
            .parse()
            .map_err(|_| PaymentError::InvalidData("Invalid Stripe session ID".to_string()))?;

        RetrieveCheckoutSession::new(session_id)
            .expand(vec![
                "line_items".to_string(),
                "setup_intent".to_string(),
                "setup_intent.payment_method".to_string(),
            ])
            .send(&self.client)
            .await
            .map_err(|e| {
                tracing::error!("Failed to retrieve Stripe checkout session: {:?}", e);
                PaymentError::ProviderApi(e.to_string())
            })
    }

    /// Fulfil a completed `setup`-mode session: save the verified card, mark the
    /// billing target verified, and pay out signup credits.
    ///
    /// No money moved, so there is no purchase to record - the only ledger entry
    /// is the (optional, once-per-target) verification grant.
    async fn fulfil_setup_session(
        &self,
        conn: &mut sqlx::PgConnection,
        session: &stripe_checkout::CheckoutSession,
        session_id: &str,
        credits_config: &crate::config::CreditsConfig,
    ) -> Result<()> {
        if session.status != Some(CheckoutSessionStatus::Complete) {
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

        let customer_id = match &session.customer {
            Some(stripe_types::Expandable::Id(id)) => Some(id.to_string()),
            Some(stripe_types::Expandable::Object(c)) => Some(c.id.to_string()),
            None => None,
        };

        let setup_intent = match &session.setup_intent {
            Some(stripe_types::Expandable::Object(si)) => si.as_ref(),
            _ => {
                tracing::error!("Setup session {} has no expanded setup_intent", session_id);
                return Err(PaymentError::InvalidData("Setup intent not found or not expanded".to_string()));
            }
        };

        // A complete session whose SetupIntent didn't succeed means the card was
        // not actually verified - never treat that as proof of payment ability.
        if setup_intent.status.as_str() != "succeeded" {
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
        if let (Some(cust_id), Some(pm)) = (&customer_id, &setup_intent.payment_method) {
            let pm_id = pm.id().to_string();
            let mut invoice_settings = stripe_core::customer::UpdateCustomerInvoiceSettings::new();
            invoice_settings.default_payment_method = Some(pm_id.clone());

            if let Err(e) = stripe_core::customer::UpdateCustomer::new(cust_id.as_str())
                .invoice_settings(invoice_settings)
                .send(&self.client)
                .await
            {
                tracing::warn!("Failed to set default payment method {} on customer {}: {:?}", pm_id, cust_id, e);
            }
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
            if let Some(ref provider_id) = customer_id
                && users.set_payment_provider_id_if_empty(target_id, provider_id).await?
            {
                tracing::debug!("Saved newly created stripe ID {} for user ID {}", provider_id, target_id);
            }

            // A verified card clears the unverified rate-limit tier in onwards,
            // same as a completed purchase does.
            users.set_verified(target_id).await?;
        }

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
fn parse_payment_session(checkout_session: &stripe_checkout::CheckoutSession) -> Result<PaymentSession> {
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
        .metadata
        .as_ref()
        .and_then(|m| m.get("creditee_id"))
        .map(|s| s.parse())
        .transpose()
        .map_err(|e| {
            tracing::error!("Failed to parse creditee ID: {:?}", e);
            PaymentError::InvalidData(format!("Invalid creditee user ID: {}", e))
        })?
        .unwrap_or(creditor_id);

    let cents = pretax_credit_cents(checkout_session).ok_or_else(|| {
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
        is_paid: checkout_session.payment_status == CheckoutSessionPaymentStatus::Paid,
        creditor_id,
        payment_provider_id: checkout_session.customer.as_ref().map(|c| c.id().to_string()),
    })
}

/// Pre-tax amount (in cents) to credit for a completed checkout session.
///
/// Prefers the first line item's `amount_subtotal`, falling back to the
/// session-level `amount_subtotal`. Both are deliberately the *subtotal*
/// (before tax): `amount_total` includes the sales tax we collect on Stripe's
/// behalf, and crediting that would gift users credits equal to the tax. This
/// flow uses a fixed price with no discounts, so the subtotal is the value of
/// the credits purchased. Returns `None` if neither amount is present.
fn pretax_credit_cents(session: &stripe_checkout::CheckoutSession) -> Option<i64> {
    session
        .line_items
        .as_ref()
        .and_then(|items| items.data.first().map(|item| item.amount_subtotal))
        .or(session.amount_subtotal)
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
        let mut checkout_params = CreateCheckoutSession::new()
            .cancel_url(cancel_url)
            .success_url(success_url)
            .client_reference_id(payer.id.to_string()) // This is who will purchase the credits
            .currency(Currency::USD)
            .line_items(vec![CreateCheckoutSessionLineItems {
                price: Some(self.config.price_id.clone()),
                quantity: Some(1),
                ..Default::default()
            }])
            .automatic_tax(CreateCheckoutSessionAutomaticTax::new(true))
            .mode(CheckoutSessionMode::Payment)
            .ui_mode(CheckoutSessionUiMode::HostedPage)
            .expand(vec!["line_items".to_string()])
            .tax_id_collection(CreateCheckoutSessionTaxIdCollection::new(true))
            .name_collection(CreateCheckoutSessionNameCollection {
                business: Some(CreateCheckoutSessionNameCollectionBusiness::new(true)),
                individual: None,
            })
            .saved_payment_method_options(CreateCheckoutSessionSavedPaymentMethodOptions {
                allow_redisplay_filters: None,
                payment_method_save: Some(CreateCheckoutSessionSavedPaymentMethodOptionsPaymentMethodSave::Enabled),
                payment_method_remove: Some(CreateCheckoutSessionSavedPaymentMethodOptionsPaymentMethodRemove::Enabled),
            });

        if let Some(user_receiving_credits) = creditee_id {
            let mut metadata = HashMap::new();
            metadata.insert("creditee_id".to_string(), user_receiving_credits.to_string());
            checkout_params = checkout_params.metadata(metadata);
        }

        // Enable invoice creation if configured
        if self.config.enable_invoice_creation {
            checkout_params = checkout_params.invoice_creation(CreateCheckoutSessionInvoiceCreation::new(true));
        }

        // Include existing customer ID if we have one
        if let Some(existing_id) = &payer.payment_provider_id {
            tracing::debug!("Using existing Stripe customer ID {} for payer {}", existing_id, payer.id);
            checkout_params = checkout_params
                .customer(existing_id)
                .customer_update(CreateCheckoutSessionCustomerUpdate {
                    address: Some(CreateCheckoutSessionCustomerUpdateAddress::Auto),
                    name: Some(CreateCheckoutSessionCustomerUpdateName::Auto),
                    shipping: None,
                })
        } else {
            tracing::debug!("No customer ID found for payer {}, Stripe will create one", payer.id);
            checkout_params = checkout_params
                .customer_email(&payer.email)
                .customer_creation(CreateCheckoutSessionCustomerCreation::Always);
        }

        // Create checkout session
        let checkout_session = checkout_params.send(&self.client).await.map_err(|e| {
            tracing::error!("Failed to create Stripe checkout session: {:?}", e);
            PaymentError::ProviderApi(e.to_string())
        })?;

        tracing::debug!(
            "Created checkout session {} for creditee {} (payer: {})",
            checkout_session.id,
            creditee_id.unwrap_or(&payer.id.to_string()),
            payer.id
        );

        // Return checkout URL for hosted checkout
        checkout_session.url.ok_or_else(|| {
            tracing::error!("Checkout session missing URL");
            PaymentError::ProviderApi("Checkout session missing URL".to_string())
        })
    }

    async fn get_payment_session(&self, session_id: &str) -> Result<PaymentSession> {
        let session_id: CheckoutSessionId = session_id
            .parse()
            .map_err(|_| PaymentError::InvalidData("Invalid Stripe session ID".to_string()))?;

        // Retrieve full checkout session with line items
        let checkout_session = RetrieveCheckoutSession::new(session_id)
            .expand(vec!["line_items".to_string()])
            .send(&self.client)
            .await
            .map_err(|e| {
                tracing::error!("Failed to retrieve Stripe checkout session: {:?}", e);
                PaymentError::ProviderApi(e.to_string())
            })?;

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
        let session = self.get_session_for_processing(session_id).await?;

        if session.mode == CheckoutSessionMode::Setup {
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

        // Validate the webhook signature and construct the event
        let event = Webhook::construct_event(body, signature, &self.config.webhook_secret).map_err(|e| {
            tracing::error!("Failed to construct webhook event: {:?}", e);
            PaymentError::InvalidData(format!("Webhook validation failed: {}", e))
        })?;

        tracing::trace!("Validated Stripe webhook event: {:?}", event.type_);

        // Convert Stripe event to our generic WebhookEvent
        let session_id = match &event.data.object {
            EventObject::CheckoutSessionCompleted(session) | EventObject::CheckoutSessionAsyncPaymentSucceeded(session) => {
                Some(session.id.to_string())
            }
            _ => None,
        };

        let webhook_event = WebhookEvent {
            event_type: event.type_.to_string(),
            session_id,
        };

        Ok(Some(webhook_event))
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
        self.create_setup_mode_session(
            payer,
            cancel_url,
            success_url,
            SetupSessionCopy {
                terms_of_service_text: self
                    .config
                    .setup_terms_of_service_text
                    .as_deref()
                    .or(self.config.auto_topup_terms_of_service_text.as_deref()),
                submit_label: "Verify payment method",
                setup_intent_description: "Payment method verification",
            },
        )
        .await
    }

    async fn create_auto_topup_checkout_session(&self, payer: &CheckoutPayer, cancel_url: &str, success_url: &str) -> Result<String> {
        self.create_setup_mode_session(
            payer,
            cancel_url,
            success_url,
            SetupSessionCopy {
                terms_of_service_text: self.config.auto_topup_terms_of_service_text.as_deref(),
                submit_label: "Set up auto top-up",
                setup_intent_description: "Auto top-up setup",
            },
        )
        .await
    }

    async fn process_auto_topup_session(&self, _db_pool: &PgPool, session_id: &str) -> Result<AutoTopupSetupResult> {
        let session = self.get_setup_session(session_id).await?;

        // Check if setup was completed
        if session.status != Some(CheckoutSessionStatus::Complete) {
            return Err(PaymentError::PaymentNotCompleted);
        }

        // Extract customer ID (may be newly created by Stripe during checkout)
        let customer_id = match &session.customer {
            Some(stripe_types::Expandable::Id(id)) => Some(id.to_string()),
            Some(stripe_types::Expandable::Object(c)) => Some(c.id.to_string()),
            None => None,
        };

        // Extract the expanded SetupIntent
        let setup_intent = match session.setup_intent {
            Some(stripe_types::Expandable::Object(si)) => *si,
            _ => return Err(PaymentError::InvalidData("Setup intent not found or not expanded".to_string())),
        };

        // Check if the SetupIntent succeeded
        if setup_intent.status.as_str() != "succeeded" {
            return Err(PaymentError::InvalidData("Payment method setup failed".to_string()));
        }

        // Set the payment method as the customer's default for invoices,
        // so get_default_payment_method can find it later for auto top-up charges.
        // Checkout setup mode attaches the PM but doesn't set it as the default.
        if let (Some(cust_id), Some(pm)) = (&customer_id, &setup_intent.payment_method) {
            let pm_id = pm.id().to_string();
            let mut invoice_settings = stripe_core::customer::UpdateCustomerInvoiceSettings::new();
            invoice_settings.default_payment_method = Some(pm_id.clone());

            if let Err(e) = stripe_core::customer::UpdateCustomer::new(cust_id.as_str())
                .invoice_settings(invoice_settings)
                .send(&self.client)
                .await
            {
                tracing::warn!("Failed to set default payment method {} on customer {}: {:?}", pm_id, cust_id, e);
                // Non-fatal: the payment method is still attached, just not set as default
            }
        }

        Ok(AutoTopupSetupResult {
            customer_id,
            user_id: session.client_reference_id,
        })
    }

    async fn accrue_invoice_item(&self, amount_cents: i64, customer_id: &str, description: &str, idempotency_key: &str) -> Result<String> {
        use stripe::{IdempotencyKey, RequestStrategy, StripeRequest};
        use stripe_billing::invoice_item::CreateInvoiceItem;

        let idem = IdempotencyKey::new(idempotency_key).map_err(|e| PaymentError::InvalidData(format!("Invalid idempotency key: {e}")))?;

        // Deliberately a *pending* invoice item: no `invoice` field, so Stripe
        // parks it on the customer and sweeps it onto their next invoice rather
        // than billing it now. Raising one invoice per top-up would put an
        // invoice per auto-top-up through the customer's AP department, which
        // is the exact thing monthly invoicing exists to avoid.
        let item = CreateInvoiceItem::new()
            .customer(customer_id)
            .amount(amount_cents)
            .currency(Currency::USD)
            .description(description)
            .customize()
            .request_strategy(RequestStrategy::Idempotent(idem))
            .send(&self.client)
            .await
            .map_err(|e| {
                tracing::error!("Failed to create Stripe invoice item: {:?}", e);
                PaymentError::ProviderApi(e.to_string())
            })?;

        tracing::info!(
            invoice_item_id = %item.id,
            customer_id,
            amount_cents,
            "Accrued charge onto the customer's next invoice"
        );

        Ok(item.id.to_string())
    }

    async fn charge_auto_topup(
        &self,
        amount_cents: i64,
        customer_id: &str,
        payment_method_id: &str,
        idempotency_key: &str,
    ) -> Result<String> {
        let pi = self
            .charge_auto_topup_internal(amount_cents, customer_id, payment_method_id, idempotency_key)
            .await?;
        Ok(pi.id.to_string())
    }

    async fn get_default_payment_method(&self, customer_id: &str) -> Result<Option<String>> {
        use stripe_core::customer::{ListPaymentMethodsCustomer, RetrieveCustomer, RetrieveCustomerReturned};

        let result = RetrieveCustomer::new(customer_id.to_string())
            .send(&self.client)
            .await
            .map_err(|e| {
                tracing::error!("Failed to retrieve Stripe customer: {:?}", e);
                PaymentError::ProviderApi(e.to_string())
            })?;

        let customer = match result {
            RetrieveCustomerReturned::Customer(c) => c,
            RetrieveCustomerReturned::DeletedCustomer(_) => {
                tracing::warn!("Stripe customer {} has been deleted", customer_id);
                return Ok(None);
            }
        };

        // Prefer invoice_settings.default_payment_method (set by billing portal or our setup flow)
        let pm = customer
            .invoice_settings
            .and_then(|s| s.default_payment_method)
            .map(|expandable: stripe_types::Expandable<_>| expandable.id().to_string());

        if pm.is_some() {
            return Ok(pm);
        }

        // Fallback: list payment methods attached to the customer.
        // Checkout setup mode attaches the PM but may not set invoice_settings default
        // (e.g. if the UpdateCustomer call in process_auto_topup_session failed).
        let methods = ListPaymentMethodsCustomer::new(customer_id.to_string())
            .limit(1)
            .send(&self.client)
            .await
            .map_err(|e| {
                tracing::error!("Failed to list payment methods for customer {}: {:?}", customer_id, e);
                PaymentError::ProviderApi(e.to_string())
            })?;

        Ok(methods.data.first().map(|pm| pm.id.to_string()))
    }

    async fn customer_has_address(&self, customer_id: &str) -> Result<bool> {
        use stripe_core::customer::{RetrieveCustomer, RetrieveCustomerReturned};

        let result = RetrieveCustomer::new(customer_id.to_string())
            .send(&self.client)
            .await
            .map_err(|e| {
                tracing::error!("Failed to retrieve Stripe customer for address check: {:?}", e);
                PaymentError::ProviderApi(e.to_string())
            })?;

        let customer = match result {
            RetrieveCustomerReturned::Customer(c) => c,
            RetrieveCustomerReturned::DeletedCustomer(_) => return Ok(false),
        };

        Ok(customer.address.is_some())
    }

    async fn create_customer(&self, email: &str, name: Option<&str>) -> Result<String> {
        use stripe_core::customer::CreateCustomer;

        let mut params = CreateCustomer::new().email(email);
        if let Some(n) = name {
            params = params.name(n);
        }

        let customer = params.send(&self.client).await.map_err(|e| {
            tracing::error!("Failed to create Stripe customer: {:?}", e);
            PaymentError::ProviderApi(e.to_string())
        })?;

        Ok(customer.id.to_string())
    }

    async fn create_billing_portal_session(&self, customer_id: &str, return_url: &str) -> Result<String> {
        // Create billing portal session using builder pattern
        let session = CreateBillingPortalSession::new()
            .customer(customer_id)
            .return_url(return_url)
            .send(&self.client)
            .await
            .map_err(|e| {
                tracing::error!("Failed to create Stripe billing portal session: {:?}", e);
                PaymentError::ProviderApi(e.to_string())
            })?;

        tracing::debug!("Created billing portal session {} for customer {}", session.id, customer_id);

        // Return the portal session URL
        Ok(session.url)
    }
}

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

    /// Build a Stripe API error, as returned on a 409/402. `type_` matters:
    /// the idempotency conflict arrives as an `invalid_request_error`, declines
    /// as a `card_error`, and the mapper dispatches on both.
    fn stripe_api_error(code: Option<ApiErrorsCode>, type_: stripe::ApiErrorsType) -> stripe::StripeError {
        stripe::StripeError::Stripe(
            Box::new(stripe::ApiErrors {
                advice_code: None,
                charge: None,
                code,
                decline_code: None,
                doc_url: None,
                message: None,
                network_advice_code: None,
                network_decline_code: None,
                param: None,
                payment_intent: None,
                payment_method: None,
                payment_method_type: None,
                request_log_url: None,
                setup_intent: None,
                source: None,
                type_,
            }),
            409,
        )
    }

    #[test]
    fn test_idempotency_key_in_use_maps_to_already_processed() {
        // Another replica is mid-charge on the same key: a no-op, not a failure.
        let err = map_auto_topup_charge_error(stripe_api_error(
            Some(ApiErrorsCode::IdempotencyKeyInUse),
            stripe::ApiErrorsType::InvalidRequestError,
        ));
        assert!(matches!(err, PaymentError::AlreadyProcessed), "got {err:?}");
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

    #[test]
    fn test_card_error_still_classifies_as_a_decline() {
        // The idempotency check must not shadow real declines.
        let err = map_auto_topup_charge_error(stripe_api_error(
            Some(ApiErrorsCode::CardDeclined),
            stripe::ApiErrorsType::CardError,
        ));
        assert!(matches!(err, PaymentError::AutoTopupDeclined(_)), "got {err:?}");
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
        };
        let provider = StripeProvider::from(config);

        assert!(provider.config.enable_invoice_creation);
    }

    #[test]
    fn classifies_auto_topup_do_not_retry_advice_as_hard_decline() {
        assert_eq!(
            classify_card_decline(Some("do_not_try_again"), Some("insufficient_funds")),
            AutoTopupDeclineKind::Hard
        );
    }

    #[test]
    fn classifies_auto_topup_terminal_codes_as_hard_declines() {
        for decline_code in ["do_not_honor", "fraudulent", "lost_card", "pickup_card", "stolen_card"] {
            assert_eq!(
                classify_card_decline(None, Some(decline_code)),
                AutoTopupDeclineKind::Hard,
                "{decline_code} should disable auto top-up immediately"
            );
        }
    }

    #[test]
    fn classifies_auto_topup_retryable_card_errors_as_soft_declines() {
        for decline_code in [Some("insufficient_funds"), Some("processing_error"), None] {
            assert_eq!(
                classify_card_decline(None, decline_code),
                AutoTopupDeclineKind::Soft,
                "{decline_code:?} should receive one retry after 24 hours"
            );
        }
    }

    #[sqlx::test]
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

    /// Regression test for the tax-inclusive crediting bug: Stripe's
    /// `amount_total` includes sales tax, so we must credit `amount_subtotal`.
    /// The fixture is a real `checkout.session.completed` session with
    /// subtotal 2500, tax 500, total 3000.
    #[test]
    fn test_pretax_credit_cents_excludes_tax() {
        let session: stripe_checkout::CheckoutSession = serde_json::from_str(include_str!("test_fixtures/checkout_session_with_tax.json"))
            .expect("fixture should deserialize into a Stripe CheckoutSession");

        // Sanity-check the fixture is the tax-bearing case we care about.
        assert_eq!(session.amount_subtotal, Some(2500));
        assert_eq!(session.amount_total, Some(3000));

        // We must credit the pre-tax subtotal (2500), never the tax-inclusive
        // total (3000) - crediting the total would gift users the tax.
        assert_eq!(pretax_credit_cents(&session), Some(2500));

        // ...and the subtotal must survive the conversion to dollars intact.
        assert_eq!(
            parse_payment_session(&session).expect("fixture parses").amount,
            Decimal::new(2500, 2)
        );
    }

    /// Integer division used to truncate the cents here, so a $10.50 checkout
    /// credited $10 and quietly kept the remaining 50c.
    #[test]
    fn test_parse_payment_session_keeps_partial_dollars() {
        let mut session: stripe_checkout::CheckoutSession =
            serde_json::from_str(include_str!("test_fixtures/checkout_session_with_tax.json"))
                .expect("fixture should deserialize into a Stripe CheckoutSession");
        session.line_items = None;
        session.amount_subtotal = Some(1050);

        let parsed = parse_payment_session(&session).expect("session parses");
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

    #[sqlx::test]
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

    #[sqlx::test]
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
