//! HTTP-API email transport clients.
//!
//! Used by [`crate::email::EmailService`] when the configured transport is
//! `EmailTransportConfig::Http`. Each provider implements [`HttpEmailClient`];
//! the [`crate::config::EmailProvider`] enum selects which.
//!
//! Hosted transactional providers typically offer a single API key (no
//! per-host TLS handshake) and structured JSON error responses we can
//! classify as permanent vs transient for retry logic.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::email::EmailEnvelope;

/// Error returned by an [`HttpEmailClient`].
///
/// The `is_transient` distinction is the input a retrying worker needs to
/// decide whether to re-enqueue: permanent errors (validation failures,
/// unverified sender domain) shouldn't be retried; transient errors
/// (provider 5xx, rate-limit, network blip) should.
#[derive(Debug, Error)]
pub enum HttpEmailError {
    /// Provider permanently rejected the request (4xx other than 429).
    /// E.g. unverified sender domain, invalid recipient, malformed payload.
    /// A retrying worker should NOT re-enqueue.
    #[error("provider permanent error (HTTP {status}): {message}")]
    Permanent { status: u16, message: String },
    /// Provider transiently failed (5xx or 429 rate-limit).
    /// A retrying worker should re-enqueue with backoff.
    #[error("provider transient error (HTTP {status}): {message}")]
    Transient { status: u16, message: String },
    /// Network / TLS / DNS failure before any response was received.
    /// Treated as transient.
    #[error("network error contacting provider: {0}")]
    Network(String),
    /// Local construction error (a bug in this module or its caller).
    #[error("client error: {0}")]
    Client(String),
}

impl HttpEmailError {
    /// Whether a retry worker should re-enqueue the job after this error.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transient { .. } | Self::Network(_))
    }
}

/// Abstract transactional-email HTTP client.
///
/// Implementations translate between [`EmailEnvelope`] and the provider's
/// wire format. The trait stays small on purpose — adding a provider means
/// one new struct, not a refactor.
#[async_trait]
pub trait HttpEmailClient: Send + Sync + std::fmt::Debug {
    /// Provider identifier for metrics/logging (e.g. `"resend"`).
    fn provider_name(&self) -> &'static str;
    /// Send a prepared email envelope.
    async fn send(&self, envelope: &EmailEnvelope) -> Result<(), HttpEmailError>;
}

/// Resend's public API base URL. Overridable via
/// `EmailTransportConfig::Http { base_url: Some(...) }` for tests.
const DEFAULT_RESEND_BASE_URL: &str = "https://api.resend.com";

/// HTTP client for the Resend transactional email API.
///
/// API reference: <https://resend.com/docs/api-reference/emails/send-email>.
#[derive(Debug, Clone)]
pub struct ResendClient {
    http: Client,
    api_key: String,
    base_url: String,
}

impl ResendClient {
    pub fn new(api_key: impl Into<String>, base_url: Option<String>) -> Result<Self, HttpEmailError> {
        // 15s covers provider response time comfortably. Transient timeouts
        // surface as `HttpEmailError::Network`.
        let http = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| HttpEmailError::Client(format!("build reqwest client: {e}")))?;
        Ok(Self {
            http,
            api_key: api_key.into(),
            base_url: base_url.unwrap_or_else(|| DEFAULT_RESEND_BASE_URL.to_string()),
        })
    }
}

/// Resend send-email request body (subset of the documented fields).
#[derive(Serialize)]
struct ResendSendBody<'a> {
    from: String,
    to: Vec<String>,
    subject: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    html: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to: Option<Vec<String>>,
}

/// Resend error response body. `name` is a stable error code (e.g.
/// `validation_error`); `message` is human-readable.
#[derive(Deserialize, Default)]
struct ResendErrorBody {
    #[serde(default)]
    message: String,
    #[serde(default)]
    name: String,
}

#[async_trait]
impl HttpEmailClient for ResendClient {
    fn provider_name(&self) -> &'static str {
        "resend"
    }

    async fn send(&self, envelope: &EmailEnvelope) -> Result<(), HttpEmailError> {
        let (html, text) = match &envelope.body {
            crate::email::EmailBody::Html(b) => (Some(b.as_str()), None),
            crate::email::EmailBody::Text(b) => (None, Some(b.as_str())),
        };

        let body = ResendSendBody {
            from: envelope.from.to_string(),
            to: vec![envelope.to.to_string()],
            subject: envelope.subject.as_str(),
            html,
            text,
            reply_to: envelope.reply_to.as_ref().map(|m| vec![m.to_string()]),
        };

        let resp = self
            .http
            .post(format!("{}/emails", self.base_url.trim_end_matches('/')))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| HttpEmailError::Network(e.to_string()))?;

        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }

        let code = status.as_u16();
        let err: ResendErrorBody = resp.json().await.unwrap_or_default();
        let message = if err.message.is_empty() {
            format!("HTTP {code}")
        } else if err.name.is_empty() {
            err.message
        } else {
            format!("{}: {}", err.name, err.message)
        };

        // 429 (rate-limit) and 5xx are transient; other 4xx are permanent.
        if code == 429 || (500..600).contains(&code) {
            Err(HttpEmailError::Transient { status: code, message })
        } else {
            Err(HttpEmailError::Permanent { status: code, message })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::email::{EmailBody, EmailEnvelope};
    use lettre::message::Mailbox;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_envelope() -> EmailEnvelope {
        EmailEnvelope {
            template_id: "test_template",
            from: Mailbox::new(Some("Doubleword".to_string()), "noreply@doubleword.ai".parse().unwrap()),
            to: Mailbox::new(Some("Alice".to_string()), "alice@example.com".parse().unwrap()),
            reply_to: None,
            subject: "hello".to_string(),
            body: EmailBody::Html("<p>hi</p>".to_string()),
        }
    }

    #[tokio::test]
    async fn resend_client_success_sends_expected_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/emails"))
            .and(header("authorization", "Bearer test-key"))
            .and(body_json(serde_json::json!({
                "from": "Doubleword <noreply@doubleword.ai>",
                "to": ["Alice <alice@example.com>"],
                "subject": "hello",
                "html": "<p>hi</p>",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": "abc" })))
            .expect(1)
            .mount(&server)
            .await;

        let client = ResendClient::new("test-key", Some(server.uri())).unwrap();
        client.send(&test_envelope()).await.expect("send succeeds");
    }

    #[tokio::test]
    async fn resend_client_text_body_sets_text_field() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_json(serde_json::json!({
                "from": "Doubleword <noreply@doubleword.ai>",
                "to": ["Alice <alice@example.com>"],
                "subject": "hello",
                "text": "hi",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": "abc" })))
            .expect(1)
            .mount(&server)
            .await;

        let mut env = test_envelope();
        env.body = EmailBody::Text("hi".to_string());
        let client = ResendClient::new("k", Some(server.uri())).unwrap();
        client.send(&env).await.expect("send succeeds");
    }

    #[tokio::test]
    async fn resend_client_includes_reply_to_when_set() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_json(serde_json::json!({
                "from": "Doubleword <noreply@doubleword.ai>",
                "to": ["Alice <alice@example.com>"],
                "subject": "hello",
                "html": "<p>hi</p>",
                "reply_to": ["user@example.com"],
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "id": "abc" })))
            .expect(1)
            .mount(&server)
            .await;

        let mut env = test_envelope();
        env.reply_to = Some(Mailbox::new(None, "user@example.com".parse().unwrap()));
        let client = ResendClient::new("k", Some(server.uri())).unwrap();
        client.send(&env).await.expect("send succeeds with reply-to");
    }

    #[tokio::test]
    async fn resend_client_permanent_error_on_422() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({
                "name": "validation_error",
                "message": "from address is unverified"
            })))
            .mount(&server)
            .await;

        let client = ResendClient::new("test-key", Some(server.uri())).unwrap();
        let err = client.send(&test_envelope()).await.unwrap_err();
        assert!(matches!(err, HttpEmailError::Permanent { status: 422, .. }), "got: {err:?}");
        assert!(!err.is_transient());
    }

    #[tokio::test]
    async fn resend_client_transient_error_on_429() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_json(serde_json::json!({
                "name": "rate_limit",
                "message": "too many requests"
            })))
            .mount(&server)
            .await;

        let client = ResendClient::new("test-key", Some(server.uri())).unwrap();
        let err = client.send(&test_envelope()).await.unwrap_err();
        assert!(matches!(err, HttpEmailError::Transient { status: 429, .. }), "got: {err:?}");
        assert!(err.is_transient());
    }

    #[tokio::test]
    async fn resend_client_transient_error_on_5xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let client = ResendClient::new("test-key", Some(server.uri())).unwrap();
        let err = client.send(&test_envelope()).await.unwrap_err();
        assert!(matches!(err, HttpEmailError::Transient { status: 503, .. }), "got: {err:?}");
        assert!(err.is_transient());
    }

    #[tokio::test]
    async fn resend_client_network_error_is_transient() {
        // Point the client at a port that's almost certainly not listening.
        let client = ResendClient::new("k", Some("http://127.0.0.1:1".to_string())).unwrap();
        let err = client.send(&test_envelope()).await.unwrap_err();
        assert!(matches!(err, HttpEmailError::Network(_)), "got: {err:?}");
        assert!(err.is_transient());
    }
}
