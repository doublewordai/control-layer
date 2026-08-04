//! Client for **tokenizer-svc**: the sole source of token counts for
//! cache *writes* (reads need no tokenization — the count is stored on the entry).
//!
//! tokenizer-svc is a dumb string->count service. We send the prompt segments to
//! count; it returns per-segment counts, running cumulative totals, and a
//! `tokenizer_version` (which becomes part of the index key). A model with no
//! tokenizer mapping yields `422 UNMAPPED_MODEL`, surfaced as
//! [`TokenizerError::Unmapped`] so the caller skips caching for that request —
//! full price, no customer-facing error.

use std::time::Duration;

use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::metrics as cache_metrics;

/// HTTP client for a tokenizer-svc deployment.
#[derive(Clone)]
pub struct TokenizerClient {
    http: Client,
    base_url: String,
}

#[derive(Debug, Serialize)]
struct TokenizeRequest<'a> {
    virtual_model: &'a str,
    segments: &'a [String],
}

/// tokenizer-svc `/v1/tokenize` response. The write-side count at a breakpoint is
/// the `cumulative` value at that segment; `total` == `cumulative.last()`.
#[derive(Debug, Clone, Deserialize)]
pub struct TokenizeResponse {
    pub virtual_model: String,
    pub tokenizer_version: String,
    pub segment_counts: Vec<u32>,
    pub cumulative: Vec<u32>,
    pub total: u32,
}

/// One entry from `/v1/models` — a model this image has a tokenizer baked for.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelInfo {
    pub alias: String,
    pub hf_repo: String,
    pub tokenizer_version: String,
    /// Present when the alias is render-capable (tokenizer-svc ≥ 0.3.0): the hash of
    /// the chat-template source (or encoder version). Folded into the index scope
    /// under exact counting so template changes age entries out.
    #[serde(default)]
    pub template_version: Option<String>,
}

#[derive(Debug, Serialize)]
struct RenderRequest<'a> {
    virtual_model: &'a str,
    messages: &'a serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a serde_json::Value>,
    /// Applies to the MAIN render only; prefix renders are always generation-prompt-off
    /// server-side (a prefix up to a marker is not a generation view).
    add_generation_prompt: bool,
    /// Truncation points to also count (≤5, canonical tools→messages order). The svc
    /// renders each truncated view independently and returns `prefix_counts` in request
    /// order. Cache-agnostic: dwctl translates markers into these; the svc never sees
    /// `cache_control`.
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    prefixes: &'a [WirePrefix],
    /// The classifier only needs counts; echoing a 64k-token render would be waste.
    return_rendered: bool,
}

/// One truncation point, in the svc's wire shape. Indices are INCLUSIVE and refer to the
/// request as transmitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum WirePrefix {
    /// `tools[0..=tools]`, no messages.
    Tools { tools: usize },
    /// All tools + `messages[0..=message]` complete.
    Message { message: usize },
    /// All tools + `messages[0..message]` + message `message` truncated to
    /// `content[0..=block]`, tool_calls removed.
    Block { message: usize, block: usize },
    /// All tools + `messages[0..message]` + message `message` with full content +
    /// `tool_calls[0..=tool_call]`.
    ToolCall { message: usize, tool_call: usize },
}

/// tokenizer-svc `/v1/render` response (0.3.0+): exact chat-templated counts — the
/// same bytes the engine tokenizes.
#[derive(Debug, Clone, Deserialize)]
pub struct RenderResponse {
    pub virtual_model: String,
    pub tokenizer_version: String,
    pub template_version: String,
    /// Token count of the MAIN render (generation prompt included when requested).
    pub total: u32,
    /// One count per requested prefix, in request order. `None` = the template refused
    /// that truncated view (the caller backfills from raw counts).
    #[serde(default)]
    pub prefix_counts: Vec<Option<u32>>,
}

#[derive(Debug, Deserialize)]
struct ModelsResponse {
    models: Vec<ModelInfo>,
}

#[derive(Debug, thiserror::Error)]
pub enum TokenizerError {
    /// The model has no tokenizer mapping (`422 UNMAPPED_MODEL`). The caller skips
    /// caching for this request — full price, no customer-facing error.
    #[error("model {0:?} is not mapped in tokenizer-svc")]
    Unmapped(String),
    /// `/v1/render` cannot template this alias or conversation (`422 NO_CHAT_TEMPLATE`
    /// or `400 TEMPLATE_RENDER_FAILED`). The caller falls back to raw-segment counting
    /// — today's accuracy, not an outage.
    #[error("tokenizer-svc cannot render for {0:?}: {1}")]
    RenderUnsupported(String, String),
    #[error("tokenizer-svc request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("tokenizer-svc returned {status}: {body}")]
    Status { status: u16, body: String },
}

pub type TokenizerResult<T> = std::result::Result<T, TokenizerError>;

impl TokenizerClient {
    /// Build a client with a sane request timeout — tokenizer-svc sits on the
    /// classify path (deadline-bounded), so a slow/hung call must not hang it.
    pub fn new(base_url: impl Into<String>) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client builds with default TLS");
        Self::with_client(http, base_url)
    }

    pub fn with_client(http: Client, base_url: impl Into<String>) -> Self {
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    /// Count tokens for each segment. Special tokens are NOT added (the service is
    /// configured `add_special_tokens=false`), so counts are additive across
    /// segments and the totals reconcile.
    pub async fn tokenize(&self, virtual_model: &str, segments: &[String]) -> TokenizerResult<TokenizeResponse> {
        let start = std::time::Instant::now();
        let size = cache_metrics::tokenize_size_bucket(segments.iter().map(String::len).sum());
        let result = self.tokenize_inner(virtual_model, segments).await;
        // Label cardinality guard: only a name tokenizer-svc actually ACCEPTED becomes a
        // `model` label — an Ok response proves the alias is on the svc's baked map (a bounded,
        // admin-controlled set), regardless of what this method was called with. Every error
        // path gets a fixed label: `unmapped` (svc rejected the name) or `error` (timeout /
        // connection / HTTP — the name was never validated, and error latency is service-wide,
        // not model-specific). So an unvetted name can never mint a new series, even mid-outage.
        let model_label = match &result {
            Ok(_) => virtual_model,
            Err(TokenizerError::Unmapped(_)) => "unmapped",
            Err(_) => "error",
        };
        cache_metrics::record_tokenizer_duration(model_label, size, start.elapsed().as_secs_f64());
        cache_metrics::record_tokenizer_request(match &result {
            Ok(_) => "ok",
            Err(TokenizerError::Unmapped(_)) => "unmapped_422",
            Err(TokenizerError::Status { .. }) => "http_error",
            Err(_) => "transport_error",
        });
        result
    }

    /// Exact chat-templated counts via `/v1/render` (tokenizer-svc ≥ 0.3.0). Same
    /// deadline/error/metrics discipline as `tokenize`; `RenderUnsupported` (422
    /// NO_CHAT_TEMPLATE / 400 TEMPLATE_RENDER_FAILED) tells the caller to fall back to
    /// raw-segment counting rather than skip caching.
    pub async fn render(
        &self,
        virtual_model: &str,
        messages: &serde_json::Value,
        tools: Option<&serde_json::Value>,
        add_generation_prompt: bool,
        prefixes: &[WirePrefix],
    ) -> TokenizerResult<RenderResponse> {
        let start = std::time::Instant::now();
        let size = cache_metrics::tokenize_size_bucket(messages.to_string().len() + tools.map(|t| t.to_string().len()).unwrap_or(0));
        let result = self
            .render_inner(virtual_model, messages, tools, add_generation_prompt, prefixes)
            .await;
        let model_label = match &result {
            Ok(_) => virtual_model,
            Err(TokenizerError::Unmapped(_)) => "unmapped",
            Err(TokenizerError::RenderUnsupported(..)) => "render_unsupported",
            Err(_) => "error",
        };
        cache_metrics::record_tokenizer_duration(model_label, size, start.elapsed().as_secs_f64());
        cache_metrics::record_tokenizer_request(match &result {
            Ok(_) => "render_ok",
            Err(TokenizerError::Unmapped(_)) => "unmapped_422",
            Err(TokenizerError::RenderUnsupported(..)) => "render_unsupported",
            Err(TokenizerError::Status { .. }) => "http_error",
            Err(_) => "transport_error",
        });
        result
    }

    async fn render_inner(
        &self,
        virtual_model: &str,
        messages: &serde_json::Value,
        tools: Option<&serde_json::Value>,
        add_generation_prompt: bool,
        prefixes: &[WirePrefix],
    ) -> TokenizerResult<RenderResponse> {
        let resp = self
            .http
            .post(format!("{}/v1/render", self.base_url))
            .json(&RenderRequest {
                virtual_model,
                messages,
                tools,
                add_generation_prompt,
                prefixes,
                return_rendered: false,
            })
            .send()
            .await?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp.json().await?);
        }
        let body = resp.text().await.unwrap_or_default();
        if status.as_u16() == 422 && body.contains("UNMAPPED_MODEL") {
            return Err(TokenizerError::Unmapped(virtual_model.to_string()));
        }
        if (status.as_u16() == 422 && body.contains("NO_CHAT_TEMPLATE"))
            || (status.as_u16() == 400 && body.contains("TEMPLATE_RENDER_FAILED"))
        {
            return Err(TokenizerError::RenderUnsupported(virtual_model.to_string(), body));
        }
        Err(TokenizerError::Status {
            status: status.as_u16(),
            body,
        })
    }

    async fn tokenize_inner(&self, virtual_model: &str, segments: &[String]) -> TokenizerResult<TokenizeResponse> {
        let resp = self
            .http
            .post(format!("{}/v1/tokenize", self.base_url))
            .json(&TokenizeRequest { virtual_model, segments })
            .send()
            .await?;
        parse_tokenize(resp, virtual_model).await
    }

    /// The set of models this tokenizer-svc image has baked. control-layer uses this
    /// to drive per-model cache enablement.
    pub async fn models(&self) -> TokenizerResult<Vec<ModelInfo>> {
        let resp = self
            .http
            .get(format!("{}/v1/models", self.base_url))
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json::<ModelsResponse>().await?.models)
    }

    pub async fn healthz(&self) -> TokenizerResult<bool> {
        let resp = self.http.get(format!("{}/healthz", self.base_url)).send().await?;
        Ok(resp.status().is_success())
    }
}

async fn parse_tokenize(resp: reqwest::Response, virtual_model: &str) -> TokenizerResult<TokenizeResponse> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp.json().await?);
    }
    let body = resp.text().await.unwrap_or_default();
    // 422 UNMAPPED_MODEL → typed skip (caching off for this request, no error).
    if status.as_u16() == 422 && body.contains("UNMAPPED_MODEL") {
        return Err(TokenizerError::Unmapped(virtual_model.to_string()));
    }
    Err(TokenizerError::Status {
        status: status.as_u16(),
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn tokenize_parses_counts() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tokenize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "virtual_model": "test-model",
                "tokenizer_version": "sha256:abc",
                "segment_counts": [128, 16],
                "cumulative": [128, 144],
                "total": 144
            })))
            .mount(&server)
            .await;

        let client = TokenizerClient::new(server.uri());
        let r = client
            .tokenize("test-model", &["sys".to_string(), "user".to_string()])
            .await
            .unwrap();
        assert_eq!(r.total, 144);
        assert_eq!(r.segment_counts, vec![128, 16]);
        assert_eq!(r.cumulative, vec![128, 144]);
        assert_eq!(r.tokenizer_version, "sha256:abc");
    }

    #[tokio::test]
    async fn unmapped_model_maps_to_typed_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tokenize"))
            .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({ "code": "UNMAPPED_MODEL" })))
            .mount(&server)
            .await;

        let client = TokenizerClient::new(server.uri());
        let err = client.tokenize("mystery-model", &["hi".to_string()]).await.unwrap_err();
        match err {
            TokenizerError::Unmapped(m) => assert_eq!(m, "mystery-model"),
            other => panic!("expected Unmapped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn other_errors_are_status() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tokenize"))
            .respond_with(ResponseTemplate::new(503).set_body_string("overloaded"))
            .mount(&server)
            .await;

        let client = TokenizerClient::new(server.uri());
        let err = client.tokenize("test-model", &["hi".to_string()]).await.unwrap_err();
        assert!(matches!(err, TokenizerError::Status { status: 503, .. }));
    }

    #[tokio::test]
    async fn models_and_healthz() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [
                    { "alias": "a", "hf_repo": "org/a", "tokenizer_version": "v1" }
                ]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/healthz"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"status":"ok"})))
            .mount(&server)
            .await;

        let client = TokenizerClient::new(server.uri());
        let models = client.models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].alias, "a");
        assert!(client.healthz().await.unwrap());
    }
}
