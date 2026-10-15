//! Client for **tokenizer-svc**: the sole source of token counts for
//! cache *writes* (reads need no tokenization — the count is stored on the entry).
//!
//! tokenizer-svc is a dumb string->count service. We send the prompt segments to
//! count; it returns per-segment counts, running cumulative totals, and a
//! `tokenizer_version` (which becomes part of the index key). A model with no
//! tokenizer mapping yields `422 UNMAPPED_MODEL`, surfaced as
//! [`TokenizerError::Unmapped`] so the caller skips caching for that request —
//! full price, no customer-facing error.
//!
//! # Single attempt vs serving scope
//!
//! A client built by [`TokenizerClient::new`] / [`TokenizerClient::with_client`] makes
//! **exactly one HTTP attempt** per call. That is the behavior for non-serving callers
//! (recompute, replay, prefix-chain, admin, and tests), and it is what
//! [`TokenizerClient::new`] tests pin: a `503` is returned to the caller once.
//!
//! The serving classify path instead uses [`TokenizerClient::serving_scoped`], a clone that
//! retries `503` for as long as the owning request lifecycle keeps the classify task alive
//! (see [`super::tokenizer_retry`] for the full semantics). That path requires a shared
//! [`TokenizerRetryBudget`] attached with [`TokenizerClient::with_retry_budget`]; without a
//! budget, `serving_scoped` is a no-op and the client stays single-attempt. Retrying is
//! activated by *construction* (the budget) plus the *serving scope* flag, never implicitly.
//!
//! Retried requests serialize their JSON body **once** to [`bytes::Bytes`] and replay those
//! exact bytes on every attempt, so attempts are byte-identical and no per-attempt
//! accumulation occurs. Retry attempts use a separate reqwest client with no idle pool
//! (`pool_max_idle_per_host(0)`) so each retry opens and closes its own connection; a custom
//! client supplied via [`TokenizerClient::with_client`] is reused for retries instead
//! (it was provided by the caller, so we do not second-guess its pool settings).
//!
//! `Retry-After` is **not** honored: backoff is the local equal-jitter exponential schedule
//! from the retry policy. Error response bodies are read at most 4 KiB and are never logged.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::metrics as cache_metrics;
use super::tokenizer_retry::{TokenizerRetryBudget, run_with_retries};

/// Cap on how much of an error response body we read (and surface in an error). The service's
/// structured error codes (e.g. `UNMAPPED_MODEL`) are short JSON objects well under this.
const MAX_ERROR_BODY_BYTES: usize = 4 * 1024;

/// HTTP client for a tokenizer-svc deployment.
#[derive(Clone)]
pub struct TokenizerClient {
    http: Client,
    /// Retry-only client: same timeout, no idle pool, so each retry gets a fresh connection.
    retry_http: Client,
    base_url: String,
    /// Shared (process-wide) retry budget, attached at construction. `None` = never retry.
    retry_budget: Option<Arc<TokenizerRetryBudget>>,
    /// Serving scope: retry 503s until success / permanent error / cancellation. Off by default;
    /// only [`TokenizerClient::serving_scoped`] turns it on.
    retry_until_cancelled: bool,
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

/// One truncation point, in the svc's wire shape. `message`/`block`/`tool_call` are
/// INCLUSIVE indices into the request as transmitted; `tools` is a COUNT (the first n
/// definitions) — per the svc contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum WirePrefix {
    /// The first `tools` definitions from `tools[]`, no messages.
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
    ///
    /// reqwest's own retrying is disabled on both clients: our retry driver must count the
    /// actual HTTP attempts it makes, and reqwest would otherwise transparently retry protocol
    /// NACKs. The retry-only client additionally has no idle pool, so a retry always opens (and
    /// closes) a fresh connection rather than reusing a possibly-poisoned keep-alive.
    pub fn new(base_url: impl Into<String>) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(5))
            .retry(reqwest::retry::never())
            .build()
            .expect("reqwest client builds with default TLS");
        let retry_http = Client::builder()
            .timeout(Duration::from_secs(5))
            .pool_max_idle_per_host(0)
            .retry(reqwest::retry::never())
            .build()
            .expect("reqwest retry client builds with default TLS");
        Self {
            http,
            retry_http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            retry_budget: None,
            retry_until_cancelled: false,
        }
    }

    /// Build a client from a caller-supplied reqwest client. The same client is used for retry
    /// attempts (its pool settings are the caller's choice; we do not swap in a retry-only one).
    pub fn with_client(http: Client, base_url: impl Into<String>) -> Self {
        Self {
            retry_http: http.clone(),
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            retry_budget: None,
            retry_until_cancelled: false,
        }
    }

    /// Attach the shared retry budget (construction-level; one `Arc` per process/destination).
    /// Does NOT enable retries by itself — see [`Self::serving_scoped`].
    pub fn with_retry_budget(mut self, budget: Arc<TokenizerRetryBudget>) -> Self {
        self.retry_budget = Some(budget);
        self
    }

    /// A request-scoped clone that retries 503s for as long as the owning task lives
    /// (cancellation = dropping/aborting the future). No-op without an attached budget.
    pub fn serving_scoped(&self) -> Self {
        Self {
            retry_until_cancelled: self.retry_budget.is_some(),
            ..self.clone()
        }
    }

    /// Whether this clone retries 503s (serving scope with a budget attached).
    pub fn retries_enabled(&self) -> bool {
        self.retry_until_cancelled && self.retry_budget.is_some()
    }

    /// The budget the retry driver runs with: `Some` only in serving scope, so every other caller
    /// makes exactly one attempt (still counted in the per-attempt metric as `attempt="first"`).
    fn active_budget(&self) -> Option<&TokenizerRetryBudget> {
        if self.retry_until_cancelled {
            self.retry_budget.as_deref()
        } else {
            None
        }
    }

    /// The client to use for one attempt: the retry-only client for retries, the primary client
    /// for the first attempt (or when both are the same caller-supplied client).
    fn http_for(&self, is_retry: bool) -> &Client {
        if is_retry { &self.retry_http } else { &self.http }
    }

    /// Count tokens for each segment. Special tokens are NOT added (the service is
    /// configured `add_special_tokens=false`), so counts are additive across
    /// segments and the totals reconcile.
    pub async fn tokenize(&self, virtual_model: &str, segments: &[String]) -> TokenizerResult<TokenizeResponse> {
        let start = std::time::Instant::now();
        let size = cache_metrics::tokenize_size_bucket(segments.iter().map(String::len).sum());
        // Serialize once; every attempt replays these exact bytes.
        let body =
            Bytes::from(serde_json::to_vec(&TokenizeRequest { virtual_model, segments }).expect("TokenizeRequest is always serializable"));
        let result = run_with_retries(self.active_budget(), "tokenize", |is_retry| {
            let http = self.http_for(is_retry);
            let body = body.clone();
            async move { tokenize_once(http, &self.base_url, body, virtual_model).await }
        })
        .await;
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
        // Serialize once; every attempt replays these exact bytes.
        let body = Bytes::from(
            serde_json::to_vec(&RenderRequest {
                virtual_model,
                messages,
                tools,
                add_generation_prompt,
                prefixes,
                return_rendered: false,
            })
            .expect("RenderRequest is always serializable"),
        );
        // Bucket by the serialized request (messages + tools + a small fixed envelope) rather than
        // re-serializing the messages just to measure them.
        let size = cache_metrics::tokenize_size_bucket(body.len());
        let result = run_with_retries(self.active_budget(), "render", |is_retry| {
            let http = self.http_for(is_retry);
            let body = body.clone();
            async move { render_once(http, &self.base_url, body, virtual_model).await }
        })
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

    /// The set of models this tokenizer-svc image has baked. control-layer uses this
    /// to drive per-model cache enablement.
    pub async fn models(&self) -> TokenizerResult<Vec<ModelInfo>> {
        run_with_retries(self.active_budget(), "models", |is_retry| {
            let http = self.http_for(is_retry);
            async move { models_once(http, &self.base_url).await }
        })
        .await
    }

    pub async fn healthz(&self) -> TokenizerResult<bool> {
        let resp = self.http.get(format!("{}/healthz", self.base_url)).send().await?;
        Ok(resp.status().is_success())
    }
}

/// One `/v1/tokenize` attempt against `http`. The body is pre-serialized by the caller so every
/// attempt in a retry sequence replays byte-identical bytes.
async fn tokenize_once(http: &Client, base_url: &str, body: Bytes, virtual_model: &str) -> TokenizerResult<TokenizeResponse> {
    let resp = http
        .post(format!("{base_url}/v1/tokenize"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await?;
    parse_tokenize(resp, virtual_model).await
}

/// One `/v1/render` attempt against `http`. See [`tokenize_once`] for the body contract.
async fn render_once(http: &Client, base_url: &str, body: Bytes, virtual_model: &str) -> TokenizerResult<RenderResponse> {
    let resp = http
        .post(format!("{base_url}/v1/render"))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await?;
    let status = resp.status();
    if status.is_success() {
        return Ok(resp.json().await?);
    }
    let body = read_body_limited(resp).await?;
    if status.as_u16() == 422 && body.contains("UNMAPPED_MODEL") {
        return Err(TokenizerError::Unmapped(virtual_model.to_string()));
    }
    if (status.as_u16() == 422 && body.contains("NO_CHAT_TEMPLATE")) || (status.as_u16() == 400 && body.contains("TEMPLATE_RENDER_FAILED"))
    {
        return Err(TokenizerError::RenderUnsupported(virtual_model.to_string(), body));
    }
    Err(TokenizerError::Status {
        status: status.as_u16(),
        body,
    })
}

/// One `/v1/models` attempt against `http`. Non-2xx statuses become [`TokenizerError::Status`]
/// (including `503`) so the retry driver can recognize an overload; transport failures stay
/// [`TokenizerError::Http`].
async fn models_once(http: &Client, base_url: &str) -> TokenizerResult<Vec<ModelInfo>> {
    let resp = http.get(format!("{base_url}/v1/models")).send().await?;
    let status = resp.status();
    if status.is_success() {
        return Ok(resp.json::<ModelsResponse>().await?.models);
    }
    let body = read_body_limited(resp).await?;
    Err(TokenizerError::Status {
        status: status.as_u16(),
        body,
    })
}

/// Read at most [`MAX_ERROR_BODY_BYTES`] of an error response. Streaming in chunks avoids
/// buffering an unbounded proxy error page; the truncation is harmless because the structured
/// error codes we match on sit at the start of a small JSON object.
async fn read_body_limited(mut resp: reqwest::Response) -> Result<String, reqwest::Error> {
    let mut buf: Vec<u8> = Vec::new();
    // A transport error mid-body propagates (as it did with `text().await?`), so a cut-off 422 is
    // never misread as a different status class.
    while buf.len() < MAX_ERROR_BODY_BYTES
        && let Some(chunk) = resp.chunk().await?
    {
        let take = chunk.len().min(MAX_ERROR_BODY_BYTES - buf.len());
        buf.extend_from_slice(&chunk[..take]);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

async fn parse_tokenize(resp: reqwest::Response, virtual_model: &str) -> TokenizerResult<TokenizeResponse> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp.json().await?);
    }
    let body = read_body_limited(resp).await?;
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::prompt_cache::tokenizer_retry::TokenizerRetryPolicy;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn fast_policy() -> TokenizerRetryPolicy {
        TokenizerRetryPolicy {
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(1),
            retries_per_second: 1_000,
            retry_burst: 1_000,
            max_concurrent_retries: 8,
        }
    }

    /// A serving-scoped client that retries with a ~1ms backoff (real time: wiremock + reqwest
    /// timeouts misbehave under paused time).
    fn retrying_client(uri: String) -> TokenizerClient {
        let budget = TokenizerRetryBudget::new(fast_policy());
        TokenizerClient::new(uri).with_retry_budget(budget).serving_scoped()
    }

    fn tokenize_success_body() -> serde_json::Value {
        serde_json::json!({
            "virtual_model": "test-model",
            "tokenizer_version": "sha256:abc",
            "segment_counts": [128, 16],
            "cumulative": [128, 144],
            "total": 144
        })
    }

    fn render_success_body() -> serde_json::Value {
        serde_json::json!({
            "virtual_model": "test-model",
            "tokenizer_version": "sha256:abc",
            "template_version": "tpl:1",
            "total": 200,
            "prefix_counts": [100]
        })
    }

    /// Mounts a mock that answers the first `failures` requests with `fail_status` and every
    /// later request with `success`.
    async fn mount_flaky(
        server: &MockServer,
        method_name: &str,
        path_name: &str,
        failures: usize,
        fail_status: u16,
        success: ResponseTemplate,
    ) {
        let calls = Arc::new(AtomicUsize::new(0));
        Mock::given(method(method_name))
            .and(path(path_name))
            .respond_with(move |_req: &Request| {
                if calls.fetch_add(1, Ordering::SeqCst) < failures {
                    ResponseTemplate::new(fail_status).set_body_string("overloaded")
                } else {
                    success.clone()
                }
            })
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn tokenize_parses_counts() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tokenize"))
            .respond_with(ResponseTemplate::new(200).set_body_json(tokenize_success_body()))
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

    #[tokio::test]
    async fn serving_tokenize_retries_503_and_replays_identical_body() {
        let server = MockServer::start().await;
        mount_flaky(
            &server,
            "POST",
            "/v1/tokenize",
            3,
            503,
            ResponseTemplate::new(200).set_body_json(tokenize_success_body()),
        )
        .await;

        let client = retrying_client(server.uri());
        let r = client
            .tokenize("test-model", &["sys".to_string(), "user".to_string()])
            .await
            .expect("recovers after 503s");
        assert_eq!(r.total, 144);
        assert_eq!(r.segment_counts, vec![128, 16]);

        let requests = server.received_requests().await.expect("requests recorded");
        assert_eq!(requests.len(), 4, "3 failures + 1 success");
        let first = &requests[0].body;
        for request in &requests {
            assert_eq!(&request.body, first, "every attempt must replay identical bytes");
        }
        let parsed: serde_json::Value = serde_json::from_slice(first).expect("json body");
        assert_eq!(parsed["virtual_model"], "test-model");
        assert_eq!(parsed["segments"], serde_json::json!(["sys", "user"]));
    }

    #[tokio::test]
    async fn serving_render_retries_503_and_replays_identical_body() {
        let server = MockServer::start().await;
        mount_flaky(
            &server,
            "POST",
            "/v1/render",
            3,
            503,
            ResponseTemplate::new(200).set_body_json(render_success_body()),
        )
        .await;

        let client = retrying_client(server.uri());
        let messages = serde_json::json!([{ "role": "user", "content": "hi" }]);
        let prefixes = vec![WirePrefix::Message { message: 0 }];
        let r = client
            .render("test-model", &messages, None, true, &prefixes)
            .await
            .expect("recovers after 503s");
        assert_eq!(r.total, 200);
        assert_eq!(r.prefix_counts, vec![Some(100)]);

        let requests = server.received_requests().await.expect("requests recorded");
        assert_eq!(requests.len(), 4, "3 failures + 1 success");
        let first = &requests[0].body;
        for request in &requests {
            assert_eq!(&request.body, first, "every attempt must replay identical bytes");
        }
    }

    #[tokio::test]
    async fn serving_models_retries_503() {
        let server = MockServer::start().await;
        mount_flaky(
            &server,
            "GET",
            "/v1/models",
            2,
            503,
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [{ "alias": "a", "hf_repo": "org/a", "tokenizer_version": "v1" }]
            })),
        )
        .await;

        let client = retrying_client(server.uri());
        let models = client.models().await.expect("recovers after 503s");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].alias, "a");

        let requests = server.received_requests().await.expect("requests recorded");
        assert_eq!(requests.len(), 3, "2 failures + 1 success");
    }

    #[tokio::test]
    async fn non_serving_client_makes_exactly_one_attempt_on_503() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tokenize"))
            .respond_with(ResponseTemplate::new(503).set_body_string("overloaded"))
            .mount(&server)
            .await;

        let client = TokenizerClient::new(server.uri());
        assert!(!client.retries_enabled());
        let err = client.tokenize("test-model", &["hi".to_string()]).await.unwrap_err();
        assert!(matches!(err, TokenizerError::Status { status: 503, .. }));

        let requests = server.received_requests().await.expect("requests recorded");
        assert_eq!(requests.len(), 1, "non-serving client must not retry");
    }

    #[tokio::test]
    async fn serving_unmapped_422_makes_exactly_one_attempt() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tokenize"))
            .respond_with(ResponseTemplate::new(422).set_body_json(serde_json::json!({ "code": "UNMAPPED_MODEL" })))
            .mount(&server)
            .await;

        let client = retrying_client(server.uri());
        let err = client.tokenize("mystery-model", &["hi".to_string()]).await.unwrap_err();
        assert!(matches!(err, TokenizerError::Unmapped(_)));
        let requests = server.received_requests().await.expect("requests recorded");
        assert_eq!(requests.len(), 1, "422 is permanent, not retried");
    }

    #[tokio::test]
    async fn serving_render_unsupported_makes_exactly_one_attempt() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/render"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({ "code": "TEMPLATE_RENDER_FAILED" })))
            .mount(&server)
            .await;

        let client = retrying_client(server.uri());
        let messages = serde_json::json!([{ "role": "user", "content": "hi" }]);
        let err = client.render("test-model", &messages, None, true, &[]).await.unwrap_err();
        assert!(matches!(err, TokenizerError::RenderUnsupported(..)));
        let requests = server.received_requests().await.expect("requests recorded");
        assert_eq!(requests.len(), 1, "400 TEMPLATE_RENDER_FAILED is permanent, not retried");
    }

    #[tokio::test]
    async fn serving_malformed_200_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tokenize"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let client = retrying_client(server.uri());
        let err = client.tokenize("test-model", &["hi".to_string()]).await.unwrap_err();
        assert!(matches!(err, TokenizerError::Http(_)), "expected Http, got {err:?}");
        let requests = server.received_requests().await.expect("requests recorded");
        assert_eq!(requests.len(), 1, "malformed 2xx is not retried");
    }
}
