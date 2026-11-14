//! Stripe API client for dwctl.
//!
//! Everything that names an async-stripe type lives here: request building,
//! response parsing, error classification and webhook signature verification.
//! The public API speaks plain types only (ids as `String`, amounts in cents),
//! so dependants never see a `stripe_*` type. Persistence and business rules
//! stay with the caller.
//!
//! The split exists for compile time. The generated Stripe response types are
//! very large, and every async method that holds one in its state makes the
//! compiler walk that type graph for borrow checking, auto-trait proofs and
//! monomorphization. Here that cost is paid once, when this crate changes,
//! instead of on every rebuild of dwctl. [`StripeApi`] returns boxed futures
//! for the same reason: the `Send` proof over the Stripe types is done in this
//! crate rather than at each call site.

use std::collections::HashMap;

use async_trait::async_trait;
use stripe::{ApiErrorsCode, ApiErrorsType, Client, ClientBuilder, IdempotencyKey, RequestStrategy, StripeError, StripeRequest};
use stripe_billing::{billing_portal_session::CreateBillingPortalSession, invoice_item::CreateInvoiceItem};
use stripe_checkout::{
    CheckoutSessionId, CheckoutSessionMode, CheckoutSessionPaymentStatus, CheckoutSessionStatus, CheckoutSessionUiMode,
    checkout_session::{
        CreateCheckoutSession, CreateCheckoutSessionAutomaticTax, CreateCheckoutSessionConsentCollection,
        CreateCheckoutSessionConsentCollectionPaymentMethodReuseAgreement,
        CreateCheckoutSessionConsentCollectionPaymentMethodReuseAgreementPosition, CreateCheckoutSessionConsentCollectionTermsOfService,
        CreateCheckoutSessionCustomText, CreateCheckoutSessionCustomerCreation, CreateCheckoutSessionCustomerUpdate,
        CreateCheckoutSessionCustomerUpdateAddress, CreateCheckoutSessionCustomerUpdateName, CreateCheckoutSessionInvoiceCreation,
        CreateCheckoutSessionLineItems, CreateCheckoutSessionNameCollection, CreateCheckoutSessionNameCollectionBusiness,
        CreateCheckoutSessionPaymentMethodTypes, CreateCheckoutSessionSavedPaymentMethodOptions,
        CreateCheckoutSessionSavedPaymentMethodOptionsPaymentMethodRemove, CreateCheckoutSessionSavedPaymentMethodOptionsPaymentMethodSave,
        CreateCheckoutSessionSetupIntentData, CreateCheckoutSessionTaxIdCollection, CustomTextPositionParam, RetrieveCheckoutSession,
    },
};
use stripe_core::{
    PaymentIntent,
    customer::{
        CreateCustomer, ListPaymentMethodsCustomer, RetrieveCustomer, RetrieveCustomerReturned, UpdateCustomer,
        UpdateCustomerInvoiceSettings,
    },
    payment_intent::{
        AsyncWorkflowsInputsParam, AsyncWorkflowsInputsTaxParam, AsyncWorkflowsParam, CreatePaymentIntent, CreatePaymentIntentOffSession,
    },
};
use stripe_misc::tax_calculation::{CreateTaxCalculation, CreateTaxCalculationLineItems};
use stripe_shared::PaymentMethod;
use stripe_types::{Currency, Expandable};
use stripe_webhook::{EventObject, Webhook};

/// Result type for Stripe operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Whether a declined off-session charge may be retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclineKind {
    Soft,
    Hard,
}

/// What went wrong talking to Stripe, already classified.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The Stripe API call failed, or its response lacked a required field.
    #[error("Stripe API error: {0}")]
    Api(String),

    /// An id, idempotency key or webhook payload was unusable.
    #[error("Invalid data: {0}")]
    InvalidData(String),

    /// Another caller is already performing the request with this idempotency key.
    #[error("Request already in progress")]
    AlreadyProcessed,

    /// The card was declined.
    #[error("Card declined: {0:?}")]
    Declined(DeclineKind),
}

/// What a setup-mode Checkout is for. Recorded on the SetupIntent as
/// `metadata.purpose` so the completed session can be told apart again:
/// `checkout.session.completed` arrives for both kinds. Both claim the saved
/// instrument when it has a fingerprint; only verification reports an
/// instrument another account already holds (auto top-up enrolment then
/// succeeds without verifying).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupPurpose {
    Verification,
    AutoTopup,
}

impl SetupPurpose {
    const METADATA_KEY: &'static str = "purpose";

    pub fn as_str(self) -> &'static str {
        match self {
            SetupPurpose::Verification => "verification",
            SetupPurpose::AutoTopup => "auto_topup",
        }
    }

    /// Sessions created before the purpose was recorded carry no metadata;
    /// treat those as verification, the stricter reading.
    fn from_metadata(metadata: Option<&HashMap<String, String>>) -> Self {
        match metadata.and_then(|m| m.get(Self::METADATA_KEY)).map(String::as_str) {
            Some("auto_topup") => SetupPurpose::AutoTopup,
            _ => SetupPurpose::Verification,
        }
    }

    /// The only things that differ between the two setup-mode checkouts.
    fn copy(self) -> SetupSessionCopy {
        match self {
            SetupPurpose::Verification => SetupSessionCopy {
                submit_label: "Verify payment method",
                setup_intent_description: "Payment method verification",
                payment_method_types: vec![CreateCheckoutSessionPaymentMethodTypes::Card],
            },
            SetupPurpose::AutoTopup => SetupSessionCopy {
                submit_label: "Set up auto top-up",
                setup_intent_description: "Auto top-up setup",
                payment_method_types: vec![
                    CreateCheckoutSessionPaymentMethodTypes::Card,
                    CreateCheckoutSessionPaymentMethodTypes::Link,
                    CreateCheckoutSessionPaymentMethodTypes::SepaDebit,
                ],
            },
        }
    }
}

/// The only things that differ between our two setup-mode checkouts.
///
/// Both save a payment method for later off-session use and are otherwise
/// identical (hosted page, tax id + business name collection, ToS + reuse
/// consent, the same customer handling), so the shape of the session lives in
/// `create_setup_checkout_session`; the wording, the purpose and the payment
/// method types offered vary.
struct SetupSessionCopy {
    /// Label on the submit button.
    submit_label: &'static str,
    /// Description recorded on the SetupIntent, for reconciliation in Stripe.
    setup_intent_description: &'static str,
    /// Payment method types Checkout offers. Verification is cards only: a
    /// SEPA mandate needs nothing but an IBAN, and in September 2026 one IBAN
    /// verified 178 bot accounts; a Link payment method carries no
    /// fingerprint, so the one-instrument-one-account rule could not be
    /// applied to it. Auto top-up keeps Link and SEPA because a verified
    /// customer may genuinely want to pay that way.
    payment_method_types: Vec<CreateCheckoutSessionPaymentMethodTypes>,
}

/// Who a checkout session bills.
#[derive(Debug, Clone, Copy)]
pub struct Payer<'a> {
    /// Recorded as the session's `client_reference_id` and read back when the
    /// session is fulfilled.
    pub reference_id: &'a str,
    /// Used to create a Stripe customer when `customer_id` is `None`.
    pub email: &'a str,
    /// Existing Stripe customer id, if any.
    pub customer_id: Option<&'a str>,
}

/// Parameters of a `payment`-mode checkout session.
#[derive(Debug, Clone, Copy)]
pub struct PaymentCheckout<'a> {
    pub payer: Payer<'a>,
    /// Recorded as `metadata.creditee_id` when credits go to someone other
    /// than the payer.
    pub creditee_id: Option<&'a str>,
    pub cancel_url: &'a str,
    pub success_url: &'a str,
    /// Stripe price of the single line item.
    pub price_id: &'a str,
    pub invoice_creation: bool,
}

/// Parameters of a `setup`-mode checkout session.
#[derive(Debug, Clone, Copy)]
pub struct SetupCheckout<'a> {
    pub payer: Payer<'a>,
    pub cancel_url: &'a str,
    pub success_url: &'a str,
    pub purpose: SetupPurpose,
    /// Custom terms-of-service acceptance text. `None` shows no ToS copy.
    pub terms_of_service_text: Option<&'a str>,
}

/// Mode of a checkout session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutMode {
    Payment,
    Setup,
    Other,
}

/// The fields of a retrieved Stripe checkout session that dwctl acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutSession {
    pub mode: CheckoutMode,
    /// The session's status is `complete`.
    pub complete: bool,
    /// The session's payment status is `paid`.
    pub paid: bool,
    pub client_reference_id: Option<String>,
    /// Stripe customer id (may have been created by Stripe during checkout).
    pub customer_id: Option<String>,
    /// `metadata.creditee_id`, when the session carries one.
    pub creditee_id: Option<String>,
    /// Pre-tax amount in cents to credit; see [`pretax_credit_cents`].
    pub pretax_cents: Option<i64>,
    /// The SetupIntent, when the session has one and it was expanded.
    pub setup_intent: Option<SetupIntent>,
}

/// The fields of an expanded SetupIntent that dwctl acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupIntent {
    /// Stripe's status string; `succeeded` means the method was verified.
    pub status: String,
    pub payment_method_id: Option<String>,
    pub purpose: SetupPurpose,
    /// Stripe's fingerprint of the saved payment method, present only when the
    /// payment method was expanded and its type has one.
    pub fingerprint: Option<String>,
}

/// A verified webhook event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookEvent {
    /// Type of event (e.g., "checkout.session.completed")
    pub event_type: String,
    /// Checkout session id, for the session completion events.
    pub session_id: Option<String>,
}

fn classify_card_decline(advice_code: Option<&str>, decline_code: Option<&str>) -> DeclineKind {
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
        DeclineKind::Hard
    } else {
        DeclineKind::Soft
    }
}

fn map_auto_topup_charge_error(error: StripeError) -> Error {
    match error {
        // Another caller is already performing this exact charge. Checked first:
        // it arrives as an invalid_request_error, not a CardError, so it would
        // otherwise fall through and be counted as a charge failure.
        StripeError::Stripe(api_error, _) if api_error.code == Some(ApiErrorsCode::IdempotencyKeyInUse) => Error::AlreadyProcessed,
        StripeError::Stripe(api_error, _) if matches!(api_error.type_, ApiErrorsType::CardError) => Error::Declined(classify_card_decline(
            api_error.advice_code.as_deref(),
            api_error.decline_code.as_deref(),
        )),
        StripeError::Stripe(api_error, status) => {
            tracing::error!(
                status,
                error_type = %api_error.type_,
                "Stripe rejected the auto top-up payment request"
            );
            Error::Api(format!("Stripe {} error (HTTP {status})", api_error.type_))
        }
        other => {
            tracing::error!(error = %other, "Failed to create auto top-up payment intent");
            Error::Api(other.to_string())
        }
    }
}

/// Stripe's fingerprint of a saved payment method: the same card or bank
/// account yields the same value on every customer, which is what the
/// one-instrument-one-account rule keys on. `None` for types without one
/// (Link, wallets), which therefore cannot verify an account.
fn instrument_fingerprint(pm: &PaymentMethod) -> Option<String> {
    pm.card
        .as_ref()
        .and_then(|card| card.fingerprint.clone())
        .or_else(|| pm.sepa_debit.as_ref().and_then(|sepa| sepa.fingerprint.clone()))
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

/// Reduce a retrieved session to the fields dwctl acts on.
fn summarize_session(session: &stripe_checkout::CheckoutSession) -> CheckoutSession {
    let setup_intent = match &session.setup_intent {
        Some(Expandable::Object(setup_intent)) => Some(SetupIntent {
            status: setup_intent.status.as_str().to_string(),
            payment_method_id: setup_intent.payment_method.as_ref().map(|pm| pm.id().to_string()),
            purpose: SetupPurpose::from_metadata(setup_intent.metadata.as_ref()),
            fingerprint: setup_intent
                .payment_method
                .as_ref()
                .and_then(|pm| pm.as_object())
                .and_then(instrument_fingerprint),
        }),
        _ => None,
    };

    CheckoutSession {
        mode: match session.mode {
            CheckoutSessionMode::Payment => CheckoutMode::Payment,
            CheckoutSessionMode::Setup => CheckoutMode::Setup,
            _ => CheckoutMode::Other,
        },
        complete: session.status == Some(CheckoutSessionStatus::Complete),
        paid: session.payment_status == CheckoutSessionPaymentStatus::Paid,
        client_reference_id: session.client_reference_id.clone(),
        customer_id: session.customer.as_ref().map(|customer| customer.id().to_string()),
        creditee_id: session.metadata.as_ref().and_then(|m| m.get("creditee_id")).cloned(),
        pretax_cents: pretax_credit_cents(session),
        setup_intent,
    }
}

fn parse_session_id(session_id: &str) -> Result<CheckoutSessionId> {
    session_id
        .parse()
        .map_err(|_| Error::InvalidData("Invalid Stripe session ID".to_string()))
}

/// Verify a webhook payload against its `stripe-signature` header value and
/// extract the event type and, for session completion events, the session id.
pub fn verify_webhook(body: &str, signature: &str, webhook_secret: &str) -> Result<WebhookEvent> {
    // Validate the webhook signature and construct the event
    let event = Webhook::construct_event(body, signature, webhook_secret).map_err(|e| {
        tracing::error!("Failed to construct webhook event: {:?}", e);
        Error::InvalidData(format!("Webhook validation failed: {}", e))
    })?;

    tracing::trace!("Validated Stripe webhook event: {:?}", event.type_);

    let session_id = match &event.data.object {
        EventObject::CheckoutSessionCompleted(session) | EventObject::CheckoutSessionAsyncPaymentSucceeded(session) => {
            Some(session.id.to_string())
        }
        _ => None,
    };

    Ok(WebhookEvent {
        event_type: event.type_.to_string(),
        session_id,
    })
}

/// The Stripe API calls dwctl makes.
///
/// A trait only so that `async_trait` boxes each method's future: the caller
/// then holds a `Pin<Box<dyn Future + Send>>` and never has to prove `Send`
/// over the Stripe response types inside it.
#[async_trait]
pub trait StripeApi: Send + Sync {
    /// Create a `payment`-mode hosted checkout session. Returns its URL.
    async fn create_payment_checkout_session(&self, checkout: PaymentCheckout<'_>) -> Result<String>;

    /// Create a `setup`-mode hosted checkout session, which verifies and saves
    /// a payment method without charging it. Returns its URL.
    async fn create_setup_checkout_session(&self, checkout: SetupCheckout<'_>) -> Result<String>;

    /// Retrieve a session with `line_items` expanded.
    async fn get_payment_session(&self, session_id: &str) -> Result<CheckoutSession>;

    /// Retrieve a session with the SetupIntent and its payment method expanded.
    async fn get_setup_session(&self, session_id: &str) -> Result<CheckoutSession>;

    /// Retrieve a session with the expansions both checkout modes need.
    ///
    /// A caller that doesn't know the mode until it has the session fetches
    /// once with the union of the expansions rather than paying a second
    /// Stripe round trip after dispatching. Stripe ignores expansions that
    /// don't apply to the session's mode.
    async fn get_session_for_processing(&self, session_id: &str) -> Result<CheckoutSession>;

    /// Make `payment_method_id` the customer's default for invoices.
    ///
    /// Best-effort: a failure is logged and otherwise ignored, because the
    /// payment method stays attached and [`StripeApi::get_default_payment_method`]
    /// falls back to listing attached methods.
    async fn set_default_payment_method(&self, customer_id: &str, payment_method_id: &str);

    /// Charge a saved payment method off-session, with tax calculated first.
    /// Returns the PaymentIntent id.
    ///
    /// `tax_code` of `None` falls back to the account-level default tax code.
    async fn charge_auto_topup(
        &self,
        amount_cents: i64,
        customer_id: &str,
        payment_method_id: &str,
        idempotency_key: &str,
        tax_code: Option<&str>,
    ) -> Result<String>;

    /// Park a pending invoice item on the customer. Returns its id.
    async fn accrue_invoice_item(&self, amount_cents: i64, customer_id: &str, description: &str, idempotency_key: &str) -> Result<String>;

    /// The customer's default payment method id, if any.
    async fn get_default_payment_method(&self, customer_id: &str) -> Result<Option<String>>;

    /// Whether the customer has an address on file.
    async fn customer_has_address(&self, customer_id: &str) -> Result<bool>;

    /// Create a customer. Returns its id.
    async fn create_customer(&self, email: &str, name: Option<&str>) -> Result<String>;

    /// Create a billing portal session. Returns its URL.
    async fn create_billing_portal_session(&self, customer_id: &str, return_url: &str) -> Result<String>;
}

/// Client for the Stripe API.
pub struct StripeClient {
    client: Client,
}

impl StripeClient {
    /// Build a client for `api_key`.
    ///
    /// `api_base` of `None` talks to Stripe itself; otherwise it must end in
    /// `/`, because request paths are appended to it verbatim.
    pub fn new(api_key: &str, api_base: Option<String>) -> Result<Self> {
        let mut builder = ClientBuilder::new(api_key);
        if let Some(api_base) = api_base {
            builder = builder.url(api_base);
        }
        let client = builder.build().map_err(|e| Error::Api(e.to_string()))?;
        Ok(Self { client })
    }

    /// Charge a saved payment method off-session and return the full PaymentIntent.
    async fn charge_auto_topup_internal(
        &self,
        amount_cents: i64,
        customer_id: &str,
        payment_method_id: &str,
        idempotency_key: &str,
        tax_code: Option<&str>,
    ) -> Result<PaymentIntent> {
        // Calculate tax (with idempotency key so retries within the same minute
        // get the same tax calculation ID back, preventing PaymentIntent conflicts)
        let mut line_item = CreateTaxCalculationLineItems::new(amount_cents);
        line_item.reference = Some("auto_topup".to_string());
        // When None, Stripe falls back to the account-level default tax code.
        line_item.tax_code = tax_code.map(str::to_string);

        let tax_idem_key = IdempotencyKey::new(format!("{}_tax", idempotency_key))
            .map_err(|e| Error::InvalidData(format!("Invalid tax idempotency key: {e}")))?;

        let tax_calc = CreateTaxCalculation::new(Currency::USD, vec![line_item])
            .customer(customer_id)
            .customize()
            .request_strategy(RequestStrategy::Idempotent(tax_idem_key))
            .send(&self.client)
            .await
            .map_err(map_auto_topup_charge_error)?;

        let tax_calc_id = tax_calc.id.ok_or_else(|| Error::Api("Tax calculation missing ID".to_string()))?;

        let idem_key = IdempotencyKey::new(idempotency_key).map_err(|e| Error::InvalidData(format!("Invalid idempotency key: {e}")))?;

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
}

#[async_trait]
impl StripeApi for StripeClient {
    async fn create_payment_checkout_session(&self, checkout: PaymentCheckout<'_>) -> Result<String> {
        let payer = checkout.payer;
        let mut checkout_params = CreateCheckoutSession::new()
            .cancel_url(checkout.cancel_url)
            .success_url(checkout.success_url)
            .client_reference_id(payer.reference_id) // This is who will purchase the credits
            .currency(Currency::USD)
            .line_items(vec![CreateCheckoutSessionLineItems {
                price: Some(checkout.price_id.to_string()),
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

        if let Some(user_receiving_credits) = checkout.creditee_id {
            let mut metadata = HashMap::new();
            metadata.insert("creditee_id".to_string(), user_receiving_credits.to_string());
            checkout_params = checkout_params.metadata(metadata);
        }

        // Enable invoice creation if configured
        if checkout.invoice_creation {
            checkout_params = checkout_params.invoice_creation(CreateCheckoutSessionInvoiceCreation::new(true));
        }

        // Include existing customer ID if we have one
        if let Some(existing_id) = payer.customer_id {
            tracing::debug!("Using existing Stripe customer ID {} for payer {}", existing_id, payer.reference_id);
            checkout_params = checkout_params
                .customer(existing_id)
                .customer_update(CreateCheckoutSessionCustomerUpdate {
                    address: Some(CreateCheckoutSessionCustomerUpdateAddress::Auto),
                    name: Some(CreateCheckoutSessionCustomerUpdateName::Auto),
                    shipping: None,
                })
        } else {
            tracing::debug!("No customer ID found for payer {}, Stripe will create one", payer.reference_id);
            checkout_params = checkout_params
                .customer_email(payer.email)
                .customer_creation(CreateCheckoutSessionCustomerCreation::Always);
        }

        // Create checkout session
        let checkout_session = checkout_params.send(&self.client).await.map_err(|e| {
            tracing::error!("Failed to create Stripe checkout session: {:?}", e);
            Error::Api(e.to_string())
        })?;

        tracing::debug!(
            "Created checkout session {} for creditee {} (payer: {})",
            checkout_session.id,
            checkout.creditee_id.unwrap_or(payer.reference_id),
            payer.reference_id
        );

        // Return checkout URL for hosted checkout
        checkout_session.url.ok_or_else(|| {
            tracing::error!("Checkout session missing URL");
            Error::Api("Checkout session missing URL".to_string())
        })
    }

    async fn create_setup_checkout_session(&self, checkout: SetupCheckout<'_>) -> Result<String> {
        let payer = checkout.payer;
        let copy = checkout.purpose.copy();
        let mut checkout_params = CreateCheckoutSession::new()
            .cancel_url(checkout.cancel_url)
            .success_url(checkout.success_url)
            // Read back as the billing target when the session is fulfilled.
            .client_reference_id(payer.reference_id)
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
                terms_of_service_acceptance: checkout.terms_of_service_text.map(CustomTextPositionParam::new),
                submit: Some(CustomTextPositionParam::new(copy.submit_label)),
                after_submit: None,
                shipping_address: None,
            })
            .payment_method_types(copy.payment_method_types)
            .setup_intent_data(CreateCheckoutSessionSetupIntentData {
                description: Some(copy.setup_intent_description.to_string()),
                metadata: Some(HashMap::from([(
                    SetupPurpose::METADATA_KEY.to_string(),
                    checkout.purpose.as_str().to_string(),
                )])),
                on_behalf_of: None,
            });

        if let Some(existing_id) = payer.customer_id {
            tracing::debug!("Using existing Stripe customer ID {} for payer {}", existing_id, payer.reference_id);
            checkout_params = checkout_params
                .customer(existing_id)
                .customer_update(CreateCheckoutSessionCustomerUpdate {
                    address: Some(CreateCheckoutSessionCustomerUpdateAddress::Auto),
                    name: Some(CreateCheckoutSessionCustomerUpdateName::Auto),
                    shipping: None,
                })
        } else {
            tracing::debug!("No customer ID found for payer {}, Stripe will create one", payer.reference_id);
            checkout_params = checkout_params
                .customer_email(payer.email)
                .customer_creation(CreateCheckoutSessionCustomerCreation::Always);
        }

        let checkout_session = checkout_params.send(&self.client).await.map_err(|e| {
            tracing::error!(
                purpose = copy.setup_intent_description,
                "Failed to create Stripe setup checkout session: {:?}",
                e
            );
            Error::Api(e.to_string())
        })?;

        tracing::debug!(
            purpose = copy.setup_intent_description,
            "Created setup checkout session {} for payer {}",
            checkout_session.id,
            payer.reference_id
        );

        checkout_session.url.ok_or_else(|| {
            tracing::error!("Setup checkout session missing URL");
            Error::Api("Checkout session missing URL".to_string())
        })
    }

    async fn get_payment_session(&self, session_id: &str) -> Result<CheckoutSession> {
        // Retrieve full checkout session with line items
        let checkout_session = RetrieveCheckoutSession::new(parse_session_id(session_id)?)
            .expand(vec!["line_items".to_string()])
            .send(&self.client)
            .await
            .map_err(|e| {
                tracing::error!("Failed to retrieve Stripe checkout session: {:?}", e);
                Error::Api(e.to_string())
            })?;

        Ok(summarize_session(&checkout_session))
    }

    async fn get_setup_session(&self, session_id: &str) -> Result<CheckoutSession> {
        let checkout_session = RetrieveCheckoutSession::new(parse_session_id(session_id)?)
            .expand(vec!["setup_intent".to_string(), "setup_intent.payment_method".to_string()])
            .send(&self.client)
            .await
            .map_err(|e| {
                tracing::error!("Failed to retrieve Stripe setup session: {:?}", e);
                Error::Api(e.to_string())
            })?;

        Ok(summarize_session(&checkout_session))
    }

    async fn get_session_for_processing(&self, session_id: &str) -> Result<CheckoutSession> {
        let checkout_session = RetrieveCheckoutSession::new(parse_session_id(session_id)?)
            .expand(vec![
                "line_items".to_string(),
                "setup_intent".to_string(),
                "setup_intent.payment_method".to_string(),
            ])
            .send(&self.client)
            .await
            .map_err(|e| {
                tracing::error!("Failed to retrieve Stripe checkout session: {:?}", e);
                Error::Api(e.to_string())
            })?;

        Ok(summarize_session(&checkout_session))
    }

    async fn set_default_payment_method(&self, customer_id: &str, payment_method_id: &str) {
        let mut invoice_settings = UpdateCustomerInvoiceSettings::new();
        invoice_settings.default_payment_method = Some(payment_method_id.to_string());

        if let Err(e) = UpdateCustomer::new(customer_id)
            .invoice_settings(invoice_settings)
            .send(&self.client)
            .await
        {
            // Non-fatal: the payment method is still attached, just not set as default
            tracing::warn!(
                "Failed to set default payment method {} on customer {}: {:?}",
                payment_method_id,
                customer_id,
                e
            );
        }
    }

    async fn charge_auto_topup(
        &self,
        amount_cents: i64,
        customer_id: &str,
        payment_method_id: &str,
        idempotency_key: &str,
        tax_code: Option<&str>,
    ) -> Result<String> {
        let pi = self
            .charge_auto_topup_internal(amount_cents, customer_id, payment_method_id, idempotency_key, tax_code)
            .await?;
        Ok(pi.id.to_string())
    }

    async fn accrue_invoice_item(&self, amount_cents: i64, customer_id: &str, description: &str, idempotency_key: &str) -> Result<String> {
        let idem = IdempotencyKey::new(idempotency_key).map_err(|e| Error::InvalidData(format!("Invalid idempotency key: {e}")))?;

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
                Error::Api(e.to_string())
            })?;

        tracing::info!(
            invoice_item_id = %item.id,
            customer_id,
            amount_cents,
            "Accrued charge onto the customer's next invoice"
        );

        Ok(item.id.to_string())
    }

    async fn get_default_payment_method(&self, customer_id: &str) -> Result<Option<String>> {
        let result = RetrieveCustomer::new(customer_id.to_string())
            .send(&self.client)
            .await
            .map_err(|e| {
                tracing::error!("Failed to retrieve Stripe customer: {:?}", e);
                Error::Api(e.to_string())
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
            .map(|expandable: Expandable<_>| expandable.id().to_string());

        if pm.is_some() {
            return Ok(pm);
        }

        // Fallback: list payment methods attached to the customer.
        // Checkout setup mode attaches the PM but may not set invoice_settings default
        // (e.g. if the earlier `set_default_payment_method` call failed).
        let methods = ListPaymentMethodsCustomer::new(customer_id.to_string())
            .limit(1)
            .send(&self.client)
            .await
            .map_err(|e| {
                tracing::error!("Failed to list payment methods for customer {}: {:?}", customer_id, e);
                Error::Api(e.to_string())
            })?;

        Ok(methods.data.first().map(|pm| pm.id.to_string()))
    }

    async fn customer_has_address(&self, customer_id: &str) -> Result<bool> {
        let result = RetrieveCustomer::new(customer_id.to_string())
            .send(&self.client)
            .await
            .map_err(|e| {
                tracing::error!("Failed to retrieve Stripe customer for address check: {:?}", e);
                Error::Api(e.to_string())
            })?;

        let customer = match result {
            RetrieveCustomerReturned::Customer(c) => c,
            RetrieveCustomerReturned::DeletedCustomer(_) => return Ok(false),
        };

        Ok(customer.address.is_some())
    }

    async fn create_customer(&self, email: &str, name: Option<&str>) -> Result<String> {
        let mut params = CreateCustomer::new().email(email);
        if let Some(n) = name {
            params = params.name(n);
        }

        let customer = params.send(&self.client).await.map_err(|e| {
            tracing::error!("Failed to create Stripe customer: {:?}", e);
            Error::Api(e.to_string())
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
                Error::Api(e.to_string())
            })?;

        tracing::debug!("Created billing portal session {} for customer {}", session.id, customer_id);

        // Return the portal session URL
        Ok(session.url)
    }
}

#[cfg(test)]
mod tests {
    use stripe::ApiErrors;

    use super::*;

    /// Build a Stripe API error, as returned on a 409/402. `type_` matters:
    /// the idempotency conflict arrives as an `invalid_request_error`, declines
    /// as a `card_error`, and the mapper dispatches on both.
    fn stripe_api_error(code: Option<ApiErrorsCode>, type_: ApiErrorsType) -> StripeError {
        StripeError::Stripe(
            Box::new(ApiErrors {
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

    /// A Stripe PaymentMethod as the API returns it, expanded on a SetupIntent.
    /// Only the fields the type requires plus the one under test.
    fn payment_method_json(type_: &str, details: &str) -> PaymentMethod {
        let json = format!(
            r#"{{"id":"pm_test","object":"payment_method","billing_details":{{}},"created":1700000000,"livemode":false,"type":"{type_}",{details}}}"#
        );
        miniserde::json::from_str(&json).expect("payment method json")
    }

    fn session_with_tax() -> stripe_checkout::CheckoutSession {
        serde_json::from_str(include_str!("../test_fixtures/checkout_session_with_tax.json"))
            .expect("fixture should deserialize into a Stripe CheckoutSession")
    }

    #[test]
    fn test_instrument_fingerprint_reads_card_and_sepa_and_nothing_else() {
        let card = payment_method_json(
            "card",
            r#""card":{"brand":"visa","exp_month":1,"exp_year":2030,"funding":"credit","last4":"4242","fingerprint":"fp_card"}"#,
        );
        assert_eq!(instrument_fingerprint(&card).as_deref(), Some("fp_card"));

        let sepa = payment_method_json(
            "sepa_debit",
            r#""sepa_debit":{"bank_code":"ABNA","branch_code":"","country":"NL","last4":"0000","fingerprint":"fp_iban"}"#,
        );
        assert_eq!(instrument_fingerprint(&sepa).as_deref(), Some("fp_iban"));

        // Link carries no fingerprint, so it can never claim an instrument.
        let link = payment_method_json("link", r#""link":{"email":"someone@example.com"}"#);
        assert_eq!(instrument_fingerprint(&link), None);

        // A card the API returned without a fingerprint is treated as none.
        let bare = payment_method_json(
            "card",
            r#""card":{"brand":"visa","exp_month":1,"exp_year":2030,"funding":"credit","last4":"4242"}"#,
        );
        assert_eq!(instrument_fingerprint(&bare), None);
    }

    #[test]
    fn test_setup_purpose_round_trips_and_defaults_to_verification() {
        let meta = HashMap::from([(SetupPurpose::METADATA_KEY.to_string(), "auto_topup".to_string())]);
        assert_eq!(SetupPurpose::from_metadata(Some(&meta)), SetupPurpose::AutoTopup);
        let meta = HashMap::from([(SetupPurpose::METADATA_KEY.to_string(), "verification".to_string())]);
        assert_eq!(SetupPurpose::from_metadata(Some(&meta)), SetupPurpose::Verification);
        assert_eq!(SetupPurpose::from_metadata(None), SetupPurpose::Verification);
    }

    #[test]
    fn test_idempotency_key_in_use_maps_to_already_processed() {
        // Another replica is mid-charge on the same key: a no-op, not a failure.
        let err = map_auto_topup_charge_error(stripe_api_error(
            Some(ApiErrorsCode::IdempotencyKeyInUse),
            ApiErrorsType::InvalidRequestError,
        ));
        assert!(matches!(err, Error::AlreadyProcessed), "got {err:?}");
    }

    #[test]
    fn test_card_error_still_classifies_as_a_decline() {
        // The idempotency check must not shadow real declines.
        let err = map_auto_topup_charge_error(stripe_api_error(Some(ApiErrorsCode::CardDeclined), ApiErrorsType::CardError));
        assert!(matches!(err, Error::Declined(_)), "got {err:?}");
    }

    #[test]
    fn classifies_auto_topup_do_not_retry_advice_as_hard_decline() {
        assert_eq!(
            classify_card_decline(Some("do_not_try_again"), Some("insufficient_funds")),
            DeclineKind::Hard
        );
    }

    #[test]
    fn classifies_auto_topup_terminal_codes_as_hard_declines() {
        for decline_code in ["do_not_honor", "fraudulent", "lost_card", "pickup_card", "stolen_card"] {
            assert_eq!(
                classify_card_decline(None, Some(decline_code)),
                DeclineKind::Hard,
                "{decline_code} should disable auto top-up immediately"
            );
        }
    }

    #[test]
    fn classifies_auto_topup_retryable_card_errors_as_soft_declines() {
        for decline_code in [Some("insufficient_funds"), Some("processing_error"), None] {
            assert_eq!(
                classify_card_decline(None, decline_code),
                DeclineKind::Soft,
                "{decline_code:?} should receive one retry after 24 hours"
            );
        }
    }

    /// Regression test for the tax-inclusive crediting bug: Stripe's
    /// `amount_total` includes sales tax, so we must credit `amount_subtotal`.
    /// The fixture is a real `checkout.session.completed` session with
    /// subtotal 2500, tax 500, total 3000.
    #[test]
    fn test_pretax_credit_cents_excludes_tax() {
        let session = session_with_tax();

        // Sanity-check the fixture is the tax-bearing case we care about.
        assert_eq!(session.amount_subtotal, Some(2500));
        assert_eq!(session.amount_total, Some(3000));

        // We must credit the pre-tax subtotal (2500), never the tax-inclusive
        // total (3000) - crediting the total would gift users the tax.
        assert_eq!(pretax_credit_cents(&session), Some(2500));

        // ...and the subtotal must survive the reduction to plain fields intact.
        assert_eq!(summarize_session(&session).pretax_cents, Some(2500));
    }

    /// Without line items the session-level subtotal is used, cents intact.
    #[test]
    fn test_pretax_credit_cents_falls_back_to_the_session_subtotal() {
        let mut session = session_with_tax();
        session.line_items = None;
        session.amount_subtotal = Some(1050);

        assert_eq!(summarize_session(&session).pretax_cents, Some(1050));
    }
}
