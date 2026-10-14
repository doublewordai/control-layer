//! Tests that drive `StripeProvider` against a local mock of the Stripe API.
//!
//! The provider's client is pointed at a `wiremock` server through
//! `StripeConfig::api_base_url`, so the real async-stripe request building,
//! response parsing and error mapping all run, and nothing reaches Stripe. The
//! fixtures under `test_fixtures/` follow the shape of real API objects but use
//! fictional ids, emails and amounts. Webhook payloads are signed in-test with
//! Stripe's scheme so `Webhook::construct_event` runs for real.

use super::*;
use crate::{
    api::models::users::Role,
    config::{CreditsConfig, PaymentConfig},
    test::utils::{create_test_config, create_test_org, create_test_user},
};
use axum::{Router, routing::post};
use axum_test::TestServer;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const WEBHOOK_SECRET: &str = "whsec_test_secret";
const SESSION_ID: &str = "cs_test_ExampleCheckoutSession";
const CUSTOMER_ID: &str = "cus_ExampleCustomer";

fn fixture(name: &str) -> Value {
    let raw = match name {
        "checkout_session_with_tax" => include_str!("../test_fixtures/checkout_session_with_tax.json"),
        "setup_session_card" => include_str!("../test_fixtures/setup_session_card.json"),
        "customer" => include_str!("../test_fixtures/customer.json"),
        "payment_method_list" => include_str!("../test_fixtures/payment_method_list.json"),
        "tax_calculation" => include_str!("../test_fixtures/tax_calculation.json"),
        "payment_intent_succeeded" => include_str!("../test_fixtures/payment_intent_succeeded.json"),
        "invoice_item" => include_str!("../test_fixtures/invoice_item.json"),
        "billing_portal_session" => include_str!("../test_fixtures/billing_portal_session.json"),
        "error_card_declined" => include_str!("../test_fixtures/error_card_declined.json"),
        "error_card_declined_soft" => include_str!("../test_fixtures/error_card_declined_soft.json"),
        "error_idempotency_in_use" => include_str!("../test_fixtures/error_idempotency_in_use.json"),
        other => panic!("unknown fixture {other}"),
    };
    serde_json::from_str(raw).expect("fixture is valid JSON")
}

fn stripe_config_at(api_base_url: &str) -> crate::config::StripeConfig {
    crate::config::StripeConfig {
        api_key: "sk_test_fake".to_string(),
        webhook_secret: WEBHOOK_SECRET.to_string(),
        price_id: "price_ExampleCredits".to_string(),
        enable_invoice_creation: false,
        auto_topup_terms_of_service_text: None,
        setup_terms_of_service_text: None,
        tax_code: None,
        api_base_url: Some(api_base_url.to_string()),
    }
}

/// Config pointing at `server`, using its URI as-is (no trailing slash).
fn stripe_config(server: &MockServer) -> crate::config::StripeConfig {
    stripe_config_at(&server.uri())
}

fn provider(server: &MockServer) -> StripeProvider {
    StripeProvider::from(stripe_config(server))
}

/// A provider for tests that must never reach the network at all.
fn offline_provider() -> StripeProvider {
    StripeProvider::from(stripe_config_at("http://127.0.0.1:9"))
}

async fn mock_json(server: &MockServer, verb: &str, route: &str, status: u16, body: Value) {
    Mock::given(method(verb))
        .and(path(route))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

async fn mock_session(server: &MockServer, session: &Value) {
    let id = session["id"].as_str().unwrap();
    mock_json(server, "GET", &format!("/v1/checkout/sessions/{id}"), 200, session.clone()).await;
}

/// A request the mock received, with the form body decoded.
struct Recorded {
    verb: String,
    route: String,
    query: Vec<(String, String)>,
    form: Vec<(String, String)>,
    idempotency_key: Option<String>,
    authorization: Option<String>,
}

impl Recorded {
    fn form_value(&self, key: &str) -> Option<&str> {
        self.form.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// Values of an array parameter (`key[0]`, `key[1]`, ...) in order.
    fn form_array(&self, key: &str) -> Vec<&str> {
        let prefix = format!("{key}[");
        self.form
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(_, v)| v.as_str())
            .collect()
    }

    fn query_array(&self, key: &str) -> Vec<&str> {
        let prefix = format!("{key}[");
        self.query
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(_, v)| v.as_str())
            .collect()
    }

    fn has_form_key_with_prefix(&self, prefix: &str) -> bool {
        self.form.iter().any(|(k, _)| k.starts_with(prefix))
    }
}

async fn recorded(server: &MockServer) -> Vec<Recorded> {
    server
        .received_requests()
        .await
        .expect("request recording is enabled")
        .into_iter()
        .map(|req| Recorded {
            verb: req.method.to_string(),
            route: req.url.path().to_string(),
            query: req.url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect(),
            form: url::form_urlencoded::parse(&req.body)
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect(),
            idempotency_key: req.headers.get("idempotency-key").map(|v| v.to_str().unwrap().to_string()),
            authorization: req.headers.get("authorization").map(|v| v.to_str().unwrap().to_string()),
        })
        .collect()
}

/// The single request made to `verb route`; fails if there were zero or several.
async fn only_request(server: &MockServer, verb: &str, route: &str) -> Recorded {
    let mut matching: Vec<Recorded> = recorded(server)
        .await
        .into_iter()
        .filter(|r| r.verb == verb && r.route == route)
        .collect();
    assert_eq!(matching.len(), 1, "expected exactly one {verb} {route}");
    matching.remove(0)
}

async fn count_requests(server: &MockServer, verb: &str, route: &str) -> usize {
    recorded(server).await.iter().filter(|r| r.verb == verb && r.route == route).count()
}

fn sign_at(payload: &str, secret: &str, timestamp: i64) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(format!("{timestamp}.{payload}").as_bytes());
    format!("t={timestamp},v1={}", hex::encode(mac.finalize().into_bytes()))
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn event_payload(event_type: &str, object: &Value) -> String {
    json!({
        "id": "evt_ExampleEvent",
        "object": "event",
        "created": 1780346900,
        "livemode": false,
        "pending_webhooks": 1,
        "type": event_type,
        "data": { "object": object },
    })
    .to_string()
}

fn signed_headers(payload: &str) -> axum::http::HeaderMap {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("stripe-signature", sign_at(payload, WEBHOOK_SECRET, now()).parse().unwrap());
    headers
}

/// A paid `payment`-mode session from the tax-bearing fixture (subtotal 2500,
/// total 3000) with the given payer and optional creditee.
fn payment_session(session_id: &str, payer: UserId, creditee: Option<UserId>) -> Value {
    let mut session = fixture("checkout_session_with_tax");
    session["id"] = json!(session_id);
    session["client_reference_id"] = json!(payer.to_string());
    session["metadata"] = match creditee {
        Some(id) => json!({ "creditee_id": id.to_string() }),
        None => json!({}),
    };
    session
}

/// A completed `setup`-mode session for `target`, saving a card.
fn setup_session(session_id: &str, target: UserId, purpose: &str, fingerprint: Option<&str>) -> Value {
    let mut session = fixture("setup_session_card");
    session["id"] = json!(session_id);
    session["client_reference_id"] = json!(target.to_string());
    session["setup_intent"]["metadata"]["purpose"] = json!(purpose);
    match fingerprint {
        Some(fp) => session["setup_intent"]["payment_method"]["card"]["fingerprint"] = json!(fp),
        None => {
            session["setup_intent"]["payment_method"]["card"]
                .as_object_mut()
                .unwrap()
                .remove("fingerprint");
        }
    }
    session
}

async fn balance(pool: &PgPool, user_id: UserId) -> Decimal {
    let mut conn = pool.acquire().await.unwrap();
    Credits::new(&mut conn).get_user_balance(user_id).await.unwrap()
}

async fn transaction_count(pool: &PgPool, source_id: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM credits_transactions WHERE source_id = $1")
        .bind(source_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn is_verified(pool: &PgPool, user_id: UserId) -> bool {
    sqlx::query_scalar("SELECT verified FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn stored_customer_id(pool: &PgPool, user_id: UserId) -> Option<String> {
    sqlx::query_scalar("SELECT payment_provider_id FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn payer(id: UserId, existing_customer: Option<&str>) -> CheckoutPayer {
    CheckoutPayer {
        id,
        email: "buyer@example.test".to_string(),
        payment_provider_id: existing_customer.map(str::to_string),
    }
}

// ---------------------------------------------------------------------------
// Webhook signature validation
// ---------------------------------------------------------------------------

fn completed_payload() -> String {
    event_payload("checkout.session.completed", &payment_session(SESSION_ID, UserId::nil(), None))
}

#[tokio::test]
async fn validate_webhook_rejects_missing_signature_header() {
    let result = offline_provider()
        .validate_webhook(&axum::http::HeaderMap::new(), &completed_payload())
        .await;
    assert!(
        matches!(&result, Err(PaymentError::InvalidData(m)) if m.contains("Missing stripe-signature")),
        "got {result:?}"
    );
}

#[tokio::test]
async fn validate_webhook_rejects_non_ascii_signature_header() {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("stripe-signature", axum::http::HeaderValue::from_bytes(&[0xE9, 0xFF]).unwrap());

    let result = offline_provider().validate_webhook(&headers, &completed_payload()).await;
    assert!(
        matches!(&result, Err(PaymentError::InvalidData(m)) if m.contains("Invalid stripe-signature")),
        "got {result:?}"
    );
}

#[tokio::test]
async fn validate_webhook_rejects_bad_signatures() {
    let payload = completed_payload();
    let provider = offline_provider();

    let wrong_secret = sign_at(&payload, "whsec_someone_else", now());
    let tampered = sign_at(&format!("{payload} "), WEBHOOK_SECRET, now());
    let garbage = "not-a-signature".to_string();
    let wrong_scheme = sign_at(&payload, WEBHOOK_SECRET, now()).replace("v1=", "v0=");

    for (case, signature) in [
        ("signed with another secret", wrong_secret),
        ("payload altered after signing", tampered),
        ("unparseable header", garbage),
        ("unknown signature scheme", wrong_scheme),
    ] {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("stripe-signature", signature.parse().unwrap());
        let result = provider.validate_webhook(&headers, &payload).await;
        assert!(
            matches!(&result, Err(PaymentError::InvalidData(m)) if m.contains("Webhook validation failed")),
            "{case}: got {result:?}"
        );
    }
}

#[tokio::test]
async fn validate_webhook_rejects_expired_timestamp() {
    let payload = completed_payload();
    // Correctly signed, but outside Stripe's five minute replay tolerance.
    let mut headers = axum::http::HeaderMap::new();
    headers.insert("stripe-signature", sign_at(&payload, WEBHOOK_SECRET, now() - 3600).parse().unwrap());

    let result = offline_provider().validate_webhook(&headers, &payload).await;
    assert!(
        matches!(result, Err(PaymentError::InvalidData(_))),
        "a replayed old delivery must be refused"
    );

    // The same payload signed now is accepted, so the timestamp was the cause.
    assert!(
        offline_provider()
            .validate_webhook(&signed_headers(&payload), &payload)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn validate_webhook_extracts_session_id_for_completion_events() {
    let provider = offline_provider();
    for event_type in ["checkout.session.completed", "checkout.session.async_payment_succeeded"] {
        let payload = event_payload(event_type, &payment_session(SESSION_ID, UserId::nil(), None));
        let event = provider
            .validate_webhook(&signed_headers(&payload), &payload)
            .await
            .expect("valid signature")
            .expect("stripe supports webhooks");
        assert_eq!(event.event_type, event_type);
        assert_eq!(event.session_id.as_deref(), Some(SESSION_ID));
    }
}

#[tokio::test]
async fn validate_webhook_passes_other_event_types_through_without_a_session() {
    let payload = event_payload("customer.created", &fixture("customer"));
    let event = offline_provider()
        .validate_webhook(&signed_headers(&payload), &payload)
        .await
        .expect("valid signature")
        .expect("stripe supports webhooks");
    assert_eq!(event.event_type, "customer.created");
    assert_eq!(event.session_id, None);
}

// ---------------------------------------------------------------------------
// Webhook event dispatch
// ---------------------------------------------------------------------------

#[dwctl_test_macros::test]
async fn process_webhook_event_ignores_unrelated_event_types(pool: PgPool) {
    let server = MockServer::start().await;
    let provider = provider(&server);

    for event_type in ["charge.updated", "payment_intent.succeeded", "customer.created"] {
        let event = WebhookEvent {
            event_type: event_type.to_string(),
            session_id: Some(SESSION_ID.to_string()),
        };
        provider
            .process_webhook_event(&pool, &event, &CreditsConfig::default())
            .await
            .unwrap_or_else(|e| panic!("{event_type} should be ignored, got {e:?}"));
    }

    assert!(
        recorded(&server).await.is_empty(),
        "ignored events must not cost a Stripe round trip"
    );
    assert_eq!(transaction_count(&pool, SESSION_ID).await, 0);
}

#[dwctl_test_macros::test]
async fn process_webhook_event_requires_a_session_id(pool: PgPool) {
    let server = MockServer::start().await;
    let event = WebhookEvent {
        event_type: "checkout.session.completed".to_string(),
        session_id: None,
    };

    let result = provider(&server)
        .process_webhook_event(&pool, &event, &CreditsConfig::default())
        .await;
    assert!(
        matches!(&result, Err(PaymentError::InvalidData(m)) if m.contains("session_id")),
        "got {result:?}"
    );
    assert!(recorded(&server).await.is_empty());
}

#[dwctl_test_macros::test]
async fn process_webhook_event_credits_for_both_completion_event_types(pool: PgPool) {
    let server = MockServer::start().await;
    let provider = provider(&server);

    for event_type in ["checkout.session.completed", "checkout.session.async_payment_succeeded"] {
        let user = create_test_user(&pool, Role::StandardUser).await.id;
        let session_id = format!("cs_test_{}", event_type.replace('.', "_"));
        mock_session(&server, &payment_session(&session_id, user, None)).await;

        let event = WebhookEvent {
            event_type: event_type.to_string(),
            session_id: Some(session_id.clone()),
        };
        provider
            .process_webhook_event(&pool, &event, &CreditsConfig::default())
            .await
            .unwrap();

        assert_eq!(
            balance(&pool, user).await,
            Decimal::new(2500, 2),
            "{event_type} should credit the subtotal"
        );
        assert_eq!(transaction_count(&pool, &session_id).await, 1, "{event_type}");
    }
}

// ---------------------------------------------------------------------------
// Payment fulfilment
// ---------------------------------------------------------------------------

#[dwctl_test_macros::test]
async fn paid_session_credits_pretax_amount_and_records_the_customer(pool: PgPool) {
    let server = MockServer::start().await;
    let user = create_test_user(&pool, Role::StandardUser).await.id;
    mock_session(&server, &payment_session(SESSION_ID, user, None)).await;

    provider(&server)
        .process_payment_session(&pool, SESSION_ID, &CreditsConfig::default())
        .await
        .unwrap();

    // Subtotal 25.00, not the 30.00 total that includes 5.00 of tax.
    assert_eq!(balance(&pool, user).await, Decimal::new(2500, 2));
    assert_eq!(transaction_count(&pool, SESSION_ID).await, 1);
    assert!(is_verified(&pool, user).await, "a completed purchase verifies the payer");
    assert_eq!(stored_customer_id(&pool, user).await.as_deref(), Some("cus_ExampleCustomer"));

    let description: String = sqlx::query_scalar("SELECT description FROM credits_transactions WHERE source_id = $1")
        .bind(SESSION_ID)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(description, "Stripe payment");

    // One retrieve, asking for every expansion either checkout mode needs.
    let request = only_request(&server, "GET", &format!("/v1/checkout/sessions/{SESSION_ID}")).await;
    let mut expand = request.query_array("expand");
    expand.sort_unstable();
    assert_eq!(expand, ["line_items", "setup_intent", "setup_intent.payment_method"]);
    assert_eq!(request.authorization.as_deref(), Some("Bearer sk_test_fake"));
}

#[dwctl_test_macros::test]
async fn unpaid_sessions_are_not_credited(pool: PgPool) {
    let server = MockServer::start().await;
    let user = create_test_user(&pool, Role::StandardUser).await.id;

    // An abandoned checkout, and a completed one whose delayed payment method
    // (e.g. a bank debit) has not settled yet: both read as `unpaid`.
    for (case, status) in [("open session", "open"), ("async payment pending", "complete")] {
        let session_id = format!("cs_test_unpaid_{}", status);
        let mut session = payment_session(&session_id, user, None);
        session["payment_status"] = json!("unpaid");
        session["status"] = json!(status);
        mock_session(&server, &session).await;

        let result = provider(&server)
            .process_payment_session(&pool, &session_id, &CreditsConfig::default())
            .await;
        assert!(matches!(result, Err(PaymentError::PaymentNotCompleted)), "{case}: got {result:?}");
        assert_eq!(transaction_count(&pool, &session_id).await, 0, "{case}");
    }

    assert_eq!(balance(&pool, user).await, Decimal::ZERO);
    assert!(!is_verified(&pool, user).await);
}

#[dwctl_test_macros::test]
async fn an_unpaid_session_can_be_fulfilled_once_it_settles(pool: PgPool) {
    // Delayed payment methods: the first delivery sees `unpaid`, the later
    // `async_payment_succeeded` sees `paid` and must credit.
    let server = MockServer::start().await;
    let user = create_test_user(&pool, Role::StandardUser).await.id;
    let provider = provider(&server);

    let mut pending = payment_session(SESSION_ID, user, None);
    pending["payment_status"] = json!("unpaid");
    mock_session(&server, &pending).await;
    let result = provider.process_payment_session(&pool, SESSION_ID, &CreditsConfig::default()).await;
    assert!(matches!(result, Err(PaymentError::PaymentNotCompleted)), "got {result:?}");

    server.reset().await;
    mock_session(&server, &payment_session(SESSION_ID, user, None)).await;
    provider
        .process_payment_session(&pool, SESSION_ID, &CreditsConfig::default())
        .await
        .unwrap();
    assert_eq!(balance(&pool, user).await, Decimal::new(2500, 2));
}

#[dwctl_test_macros::test]
async fn replayed_webhook_and_manual_confirm_credit_exactly_once(pool: PgPool) {
    let server = MockServer::start().await;
    let user = create_test_user(&pool, Role::StandardUser).await.id;
    mock_session(&server, &payment_session(SESSION_ID, user, None)).await;
    let provider = provider(&server);
    let event = WebhookEvent {
        event_type: "checkout.session.completed".to_string(),
        session_id: Some(SESSION_ID.to_string()),
    };

    // Webhook delivery, its redelivery, then the SPA's PATCH /payments/{id}.
    provider
        .process_webhook_event(&pool, &event, &CreditsConfig::default())
        .await
        .unwrap();
    provider
        .process_webhook_event(&pool, &event, &CreditsConfig::default())
        .await
        .unwrap();
    provider
        .process_payment_session(&pool, SESSION_ID, &CreditsConfig::default())
        .await
        .unwrap();

    assert_eq!(balance(&pool, user).await, Decimal::new(2500, 2));
    assert_eq!(transaction_count(&pool, SESSION_ID).await, 1);
    assert_eq!(
        count_requests(&server, "GET", &format!("/v1/checkout/sessions/{SESSION_ID}")).await,
        1,
        "later deliveries should be answered from the ledger, not Stripe"
    );
}

#[dwctl_test_macros::test]
async fn concurrent_fulfilment_of_one_session_credits_once(pool: PgPool) {
    // The webhook and the front-channel confirm can race past the fast-path
    // check; the unique source_id is what keeps the second from double crediting.
    let server = MockServer::start().await;
    let user = create_test_user(&pool, Role::StandardUser).await.id;
    mock_session(&server, &payment_session(SESSION_ID, user, None)).await;
    let provider = provider(&server);
    let credits_config = CreditsConfig::default();

    let (first, second) = tokio::join!(
        provider.process_payment_session(&pool, SESSION_ID, &credits_config),
        provider.process_payment_session(&pool, SESSION_ID, &credits_config),
    );
    for result in [first, second] {
        assert!(matches!(result, Ok(()) | Err(PaymentError::AlreadyProcessed)), "got {result:?}");
    }
    assert_eq!(balance(&pool, user).await, Decimal::new(2500, 2));
    assert_eq!(transaction_count(&pool, SESSION_ID).await, 1);
}

#[dwctl_test_macros::test]
async fn org_payment_credits_the_org_and_verifies_the_payer(pool: PgPool) {
    let server = MockServer::start().await;
    let payer_user = create_test_user(&pool, Role::StandardUser).await;
    let org = create_test_org(&pool, payer_user.id).await;
    // The payer pays on behalf of the org: the org is the creditee.
    mock_session(&server, &payment_session(SESSION_ID, payer_user.id, Some(org.id))).await;

    provider(&server)
        .process_payment_session(&pool, SESSION_ID, &CreditsConfig::default())
        .await
        .unwrap();

    assert_eq!(balance(&pool, org.id).await, Decimal::new(2500, 2));
    assert_eq!(balance(&pool, payer_user.id).await, Decimal::ZERO);
    assert!(is_verified(&pool, payer_user.id).await, "the payer proved they can pay");

    let description: String = sqlx::query_scalar("SELECT description FROM credits_transactions WHERE source_id = $1")
        .bind(SESSION_ID)
        .fetch_one(&pool)
        .await
        .unwrap();
    let payer_name = payer_user.display_name.unwrap_or(payer_user.email);
    assert_eq!(description, format!("Stripe payment from {payer_name}"));
}

#[dwctl_test_macros::test]
async fn sessions_for_unknown_users_are_reported_as_foreign(pool: PgPool) {
    let server = MockServer::start().await;
    let user = create_test_user(&pool, Role::StandardUser).await.id;
    let provider = provider(&server);

    // Another region's creditee, and another region's payer.
    let stranger = UserId::new_v4();
    for (case, session_id, payer, creditee) in [
        ("unknown creditee", "cs_test_unknown_creditee", user, Some(stranger)),
        ("unknown payer", "cs_test_unknown_payer", stranger, None),
    ] {
        mock_session(&server, &payment_session(session_id, payer, creditee)).await;
        let result = provider.process_payment_session(&pool, session_id, &CreditsConfig::default()).await;
        assert!(
            matches!(&result, Err(PaymentError::UnknownReference(id)) if *id == stranger.to_string()),
            "{case}: got {result:?}"
        );
        assert_eq!(transaction_count(&pool, session_id).await, 0, "{case}");
    }
    assert_eq!(balance(&pool, user).await, Decimal::ZERO);
}

#[dwctl_test_macros::test]
async fn malformed_sessions_are_rejected_without_crediting(pool: PgPool) {
    let server = MockServer::start().await;
    let user = create_test_user(&pool, Role::StandardUser).await.id;
    let provider = provider(&server);

    let mut no_reference = payment_session("cs_test_no_reference", user, None);
    no_reference["client_reference_id"] = Value::Null;
    let mut bad_reference = payment_session("cs_test_bad_reference", user, None);
    bad_reference["client_reference_id"] = json!("not-a-uuid");
    let mut bad_creditee = payment_session("cs_test_bad_creditee", user, None);
    bad_creditee["metadata"] = json!({ "creditee_id": "not-a-uuid" });
    let mut no_amount = payment_session("cs_test_no_amount", user, None);
    no_amount["line_items"] = Value::Null;
    no_amount["amount_subtotal"] = Value::Null;

    for (case, session) in [
        ("missing client_reference_id", no_reference),
        ("unparseable client_reference_id", bad_reference),
        ("unparseable creditee_id", bad_creditee),
        ("no amount", no_amount),
    ] {
        let session_id = session["id"].as_str().unwrap().to_string();
        mock_session(&server, &session).await;
        let result = provider
            .process_payment_session(&pool, &session_id, &CreditsConfig::default())
            .await;
        assert!(matches!(result, Err(PaymentError::InvalidData(_))), "{case}: got {result:?}");
        assert_eq!(transaction_count(&pool, &session_id).await, 0, "{case}");
    }
    assert_eq!(balance(&pool, user).await, Decimal::ZERO);
}

#[dwctl_test_macros::test]
async fn stripe_api_failure_while_retrieving_a_session_is_a_provider_error(pool: PgPool) {
    let server = MockServer::start().await;
    mock_json(
        &server,
        "GET",
        &format!("/v1/checkout/sessions/{SESSION_ID}"),
        404,
        json!({ "error": { "type": "invalid_request_error", "code": "resource_missing", "message": "No such checkout.session" } }),
    )
    .await;

    let result = provider(&server)
        .process_payment_session(&pool, SESSION_ID, &CreditsConfig::default())
        .await;
    assert!(matches!(result, Err(PaymentError::ProviderApi(_))), "got {result:?}");
    assert_eq!(transaction_count(&pool, SESSION_ID).await, 0);
}

#[dwctl_test_macros::test]
async fn first_payment_match_is_granted_on_top_of_the_purchase(pool: PgPool) {
    let server = MockServer::start().await;
    let user = create_test_user(&pool, Role::StandardUser).await.id;
    mock_session(&server, &payment_session(SESSION_ID, user, None)).await;
    let credits_config = CreditsConfig {
        first_payment_match_up_to: Decimal::new(1000, 2),
        ..Default::default()
    };

    provider(&server)
        .process_payment_session(&pool, SESSION_ID, &credits_config)
        .await
        .unwrap();

    // 25.00 purchased, plus a match capped at 10.00.
    assert_eq!(balance(&pool, user).await, Decimal::new(3500, 2));
}

#[tokio::test]
async fn get_payment_session_parses_the_retrieved_session() {
    let server = MockServer::start().await;
    let user = UserId::new_v4();
    mock_session(&server, &payment_session(SESSION_ID, user, None)).await;

    let session = provider(&server).get_payment_session(SESSION_ID).await.unwrap();

    assert_eq!(session.creditor_id, user);
    assert_eq!(session.creditee_id, user, "self-payment when no creditee_id metadata is present");
    assert_eq!(session.amount, Decimal::new(2500, 2));
    assert!(session.is_paid);
    assert_eq!(session.payment_provider_id.as_deref(), Some("cus_ExampleCustomer"));

    let request = only_request(&server, "GET", &format!("/v1/checkout/sessions/{SESSION_ID}")).await;
    assert_eq!(request.query_array("expand"), ["line_items"]);
}

// ---------------------------------------------------------------------------
// Setup-mode sessions (card verification and auto top-up enrolment)
// ---------------------------------------------------------------------------

fn credits_with_verification(amount: i64) -> CreditsConfig {
    CreditsConfig {
        verification_credits: Decimal::new(amount, 0),
        ..Default::default()
    }
}

#[dwctl_test_macros::test]
async fn verification_session_verifies_the_account_and_grants_signup_credits(pool: PgPool) {
    let server = MockServer::start().await;
    let user = create_test_user(&pool, Role::StandardUser).await.id;
    mock_session(
        &server,
        &setup_session("cs_test_setup_ok", user, "verification", Some("fp_first_card")),
    )
    .await;
    mock_json(&server, "POST", &format!("/v1/customers/{CUSTOMER_ID}"), 200, fixture("customer")).await;

    provider(&server)
        .process_payment_session(&pool, "cs_test_setup_ok", &credits_with_verification(5))
        .await
        .unwrap();

    assert!(is_verified(&pool, user).await);
    assert_eq!(balance(&pool, user).await, Decimal::new(5, 0));
    assert_eq!(stored_customer_id(&pool, user).await.as_deref(), Some(CUSTOMER_ID));
    // A setup session moves no money, so the only ledger entry is the grant.
    assert_eq!(transaction_count(&pool, "cs_test_setup_ok").await, 0);

    // The saved card becomes the customer's default for later off-session charges.
    let update = only_request(&server, "POST", &format!("/v1/customers/{CUSTOMER_ID}")).await;
    assert_eq!(
        update.form_value("invoice_settings[default_payment_method]"),
        Some("pm_ExamplePaymentMethod")
    );
}

#[dwctl_test_macros::test]
async fn verification_session_replay_does_not_grant_twice(pool: PgPool) {
    let server = MockServer::start().await;
    let user = create_test_user(&pool, Role::StandardUser).await.id;
    mock_session(
        &server,
        &setup_session("cs_test_setup_replay", user, "verification", Some("fp_replay")),
    )
    .await;
    mock_json(&server, "POST", &format!("/v1/customers/{CUSTOMER_ID}"), 200, fixture("customer")).await;
    let provider = provider(&server);

    for _ in 0..2 {
        provider
            .process_payment_session(&pool, "cs_test_setup_replay", &credits_with_verification(5))
            .await
            .unwrap();
    }
    assert_eq!(
        balance(&pool, user).await,
        Decimal::new(5, 0),
        "webhook plus front-channel confirm grant once"
    );
}

#[dwctl_test_macros::test]
async fn verification_with_an_instrument_held_elsewhere_is_refused(pool: PgPool) {
    let server = MockServer::start().await;
    let first = create_test_user(&pool, Role::StandardUser).await.id;
    let second = create_test_user(&pool, Role::StandardUser).await.id;
    mock_json(&server, "POST", &format!("/v1/customers/{CUSTOMER_ID}"), 200, fixture("customer")).await;
    let provider = provider(&server);

    mock_session(&server, &setup_session("cs_test_first", first, "verification", Some("fp_shared"))).await;
    provider
        .process_payment_session(&pool, "cs_test_first", &credits_with_verification(5))
        .await
        .unwrap();

    mock_session(&server, &setup_session("cs_test_second", second, "verification", Some("fp_shared"))).await;
    let result = provider
        .process_payment_session(&pool, "cs_test_second", &credits_with_verification(5))
        .await;

    assert!(matches!(result, Err(PaymentError::InstrumentAlreadyUsed)), "got {result:?}");
    assert!(is_verified(&pool, first).await);
    assert!(!is_verified(&pool, second).await, "one instrument verifies one account");
    assert_eq!(
        balance(&pool, second).await,
        Decimal::ZERO,
        "no signup credits for the second account"
    );
}

#[dwctl_test_macros::test]
async fn auto_topup_enrolment_with_an_instrument_held_elsewhere_succeeds_unverified(pool: PgPool) {
    let server = MockServer::start().await;
    let first = create_test_user(&pool, Role::StandardUser).await.id;
    let second = create_test_user(&pool, Role::StandardUser).await.id;
    mock_json(&server, "POST", &format!("/v1/customers/{CUSTOMER_ID}"), 200, fixture("customer")).await;
    let provider = provider(&server);

    mock_session(&server, &setup_session("cs_test_first", first, "verification", Some("fp_shared"))).await;
    provider
        .process_payment_session(&pool, "cs_test_first", &credits_with_verification(5))
        .await
        .unwrap();

    mock_session(&server, &setup_session("cs_test_second", second, "auto_topup", Some("fp_shared"))).await;
    provider
        .process_payment_session(&pool, "cs_test_second", &credits_with_verification(5))
        .await
        .expect("enrolment still succeeds; the card is saved and can pay for top-ups");

    assert!(!is_verified(&pool, second).await);
    assert_eq!(balance(&pool, second).await, Decimal::ZERO);
    // Still set as the default payment method, which is what auto top-up needs.
    assert_eq!(count_requests(&server, "POST", &format!("/v1/customers/{CUSTOMER_ID}")).await, 2);
}

#[dwctl_test_macros::test]
async fn payment_methods_without_a_fingerprint_cannot_verify(pool: PgPool) {
    let server = MockServer::start().await;
    let user = create_test_user(&pool, Role::StandardUser).await.id;
    mock_json(&server, "POST", &format!("/v1/customers/{CUSTOMER_ID}"), 200, fixture("customer")).await;
    let provider = provider(&server);

    mock_session(&server, &setup_session("cs_test_verify_nofp", user, "verification", None)).await;
    let result = provider
        .process_payment_session(&pool, "cs_test_verify_nofp", &credits_with_verification(5))
        .await;
    assert!(
        matches!(&result, Err(PaymentError::InvalidData(m)) if m.contains("fingerprint")),
        "got {result:?}"
    );

    // Link and similar methods are fine for enrolling auto top-up; they just
    // do not verify.
    mock_session(&server, &setup_session("cs_test_topup_nofp", user, "auto_topup", None)).await;
    provider
        .process_payment_session(&pool, "cs_test_topup_nofp", &credits_with_verification(5))
        .await
        .unwrap();

    assert!(!is_verified(&pool, user).await);
    assert_eq!(balance(&pool, user).await, Decimal::ZERO);
}

#[dwctl_test_macros::test]
async fn setup_sessions_that_did_not_complete_are_refused(pool: PgPool) {
    let server = MockServer::start().await;
    let user = create_test_user(&pool, Role::StandardUser).await.id;
    let provider = provider(&server);

    let mut open = setup_session("cs_test_open", user, "verification", Some("fp_open"));
    open["status"] = json!("open");
    let mut failed_intent = setup_session("cs_test_failed_intent", user, "verification", Some("fp_failed"));
    failed_intent["setup_intent"]["status"] = json!("requires_payment_method");
    let mut unexpanded = setup_session("cs_test_unexpanded", user, "verification", Some("fp_unexpanded"));
    unexpanded["setup_intent"] = json!("seti_ExampleSetupIntent0000");
    let mut no_reference = setup_session("cs_test_no_reference", user, "verification", Some("fp_noref"));
    no_reference["client_reference_id"] = Value::Null;
    let stranger = setup_session("cs_test_stranger", UserId::new_v4(), "verification", Some("fp_stranger"));

    for (case, session) in [
        ("session still open", open),
        ("setup intent failed", failed_intent),
        ("setup intent not expanded", unexpanded),
        ("no client_reference_id", no_reference),
        ("target user unknown", stranger),
    ] {
        let session_id = session["id"].as_str().unwrap().to_string();
        mock_session(&server, &session).await;
        let result = provider
            .process_payment_session(&pool, &session_id, &credits_with_verification(5))
            .await;
        match case {
            "session still open" => assert!(matches!(result, Err(PaymentError::PaymentNotCompleted)), "{case}: got {result:?}"),
            _ => assert!(matches!(result, Err(PaymentError::InvalidData(_))), "{case}: got {result:?}"),
        }
    }

    assert!(!is_verified(&pool, user).await);
    assert_eq!(balance(&pool, user).await, Decimal::ZERO);
}

#[dwctl_test_macros::test]
async fn failing_to_set_the_default_payment_method_does_not_block_verification(pool: PgPool) {
    let server = MockServer::start().await;
    let user = create_test_user(&pool, Role::StandardUser).await.id;
    mock_session(
        &server,
        &setup_session("cs_test_setup_default_fails", user, "verification", Some("fp_besteffort")),
    )
    .await;
    mock_json(
        &server,
        "POST",
        &format!("/v1/customers/{CUSTOMER_ID}"),
        500,
        json!({ "error": { "type": "api_error", "message": "temporary failure" } }),
    )
    .await;

    provider(&server)
        .process_payment_session(&pool, "cs_test_setup_default_fails", &credits_with_verification(5))
        .await
        .unwrap();

    assert!(is_verified(&pool, user).await);
}

#[tokio::test]
async fn process_auto_topup_session_returns_customer_and_user_and_sets_default() {
    let server = MockServer::start().await;
    let user = UserId::new_v4();
    let session = setup_session("cs_test_topup_setup", user, "auto_topup", Some("fp_topup"));
    mock_session(&server, &session).await;
    mock_json(&server, "POST", &format!("/v1/customers/{CUSTOMER_ID}"), 200, fixture("customer")).await;

    let pool = PgPool::connect_lazy("postgres://unused@127.0.0.1:1/unused").unwrap();
    let result = provider(&server)
        .process_auto_topup_session(&pool, "cs_test_topup_setup")
        .await
        .unwrap();

    assert_eq!(result.customer_id.as_deref(), Some(CUSTOMER_ID));
    assert_eq!(result.user_id.as_deref(), Some(user.to_string().as_str()));

    let retrieve = only_request(&server, "GET", "/v1/checkout/sessions/cs_test_topup_setup").await;
    let mut expand = retrieve.query_array("expand");
    expand.sort_unstable();
    assert_eq!(expand, ["setup_intent", "setup_intent.payment_method"]);
    let update = only_request(&server, "POST", &format!("/v1/customers/{CUSTOMER_ID}")).await;
    assert_eq!(
        update.form_value("invoice_settings[default_payment_method]"),
        Some("pm_ExamplePaymentMethod")
    );
}

#[tokio::test]
async fn process_auto_topup_session_refuses_incomplete_setups() {
    let server = MockServer::start().await;
    let user = UserId::new_v4();
    let provider = provider(&server);
    let pool = PgPool::connect_lazy("postgres://unused@127.0.0.1:1/unused").unwrap();

    let mut open = setup_session("cs_test_topup_open", user, "auto_topup", Some("fp"));
    open["status"] = json!("open");
    let mut failed = setup_session("cs_test_topup_failed", user, "auto_topup", Some("fp"));
    failed["setup_intent"]["status"] = json!("canceled");
    let mut unexpanded = setup_session("cs_test_topup_unexpanded", user, "auto_topup", Some("fp"));
    unexpanded["setup_intent"] = Value::Null;

    for (session, expect_incomplete) in [(open, true), (failed, false), (unexpanded, false)] {
        let session_id = session["id"].as_str().unwrap().to_string();
        mock_session(&server, &session).await;
        let result = provider.process_auto_topup_session(&pool, &session_id).await;
        if expect_incomplete {
            assert!(
                matches!(result, Err(PaymentError::PaymentNotCompleted)),
                "{session_id}: got {result:?}"
            );
        } else {
            assert!(matches!(result, Err(PaymentError::InvalidData(_))), "{session_id}: got {result:?}");
        }
    }
    assert_eq!(count_requests(&server, "POST", &format!("/v1/customers/{CUSTOMER_ID}")).await, 0);
}

// ---------------------------------------------------------------------------
// Auto top-up charges
// ---------------------------------------------------------------------------

async fn mock_charge_path(server: &MockServer) {
    mock_json(server, "POST", "/v1/tax/calculations", 200, fixture("tax_calculation")).await;
    mock_json(server, "POST", "/v1/payment_intents", 200, fixture("payment_intent_succeeded")).await;
}

#[tokio::test]
async fn charge_auto_topup_charges_the_tax_inclusive_total_with_idempotency_keys() {
    let server = MockServer::start().await;
    mock_charge_path(&server).await;
    let mut config = stripe_config(&server);
    config.tax_code = Some("txcd_10000000".to_string());

    let id = StripeProvider::from(config)
        .charge_auto_topup(5000, CUSTOMER_ID, "pm_ExampleDefault", "auto-topup-user-1-2026-06-01T10:00")
        .await
        .unwrap();
    assert_eq!(id, "pi_ExampleAutoTopup");

    let tax = only_request(&server, "POST", "/v1/tax/calculations").await;
    assert_eq!(tax.idempotency_key.as_deref(), Some("auto-topup-user-1-2026-06-01T10:00_tax"));
    assert_eq!(tax.form_value("customer"), Some(CUSTOMER_ID));
    assert_eq!(tax.form_value("currency"), Some("usd"));
    assert_eq!(tax.form_value("line_items[0][amount]"), Some("5000"));
    assert_eq!(tax.form_value("line_items[0][reference]"), Some("auto_topup"));
    assert_eq!(tax.form_value("line_items[0][tax_code]"), Some("txcd_10000000"));

    let intent = only_request(&server, "POST", "/v1/payment_intents").await;
    assert_eq!(intent.idempotency_key.as_deref(), Some("auto-topup-user-1-2026-06-01T10:00"));
    assert_eq!(
        intent.form_value("amount"),
        Some("6000"),
        "charge the amount_total Stripe Tax returned"
    );
    assert_eq!(intent.form_value("currency"), Some("usd"));
    assert_eq!(intent.form_value("customer"), Some(CUSTOMER_ID));
    assert_eq!(intent.form_value("payment_method"), Some("pm_ExampleDefault"));
    assert_eq!(intent.form_value("off_session"), Some("true"));
    assert_eq!(intent.form_value("confirm"), Some("true"));
    assert_eq!(intent.form_value("statement_descriptor_suffix"), Some("AUTO-TOPUP"));
    assert_eq!(
        intent.form_value("hooks[inputs][tax][calculation]"),
        Some("taxcalc_ExampleTaxCalculation")
    );
}

#[tokio::test]
async fn charge_auto_topup_omits_the_tax_code_unless_configured() {
    let server = MockServer::start().await;
    mock_charge_path(&server).await;

    provider(&server)
        .charge_auto_topup(5000, CUSTOMER_ID, "pm_ExampleDefault", "key-no-tax-code")
        .await
        .unwrap();

    let tax = only_request(&server, "POST", "/v1/tax/calculations").await;
    assert!(
        !tax.has_form_key_with_prefix("line_items[0][tax_code]"),
        "falls back to the account default"
    );
}

#[tokio::test]
async fn charge_auto_topup_retries_reuse_the_same_idempotency_keys() {
    let server = MockServer::start().await;
    mock_charge_path(&server).await;
    let provider = provider(&server);

    for _ in 0..2 {
        provider
            .charge_auto_topup(5000, CUSTOMER_ID, "pm_ExampleDefault", "retry-key")
            .await
            .unwrap();
    }

    let requests = recorded(&server).await;
    let keys = |route: &str| -> Vec<Option<String>> {
        requests
            .iter()
            .filter(|r| r.route == route)
            .map(|r| r.idempotency_key.clone())
            .collect()
    };
    assert_eq!(keys("/v1/tax/calculations"), vec![Some("retry-key_tax".to_string()); 2]);
    assert_eq!(keys("/v1/payment_intents"), vec![Some("retry-key".to_string()); 2]);
}

#[tokio::test]
async fn charge_auto_topup_maps_declines_by_how_retryable_they_are() {
    for (fixture_name, expected) in [
        ("error_card_declined", AutoTopupDeclineKind::Hard),
        ("error_card_declined_soft", AutoTopupDeclineKind::Soft),
    ] {
        let server = MockServer::start().await;
        mock_json(&server, "POST", "/v1/tax/calculations", 200, fixture("tax_calculation")).await;
        mock_json(&server, "POST", "/v1/payment_intents", 402, fixture(fixture_name)).await;

        let result = provider(&server)
            .charge_auto_topup(5000, CUSTOMER_ID, "pm_ExampleDefault", "decline-key")
            .await;
        assert!(
            matches!(result, Err(PaymentError::AutoTopupDeclined(kind)) if kind == expected),
            "{fixture_name}: got {result:?}"
        );
    }
}

#[tokio::test]
async fn charge_auto_topup_treats_an_in_flight_idempotency_key_as_already_processed() {
    let server = MockServer::start().await;
    mock_json(&server, "POST", "/v1/tax/calculations", 200, fixture("tax_calculation")).await;
    mock_json(&server, "POST", "/v1/payment_intents", 409, fixture("error_idempotency_in_use")).await;

    let result = provider(&server)
        .charge_auto_topup(5000, CUSTOMER_ID, "pm_ExampleDefault", "in-flight-key")
        .await;
    assert!(matches!(result, Err(PaymentError::AlreadyProcessed)), "got {result:?}");
}

#[tokio::test]
async fn charge_auto_topup_stops_when_tax_calculation_fails() {
    let server = MockServer::start().await;
    mock_json(
        &server,
        "POST",
        "/v1/tax/calculations",
        400,
        json!({ "error": { "type": "invalid_request_error", "code": "customer_tax_location_invalid", "message": "no address" } }),
    )
    .await;
    mock_json(&server, "POST", "/v1/payment_intents", 200, fixture("payment_intent_succeeded")).await;

    let result = provider(&server)
        .charge_auto_topup(5000, CUSTOMER_ID, "pm_ExampleDefault", "no-address-key")
        .await;

    assert!(
        matches!(&result, Err(PaymentError::ProviderApi(m)) if m.contains("invalid_request_error")),
        "got {result:?}"
    );
    assert_eq!(
        count_requests(&server, "POST", "/v1/payment_intents").await,
        0,
        "never charge without a tax calculation"
    );
}

#[tokio::test]
async fn charge_auto_topup_maps_other_stripe_failures_to_provider_errors() {
    let server = MockServer::start().await;
    mock_json(&server, "POST", "/v1/tax/calculations", 200, fixture("tax_calculation")).await;
    mock_json(
        &server,
        "POST",
        "/v1/payment_intents",
        500,
        json!({ "error": { "type": "api_error", "message": "internal" } }),
    )
    .await;

    let result = provider(&server)
        .charge_auto_topup(5000, CUSTOMER_ID, "pm_ExampleDefault", "api-error-key")
        .await;
    assert!(
        matches!(&result, Err(PaymentError::ProviderApi(m)) if m.contains("HTTP 500")),
        "got {result:?}"
    );
}

#[tokio::test]
async fn charge_auto_topup_rejects_unusable_idempotency_keys_before_calling_stripe() {
    let server = MockServer::start().await;
    mock_charge_path(&server).await;
    let provider = provider(&server);

    // Too long once the `_tax` suffix is added, then too long outright.
    for key in ["k".repeat(252), "k".repeat(300)] {
        let result = provider.charge_auto_topup(5000, CUSTOMER_ID, "pm_ExampleDefault", &key).await;
        assert!(
            matches!(result, Err(PaymentError::InvalidData(_))),
            "key of {} chars: got {result:?}",
            key.len()
        );
    }
    assert!(recorded(&server).await.is_empty());

    // An empty key only fails on the charge itself (`_tax` alone is valid), but
    // must still never reach the PaymentIntent endpoint.
    let result = provider.charge_auto_topup(5000, CUSTOMER_ID, "pm_ExampleDefault", "").await;
    assert!(matches!(result, Err(PaymentError::InvalidData(_))), "got {result:?}");
    assert_eq!(count_requests(&server, "POST", "/v1/payment_intents").await, 0);
}

#[tokio::test]
async fn default_payment_method_prefers_the_invoice_settings_default() {
    let server = MockServer::start().await;
    mock_json(&server, "GET", &format!("/v1/customers/{CUSTOMER_ID}"), 200, fixture("customer")).await;

    let pm = provider(&server).get_default_payment_method(CUSTOMER_ID).await.unwrap();

    assert_eq!(pm.as_deref(), Some("pm_ExampleDefault"));
    assert_eq!(
        count_requests(&server, "GET", &format!("/v1/customers/{CUSTOMER_ID}/payment_methods")).await,
        0
    );
}

#[tokio::test]
async fn default_payment_method_falls_back_to_the_first_attached_method() {
    let server = MockServer::start().await;
    let mut customer = fixture("customer");
    customer["invoice_settings"]["default_payment_method"] = Value::Null;
    mock_json(&server, "GET", &format!("/v1/customers/{CUSTOMER_ID}"), 200, customer).await;
    mock_json(
        &server,
        "GET",
        &format!("/v1/customers/{CUSTOMER_ID}/payment_methods"),
        200,
        fixture("payment_method_list"),
    )
    .await;

    let pm = provider(&server).get_default_payment_method(CUSTOMER_ID).await.unwrap();

    assert_eq!(pm.as_deref(), Some("pm_ExampleAttached"));
    let list = only_request(&server, "GET", &format!("/v1/customers/{CUSTOMER_ID}/payment_methods")).await;
    assert!(list.query.contains(&("limit".to_string(), "1".to_string())));
}

#[tokio::test]
async fn customers_without_any_payment_method_have_no_default() {
    let server = MockServer::start().await;
    let mut customer = fixture("customer");
    customer["invoice_settings"]["default_payment_method"] = Value::Null;
    mock_json(&server, "GET", &format!("/v1/customers/{CUSTOMER_ID}"), 200, customer).await;
    let mut empty_list = fixture("payment_method_list");
    empty_list["data"] = json!([]);
    mock_json(
        &server,
        "GET",
        &format!("/v1/customers/{CUSTOMER_ID}/payment_methods"),
        200,
        empty_list,
    )
    .await;

    let pm = provider(&server).get_default_payment_method(CUSTOMER_ID).await.unwrap();
    assert_eq!(pm, None);

    // And a deleted customer has none either, without listing anything.
    let deleted_server = MockServer::start().await;
    mock_json(
        &deleted_server,
        "GET",
        &format!("/v1/customers/{CUSTOMER_ID}"),
        200,
        json!({ "id": CUSTOMER_ID, "object": "customer", "deleted": true }),
    )
    .await;
    assert_eq!(
        provider(&deleted_server).get_default_payment_method(CUSTOMER_ID).await.unwrap(),
        None
    );
    assert_eq!(recorded(&deleted_server).await.len(), 1);
}

#[tokio::test]
async fn default_payment_method_surfaces_stripe_failures() {
    let server = MockServer::start().await;
    let error = json!({ "error": { "type": "api_error", "message": "boom" } });
    mock_json(&server, "GET", &format!("/v1/customers/{CUSTOMER_ID}"), 500, error.clone()).await;
    let result = provider(&server).get_default_payment_method(CUSTOMER_ID).await;
    assert!(
        matches!(result, Err(PaymentError::ProviderApi(_))),
        "retrieve failure: got {result:?}"
    );

    let server = MockServer::start().await;
    let mut customer = fixture("customer");
    customer["invoice_settings"]["default_payment_method"] = Value::Null;
    mock_json(&server, "GET", &format!("/v1/customers/{CUSTOMER_ID}"), 200, customer).await;
    mock_json(&server, "GET", &format!("/v1/customers/{CUSTOMER_ID}/payment_methods"), 500, error).await;
    let result = provider(&server).get_default_payment_method(CUSTOMER_ID).await;
    assert!(matches!(result, Err(PaymentError::ProviderApi(_))), "list failure: got {result:?}");
}

// ---------------------------------------------------------------------------
// Outbound request shapes
// ---------------------------------------------------------------------------

fn created_session(url: Option<&str>) -> Value {
    let mut session = fixture("checkout_session_with_tax");
    session["id"] = json!("cs_test_ExampleCreated");
    session["url"] = url.map_or(Value::Null, |u| json!(u));
    session
}

const CHECKOUT_URL: &str = "https://checkout.stripe.test/c/pay/cs_test_ExampleCreated";

#[tokio::test]
async fn create_checkout_session_for_a_new_customer() {
    let server = MockServer::start().await;
    mock_json(&server, "POST", "/v1/checkout/sessions", 200, created_session(Some(CHECKOUT_URL))).await;
    let payer = payer(UserId::new_v4(), None);

    let url = provider(&server)
        .create_checkout_session(
            &payer,
            Some("creditee-123"),
            "https://app.example.test/cancel",
            "https://app.example.test/success",
        )
        .await
        .unwrap();
    assert_eq!(url, CHECKOUT_URL);

    let request = only_request(&server, "POST", "/v1/checkout/sessions").await;
    assert_eq!(request.form_value("mode"), Some("payment"));
    assert_eq!(request.form_value("ui_mode"), Some("hosted_page"));
    assert_eq!(request.form_value("currency"), Some("usd"));
    assert_eq!(request.form_value("client_reference_id"), Some(payer.id.to_string().as_str()));
    assert_eq!(request.form_value("cancel_url"), Some("https://app.example.test/cancel"));
    assert_eq!(request.form_value("success_url"), Some("https://app.example.test/success"));
    assert_eq!(request.form_value("line_items[0][price]"), Some("price_ExampleCredits"));
    assert_eq!(request.form_value("line_items[0][quantity]"), Some("1"));
    assert_eq!(request.form_value("automatic_tax[enabled]"), Some("true"));
    assert_eq!(request.form_value("tax_id_collection[enabled]"), Some("true"));
    assert_eq!(request.form_value("metadata[creditee_id]"), Some("creditee-123"));
    assert_eq!(request.form_array("expand"), ["line_items"]);
    assert_eq!(request.form_value("customer_email"), Some("buyer@example.test"));
    assert_eq!(request.form_value("customer_creation"), Some("always"));
    assert_eq!(request.form_value("customer"), None);
    assert!(!request.has_form_key_with_prefix("invoice_creation"), "off unless configured");
}

#[tokio::test]
async fn create_checkout_session_reuses_an_existing_customer_and_can_create_invoices() {
    let server = MockServer::start().await;
    mock_json(&server, "POST", "/v1/checkout/sessions", 200, created_session(Some(CHECKOUT_URL))).await;
    let mut config = stripe_config(&server);
    config.enable_invoice_creation = true;

    StripeProvider::from(config)
        .create_checkout_session(
            &payer(UserId::new_v4(), Some(CUSTOMER_ID)),
            None,
            "https://app.example.test/cancel",
            "https://app.example.test/success",
        )
        .await
        .unwrap();

    let request = only_request(&server, "POST", "/v1/checkout/sessions").await;
    assert_eq!(request.form_value("customer"), Some(CUSTOMER_ID));
    assert_eq!(request.form_value("customer_update[address]"), Some("auto"));
    assert_eq!(request.form_value("customer_update[name]"), Some("auto"));
    assert_eq!(request.form_value("customer_email"), None);
    assert_eq!(request.form_value("customer_creation"), None);
    assert_eq!(request.form_value("invoice_creation[enabled]"), Some("true"));
    assert!(!request.has_form_key_with_prefix("metadata"), "no creditee means a self payment");
}

#[tokio::test]
async fn create_checkout_session_reports_stripe_failures_and_a_missing_url() {
    let payer = payer(UserId::new_v4(), None);

    let server = MockServer::start().await;
    mock_json(&server, "POST", "/v1/checkout/sessions", 200, created_session(None)).await;
    let result = provider(&server).create_checkout_session(&payer, None, "c", "s").await;
    assert!(
        matches!(&result, Err(PaymentError::ProviderApi(m)) if m.contains("missing URL")),
        "got {result:?}"
    );

    let server = MockServer::start().await;
    mock_json(
        &server,
        "POST",
        "/v1/checkout/sessions",
        400,
        json!({ "error": { "type": "invalid_request_error", "message": "No such price" } }),
    )
    .await;
    let result = provider(&server).create_checkout_session(&payer, None, "c", "s").await;
    assert!(matches!(result, Err(PaymentError::ProviderApi(_))), "got {result:?}");
}

#[tokio::test]
async fn setup_checkout_sessions_differ_in_purpose_and_offered_methods() {
    let target = UserId::new_v4();
    let urls = ("https://app.example.test/cancel", "https://app.example.test/success");

    // Card verification: cards only, so every instrument carries a fingerprint.
    let server = MockServer::start().await;
    mock_json(&server, "POST", "/v1/checkout/sessions", 200, created_session(Some(CHECKOUT_URL))).await;
    let mut config = stripe_config(&server);
    config.auto_topup_terms_of_service_text = Some("auto top-up terms".to_string());
    let verification = StripeProvider::from(config)
        .create_setup_checkout_session(&payer(target, None), urls.0, urls.1)
        .await
        .unwrap();
    assert_eq!(verification, CHECKOUT_URL);

    let request = only_request(&server, "POST", "/v1/checkout/sessions").await;
    assert_eq!(request.form_value("mode"), Some("setup"));
    assert_eq!(request.form_value("ui_mode"), Some("hosted_page"));
    assert_eq!(request.form_value("client_reference_id"), Some(target.to_string().as_str()));
    assert_eq!(request.form_array("payment_method_types"), ["card"]);
    assert_eq!(request.form_value("setup_intent_data[metadata][purpose]"), Some("verification"));
    assert_eq!(
        request.form_value("setup_intent_data[description]"),
        Some("Payment method verification")
    );
    assert_eq!(request.form_value("custom_text[submit][message]"), Some("Verify payment method"));
    assert_eq!(
        request.form_value("custom_text[terms_of_service_acceptance][message]"),
        Some("auto top-up terms"),
        "falls back to the auto top-up terms when no setup-specific text is set"
    );
    assert_eq!(request.form_value("consent_collection[terms_of_service]"), Some("required"));
    assert_eq!(request.form_value("tax_id_collection[enabled]"), Some("true"));
    assert_eq!(request.form_value("customer_email"), Some("buyer@example.test"));
    assert_eq!(request.form_value("customer_creation"), Some("always"));

    // A dedicated setup text wins over the fallback.
    let server = MockServer::start().await;
    mock_json(&server, "POST", "/v1/checkout/sessions", 200, created_session(Some(CHECKOUT_URL))).await;
    let mut config = stripe_config(&server);
    config.auto_topup_terms_of_service_text = Some("auto top-up terms".to_string());
    config.setup_terms_of_service_text = Some("verification terms".to_string());
    StripeProvider::from(config)
        .create_setup_checkout_session(&payer(target, Some(CUSTOMER_ID)), urls.0, urls.1)
        .await
        .unwrap();
    let request = only_request(&server, "POST", "/v1/checkout/sessions").await;
    assert_eq!(
        request.form_value("custom_text[terms_of_service_acceptance][message]"),
        Some("verification terms")
    );
    assert_eq!(request.form_value("customer"), Some(CUSTOMER_ID));
    assert_eq!(request.form_value("customer_update[address]"), Some("auto"));

    // Auto top-up enrolment: also Link and SEPA, with its own purpose and copy.
    let server = MockServer::start().await;
    mock_json(&server, "POST", "/v1/checkout/sessions", 200, created_session(Some(CHECKOUT_URL))).await;
    provider(&server)
        .create_auto_topup_checkout_session(&payer(target, None), urls.0, urls.1)
        .await
        .unwrap();
    let request = only_request(&server, "POST", "/v1/checkout/sessions").await;
    assert_eq!(request.form_value("mode"), Some("setup"));
    assert_eq!(request.form_array("payment_method_types"), ["card", "link", "sepa_debit"]);
    assert_eq!(request.form_value("setup_intent_data[metadata][purpose]"), Some("auto_topup"));
    assert_eq!(request.form_value("setup_intent_data[description]"), Some("Auto top-up setup"));
    assert_eq!(request.form_value("custom_text[submit][message]"), Some("Set up auto top-up"));
    assert!(
        !request.has_form_key_with_prefix("custom_text[terms_of_service_acceptance]"),
        "no terms text configured, none shown"
    );
}

#[tokio::test]
async fn setup_checkout_session_without_a_url_is_an_error() {
    let server = MockServer::start().await;
    mock_json(&server, "POST", "/v1/checkout/sessions", 200, created_session(None)).await;
    let result = provider(&server)
        .create_auto_topup_checkout_session(&payer(UserId::new_v4(), None), "c", "s")
        .await;
    assert!(
        matches!(&result, Err(PaymentError::ProviderApi(m)) if m.contains("missing URL")),
        "got {result:?}"
    );

    let server = MockServer::start().await;
    mock_json(
        &server,
        "POST",
        "/v1/checkout/sessions",
        500,
        json!({ "error": { "type": "api_error", "message": "boom" } }),
    )
    .await;
    let result = provider(&server)
        .create_setup_checkout_session(&payer(UserId::new_v4(), None), "c", "s")
        .await;
    assert!(matches!(result, Err(PaymentError::ProviderApi(_))), "got {result:?}");
}

#[tokio::test]
async fn accrue_invoice_item_creates_a_pending_item_with_an_idempotency_key() {
    let server = MockServer::start().await;
    mock_json(&server, "POST", "/v1/invoiceitems", 200, fixture("invoice_item")).await;

    let id = provider(&server)
        .accrue_invoice_item(5000, CUSTOMER_ID, "Auto top-up accrual", "accrue-key-1")
        .await
        .unwrap();
    assert_eq!(id, "ii_ExampleInvoiceItem");

    let request = only_request(&server, "POST", "/v1/invoiceitems").await;
    assert_eq!(request.idempotency_key.as_deref(), Some("accrue-key-1"));
    assert_eq!(request.form_value("customer"), Some(CUSTOMER_ID));
    assert_eq!(request.form_value("amount"), Some("5000"));
    assert_eq!(request.form_value("currency"), Some("usd"));
    assert_eq!(request.form_value("description"), Some("Auto top-up accrual"));
    assert_eq!(
        request.form_value("invoice"),
        None,
        "pending item: swept onto the next invoice, not billed now"
    );
}

#[tokio::test]
async fn accrue_invoice_item_rejects_bad_keys_and_reports_stripe_failures() {
    let server = MockServer::start().await;
    let result = provider(&server).accrue_invoice_item(5000, CUSTOMER_ID, "d", "").await;
    assert!(matches!(result, Err(PaymentError::InvalidData(_))), "got {result:?}");
    assert!(recorded(&server).await.is_empty());

    mock_json(
        &server,
        "POST",
        "/v1/invoiceitems",
        500,
        json!({ "error": { "type": "api_error", "message": "boom" } }),
    )
    .await;
    let result = provider(&server).accrue_invoice_item(5000, CUSTOMER_ID, "d", "key").await;
    assert!(matches!(result, Err(PaymentError::ProviderApi(_))), "got {result:?}");
}

#[tokio::test]
async fn customer_has_address_reflects_the_stripe_customer() {
    let with_address = MockServer::start().await;
    mock_json(
        &with_address,
        "GET",
        &format!("/v1/customers/{CUSTOMER_ID}"),
        200,
        fixture("customer"),
    )
    .await;
    assert!(provider(&with_address).customer_has_address(CUSTOMER_ID).await.unwrap());

    let without = MockServer::start().await;
    let mut customer = fixture("customer");
    customer["address"] = Value::Null;
    mock_json(&without, "GET", &format!("/v1/customers/{CUSTOMER_ID}"), 200, customer).await;
    assert!(!provider(&without).customer_has_address(CUSTOMER_ID).await.unwrap());

    let deleted = MockServer::start().await;
    mock_json(
        &deleted,
        "GET",
        &format!("/v1/customers/{CUSTOMER_ID}"),
        200,
        json!({ "id": CUSTOMER_ID, "object": "customer", "deleted": true }),
    )
    .await;
    assert!(!provider(&deleted).customer_has_address(CUSTOMER_ID).await.unwrap());

    let failing = MockServer::start().await;
    mock_json(
        &failing,
        "GET",
        &format!("/v1/customers/{CUSTOMER_ID}"),
        500,
        json!({ "error": { "type": "api_error", "message": "boom" } }),
    )
    .await;
    let result = provider(&failing).customer_has_address(CUSTOMER_ID).await;
    assert!(matches!(result, Err(PaymentError::ProviderApi(_))), "got {result:?}");
}

#[tokio::test]
async fn create_customer_sends_email_and_optional_name() {
    let server = MockServer::start().await;
    mock_json(&server, "POST", "/v1/customers", 200, fixture("customer")).await;
    let provider = provider(&server);

    assert_eq!(
        provider
            .create_customer("buyer@example.test", Some("Example Buyer Ltd"))
            .await
            .unwrap(),
        CUSTOMER_ID
    );
    assert_eq!(provider.create_customer("other@example.test", None).await.unwrap(), CUSTOMER_ID);

    let requests: Vec<Recorded> = recorded(&server).await.into_iter().filter(|r| r.route == "/v1/customers").collect();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].form_value("email"), Some("buyer@example.test"));
    assert_eq!(requests[0].form_value("name"), Some("Example Buyer Ltd"));
    assert_eq!(requests[1].form_value("email"), Some("other@example.test"));
    assert_eq!(requests[1].form_value("name"), None);
}

#[tokio::test]
async fn api_base_url_with_a_path_prefix_keeps_the_prefix() {
    // An egress proxy mounted under a path, configured without a trailing slash.
    let server = MockServer::start().await;
    mock_json(&server, "POST", "/stripe/v1/customers", 200, fixture("customer")).await;
    let provider = StripeProvider::from(stripe_config_at(&format!("{}/stripe", server.uri())));

    assert_eq!(provider.create_customer("buyer@example.test", None).await.unwrap(), CUSTOMER_ID);
    only_request(&server, "POST", "/stripe/v1/customers").await;
}

#[tokio::test]
async fn create_customer_reports_stripe_failures() {
    let server = MockServer::start().await;
    mock_json(
        &server,
        "POST",
        "/v1/customers",
        400,
        json!({ "error": { "type": "invalid_request_error", "message": "Invalid email" } }),
    )
    .await;
    let result = provider(&server).create_customer("nope", None).await;
    assert!(matches!(result, Err(PaymentError::ProviderApi(_))), "got {result:?}");
}

#[tokio::test]
async fn create_billing_portal_session_returns_the_portal_url() {
    let server = MockServer::start().await;
    mock_json(
        &server,
        "POST",
        "/v1/billing_portal/sessions",
        200,
        fixture("billing_portal_session"),
    )
    .await;
    let provider = provider(&server);

    let url = provider
        .create_billing_portal_session(CUSTOMER_ID, "https://app.example.test/billing")
        .await
        .unwrap();
    assert_eq!(url, "https://billing.stripe.test/session/example");

    let request = only_request(&server, "POST", "/v1/billing_portal/sessions").await;
    assert_eq!(request.form_value("customer"), Some(CUSTOMER_ID));
    assert_eq!(request.form_value("return_url"), Some("https://app.example.test/billing"));

    let failing = MockServer::start().await;
    mock_json(
        &failing,
        "POST",
        "/v1/billing_portal/sessions",
        400,
        json!({ "error": { "type": "invalid_request_error", "message": "no such customer" } }),
    )
    .await;
    let result = self::provider(&failing).create_billing_portal_session("cus_missing", "r").await;
    assert!(matches!(result, Err(PaymentError::ProviderApi(_))), "got {result:?}");
}

// ---------------------------------------------------------------------------
// POST /webhooks/payments with the Stripe provider configured
// ---------------------------------------------------------------------------

async fn webhook_server(pool: &PgPool, stripe: &MockServer) -> TestServer {
    let mut config = create_test_config();
    config.payment = Some(PaymentConfig::Stripe(stripe_config(stripe)));
    let state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;
    let app = Router::new()
        .route("/webhooks/payments", post(crate::api::handlers::payments::webhook_handler))
        .with_state(state);
    TestServer::new(app).unwrap()
}

#[dwctl_test_macros::test]
async fn webhook_endpoint_rejects_bad_signatures_with_400(pool: PgPool) {
    let stripe = MockServer::start().await;
    let server = webhook_server(&pool, &stripe).await;
    let user = create_test_user(&pool, Role::StandardUser).await.id;
    mock_session(&stripe, &payment_session(SESSION_ID, user, None)).await;
    let payload = event_payload("checkout.session.completed", &payment_session(SESSION_ID, user, None));

    let no_header = server.post("/webhooks/payments").text(payload.clone()).await;
    no_header.assert_status(axum::http::StatusCode::BAD_REQUEST);

    let wrong_secret = server
        .post("/webhooks/payments")
        .add_header("stripe-signature", sign_at(&payload, "whsec_someone_else", now()))
        .text(payload.clone())
        .await;
    wrong_secret.assert_status(axum::http::StatusCode::BAD_REQUEST);

    let stale = server
        .post("/webhooks/payments")
        .add_header("stripe-signature", sign_at(&payload, WEBHOOK_SECRET, now() - 3600))
        .text(payload.clone())
        .await;
    stale.assert_status(axum::http::StatusCode::BAD_REQUEST);

    assert_eq!(transaction_count(&pool, SESSION_ID).await, 0);
    assert!(
        recorded(&stripe).await.is_empty(),
        "an unauthenticated delivery must not reach Stripe or the ledger"
    );
}

#[dwctl_test_macros::test]
async fn webhook_endpoint_credits_a_signed_checkout_completion_once(pool: PgPool) {
    let stripe = MockServer::start().await;
    let server = webhook_server(&pool, &stripe).await;
    let user = create_test_user(&pool, Role::StandardUser).await.id;
    let session = payment_session(SESSION_ID, user, None);
    mock_session(&stripe, &session).await;
    let payload = event_payload("checkout.session.completed", &session);

    assert_eq!(balance(&pool, user).await, Decimal::ZERO);
    // Stripe redelivers on any non-2xx and at-least-once otherwise, so a replay
    // must also be a 200 that changes nothing.
    for _ in 0..2 {
        let response = server
            .post("/webhooks/payments")
            .add_header("stripe-signature", sign_at(&payload, WEBHOOK_SECRET, now()))
            .text(payload.clone())
            .await;
        response.assert_status_ok();
    }

    assert_eq!(balance(&pool, user).await, Decimal::new(2500, 2));
    assert_eq!(transaction_count(&pool, SESSION_ID).await, 1);
}

#[dwctl_test_macros::test]
async fn webhook_endpoint_acks_unrelated_events_and_foreign_sessions(pool: PgPool) {
    let stripe = MockServer::start().await;
    let server = webhook_server(&pool, &stripe).await;

    let unrelated = event_payload("customer.created", &fixture("customer"));
    server
        .post("/webhooks/payments")
        .add_header("stripe-signature", sign_at(&unrelated, WEBHOOK_SECRET, now()))
        .text(unrelated)
        .await
        .assert_status_ok();
    assert!(recorded(&stripe).await.is_empty());

    // A session belonging to a user this plane has never heard of: 200, so
    // Stripe stops redelivering, and nothing is credited.
    let foreign = payment_session("cs_test_foreign", UserId::new_v4(), None);
    mock_session(&stripe, &foreign).await;
    let payload = event_payload("checkout.session.completed", &foreign);
    server
        .post("/webhooks/payments")
        .add_header("stripe-signature", sign_at(&payload, WEBHOOK_SECRET, now()))
        .text(payload)
        .await
        .assert_status_ok();
    assert_eq!(transaction_count(&pool, "cs_test_foreign").await, 0);
}
