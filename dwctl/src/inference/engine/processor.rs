//! [`fusillade::RequestProcessor`] dispatcher: routes `/v1/responses`
//! claims into the multi-step orchestration loop, defers everything
//! else to the existing [`fusillade::DefaultRequestProcessor`].
//!
//! ## How the multi-step path reuses fusillade machinery
//!
//! The multi-step loop is plugged into fusillade via the `HttpClient`
//! trait — see [`ResponseLoopHttpClient`](super::loop_http_client::ResponseLoopHttpClient).
//! From fusillade's perspective the loop *is* the HTTP call: it runs
//! inside the spawned task that `Request<Claimed>::process` creates,
//! and its assembled JSON is returned as a synthesized `HttpResponse`.
//!
//! That single decision gives us, for free:
//!
//! - `claimed → processing` state transition (via fusillade's
//!   `process()` UPDATE), which means the row gets the longer
//!   `processing_timeout_ms` budget instead of the short
//!   `claim_timeout_ms` — no more reclaim race on slow upstreams.
//! - `abort_handle` cancellation: cancelling the spawned task drops
//!   the loop future, which cascades into the in-flight upstream
//!   request being cancelled.
//! - `should_retry` policy + terminal-state persistence via
//!   `Request<Processing>::complete` — same path every other request
//!   goes through.
//!
//! See `docs/responses-processor-design.md` for the full design.
//!
//! ## Single-step requests are unchanged
//!
//! Anything whose endpoint is not `/v1/responses` flows straight
//! through `DefaultRequestProcessor::process(...)`. The batch path,
//! `/v1/chat/completions`, and `/v1/embeddings` get the exact same
//! pipeline as before.

use std::sync::Arc;

use async_trait::async_trait;
use fusillade::request::{Canceled, Claimed, Request, RequestCompletionResult};
use fusillade::{
    CancellationFuture, DefaultRequestProcessor, PoolProvider as FusilladePool, RequestProcessor, ReqwestHttpClient, ShouldRetry, Storage,
};
use onwards::LoopConfig;
use onwards::traits::ToolExecutor;

use crate::inference::engine::loop_http_client::ResponseLoopHttpClient;
use crate::inference::store::FusilladeResponseStore;

/// Dispatches per-claim work to the multi-step loop for `/v1/responses`
/// requests, falling through to [`DefaultRequestProcessor`] for
/// everything else.
///
/// Generic over the tool executor type so test fixtures can wrap the
/// production [`HttpToolExecutor`] with context-injecting shims (the
/// daemon path doesn't have request-scoped middleware to populate
/// `RequestContext.extensions::<ResolvedTools>`, so the test wraps with
/// an injector). Production wiring uses `HttpToolExecutor` directly +
/// the [`tool_resolver`](Self::tool_resolver) field below.
pub struct DwctlRequestProcessor<P, T>
where
    P: FusilladePool + Clone + Send + Sync + 'static,
    T: ToolExecutor + 'static,
{
    pub response_store: Arc<FusilladeResponseStore<P>>,
    pub tool_executor: Arc<T>,
    pub http_client: Arc<ReqwestHttpClient>,
    pub loop_config: LoopConfig,
    /// Tool resolver for the daemon path. Tools today are scoped per
    /// (api_key, model alias) — same as the realtime middleware path —
    /// so the daemon resolves the same tool set the original request
    /// would have seen. `None` means no DB-backed resolution; the
    /// processor will run the loop with whatever tools the
    /// `tool_executor` discovers from an empty context (used by the
    /// daemon-test fixture that injects ResolvedTools through its own
    /// shim).
    pub tool_resolver: Option<Arc<dyn DaemonToolResolver>>,
    /// Optional image normaliser for JIT signing of `dw-img://` tokens
    /// embedded in the request body. When set, the processor walks the
    /// body before either dispatch branch, replaces every token with a
    /// fresh signed URL (TTL = `dispatch_ttl`), and persists the swapped
    /// body back into `request.data.body` for the rest of the dispatch.
    /// `None` skips JIT signing — bodies pass through unchanged (used
    /// when image normalisation is disabled in config).
    pub image_normalizer: Option<Arc<dyn crate::image_normalizer::ImageNormalizer>>,
    /// TTL applied to signed URLs generated at dispatch. Refreshed on
    /// every dispatch attempt so retries get a new URL and the leak
    /// window per attempt is bounded.
    pub dispatch_ttl: std::time::Duration,
    /// Default processor used for non-`/v1/responses` endpoints. Owns
    /// no state — declared as a field so the trait dispatch below has
    /// a stable receiver.
    pub default: DefaultRequestProcessor,
    /// Encrypted key custody for ZDR flex bodies. `None` disables ZDR
    /// decryption (bodies pass through unchanged).
    pub keystore: Option<crate::keystore::Keystore>,
}

/// Resolve the tool set for a daemon-claimed request. Called once per
/// `/v1/responses` claim before the loop runs. The default production
/// implementation (see [`DbToolResolver`]) runs the same DB join the
/// realtime middleware does, scoped to the row's API key and model
/// alias.
#[async_trait]
pub trait DaemonToolResolver: Send + Sync {
    async fn resolve(&self, api_key: &str, model_alias: &str) -> Result<Option<crate::inference::tools::ResolvedToolSet>, anyhow::Error>;
}

/// Production [`DaemonToolResolver`] backed by the same query the
/// realtime tool injection middleware uses.
pub struct DbToolResolver {
    pub pool: sqlx::PgPool,
}

#[async_trait]
impl DaemonToolResolver for DbToolResolver {
    async fn resolve(&self, api_key: &str, model_alias: &str) -> Result<Option<crate::inference::tools::ResolvedToolSet>, anyhow::Error> {
        crate::inference::tools::resolve_tools_for_request(&self.pool, api_key, Some(model_alias)).await
    }
}

impl<P, T> DwctlRequestProcessor<P, T>
where
    P: FusilladePool + Clone + Send + Sync + 'static,
    T: ToolExecutor + 'static,
{
    pub fn new(
        response_store: Arc<FusilladeResponseStore<P>>,
        tool_executor: Arc<T>,
        http_client: Arc<ReqwestHttpClient>,
        loop_config: LoopConfig,
    ) -> Self {
        Self {
            response_store,
            tool_executor,
            http_client,
            loop_config,
            tool_resolver: None,
            image_normalizer: None,
            // 30 min default. Realistic upstream processing windows are
            // shorter; this gives retries a generous-enough budget while
            // keeping the leak window per attempt bounded. The startup
            // wiring in `lib.rs` overrides this from
            // `config.image_normalizer.signing.dispatch_ttl()`.
            dispatch_ttl: std::time::Duration::from_secs(1800),
            default: DefaultRequestProcessor,
            keystore: None,
        }
    }

    /// Wire in the image normaliser for JIT signing. Without this, the
    /// processor passes bodies through unchanged. The TTL controls how
    /// long the signed URL handed to the upstream is valid.
    pub fn with_image_normalizer(
        mut self,
        normalizer: Arc<dyn crate::image_normalizer::ImageNormalizer>,
        ttl: std::time::Duration,
    ) -> Self {
        self.image_normalizer = Some(normalizer);
        self.dispatch_ttl = ttl;
        self
    }

    /// Wire in the keystore so ZDR request bodies are decrypted before dispatch.
    pub fn with_keystore(mut self, keystore: Option<crate::keystore::Keystore>) -> Self {
        self.keystore = keystore;
        self
    }

    /// Wire in the production tool resolver. Without this, the daemon
    /// path runs the loop with no resolved tools — fine for tests, but
    /// in production this should always be set so multi-step requests
    /// see the same tools their original API key + model alias would.
    pub fn with_tool_resolver(mut self, resolver: Arc<dyn DaemonToolResolver>) -> Self {
        self.tool_resolver = Some(resolver);
        self
    }
}

#[async_trait]
impl<S, H, P, T> RequestProcessor<S, H> for DwctlRequestProcessor<P, T>
where
    S: Storage + Sync,
    H: fusillade::HttpClient + 'static,
    P: FusilladePool + Clone + Send + Sync + 'static,
    T: ToolExecutor + 'static,
{
    async fn process(
        &self,
        mut request: Request<Claimed>,
        http: H,
        storage: &S,
        should_retry: ShouldRetry,
        cancellation: CancellationFuture,
    ) -> fusillade::Result<RequestCompletionResult> {
        // ZDR: the stored request body is a self-describing ciphertext envelope.
        // Decrypt it here, before JIT signing and either dispatch branch, so the
        // rest of the flow sees plaintext. The response is re-encrypted on its way
        // back through the loopback layer.
        if crate::inference::zdr::is_zdr_body(&request.data.body) {
            let keystore = self
                .keystore
                .as_ref()
                .ok_or_else(|| fusillade::FusilladeError::Other(anyhow::anyhow!("ZDR request claimed but keystore is not configured")))?;
            let key_id = crate::inference::zdr::key_id(&request.data.id.0, crate::inference::zdr::KeyKind::Request);
            match keystore.get(&key_id).await {
                Ok(Some(key)) => {
                    request.data.body = crate::inference::zdr::decrypt_body(&key, &request.data.body)
                        .map_err(|e| fusillade::FusilladeError::Other(anyhow::anyhow!("ZDR request decrypt failed: {e}")))?;
                    // TRANSITIONAL (dwctl ZDR): mark the dispatch so the loopback
                    // analytics handler blanks the now-plaintext body instead of
                    // logging it. fusillade forwards batch_metadata entries as
                    // `x-fusillade-batch-<key>` headers, so this rides out as
                    // `x-fusillade-batch-zdr: 1`; the outlet handler reads that.
                    // Piggybacks the existing header channel to avoid a fusillade
                    // API change - drop when reassembly moves into dwctl.
                    request
                        .data
                        .batch_metadata
                        .insert(crate::inference::zdr::ZDR_MARKER_KEY.to_string(), "1".to_string());
                }
                Ok(None) => {
                    // Key expired or was deleted before dispatch: the prompt is
                    // gone and the request can never be processed. Fail it.
                    return Err(fusillade::FusilladeError::ValidationError(
                        "ZDR request key expired before dispatch; cannot decrypt".to_string(),
                    ));
                }
                Err(e) => {
                    return Err(fusillade::FusilladeError::Other(anyhow::anyhow!("ZDR keystore error: {e}")));
                }
            }
        }

        // JIT signing: any `dw-img://{sha256}` token embedded in the body
        // (placed there by the file-ingest path) gets resolved to a fresh
        // short-lived signed URL right before dispatch. This means the
        // long-lived value at rest in the database is only the opaque
        // token; the signed URL only exists for the dispatch attempt's
        // TTL, so retries get fresh URLs and per-attempt leak windows
        // remain bounded. No-op when the normaliser is unset.
        if let Some(normalizer) = self.image_normalizer.clone() {
            let ttl = self.dispatch_ttl;
            // Fail loud if the body isn't parseable JSON. A row that
            // contains `dw-img://` tokens by construction always has a
            // JSON body; an unparseable body here means corruption, and
            // silently dispatching the literal token to an upstream
            // (which can't fetch a `dw-img://` URL) would manifest as a
            // confusing upstream error far from the root cause.
            // Use ValidationError (not Other): a body that won't parse as
            // JSON is malformed input that will never succeed on retry —
            // semantically a validation failure, not a transient/unknown
            // error. (The fusillade daemon currently treats all process()
            // Err variants the same, but classifying it correctly future-
            // proofs against fusillade differentiating non-retryable errors.)
            let mut body_value: serde_json::Value = serde_json::from_str(&request.data.body).map_err(|e| {
                fusillade::FusilladeError::ValidationError(format!(
                    "JIT image signing: request body is not valid JSON ({e}); refusing to dispatch with unresolved tokens"
                ))
            })?;
            let result = crate::image_normalizer::walker::substitute_with(
                &mut body_value,
                crate::image_normalizer::Mode::TokensOnly,
                |maybe_token| {
                    let normalizer = Arc::clone(&normalizer);
                    async move {
                        let token: crate::image_normalizer::ImageToken = maybe_token
                            .parse()
                            .map_err(|e: crate::image_normalizer::TokenParseError| format!("invalid dw-img token: {e}"))?;
                        let signed = normalizer.sign(token, ttl).await.map_err(|e| format!("sign failed: {e}"))?;
                        Ok::<String, String>(signed.url)
                    }
                },
            )
            .await;
            match result {
                Ok(count) if count > 0 => match serde_json::to_string(&body_value) {
                    Ok(new_body) => request.data.body = new_body,
                    Err(e) => {
                        return Err(fusillade::FusilladeError::Other(anyhow::anyhow!(
                            "re-serialise body after JIT signing: {e}"
                        )));
                    }
                },
                Ok(_) => {} // no tokens found, leave body alone
                Err(e) => {
                    return Err(fusillade::FusilladeError::Other(anyhow::anyhow!(
                        "JIT image-URL signing failed: {e}"
                    )));
                }
            }
        }

        // Multi-step path is gated on the request's API path. fusillade's
        // RequestData splits URL into `endpoint` (base URL like
        // https://api.openai.com) and `path` (e.g. /v1/responses), so we
        // match on `path`. Subpaths (e.g. `/v1/responses/...`) and other
        // routes flow through the default processor unchanged.
        if request.data.path != "/v1/responses" {
            return self.default.process(request, http, storage, should_retry, cancellation).await;
        }

        // Tool-free /v1/responses doesn't need the multi-step loop —
        // there are no tool_calls to dispatch. Delegate to the default
        // processor: it fires a single HTTP call to the row's
        // {endpoint}/v1/responses (the dwctl loopback) and onwards
        // handles the /v1/responses → /v1/chat/completions rewrite
        // natively. Result: one tracking row, no record_step,
        // no response_steps, no finalize_head_request.
        //
        // `has_tools` is parsed from the row's body — which the
        // middleware already populated with any server-side-resolved
        // tools before persisting. A failed parse falls through to
        // the loop path defensively (a malformed body will fail
        // there too, but with the existing error surfaces).
        let has_tools = serde_json::from_str::<serde_json::Value>(&request.data.body)
            .ok()
            .as_ref()
            .and_then(|v| v.get("tools"))
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty());
        if !has_tools {
            return self.default.process(request, http, storage, should_retry, cancellation).await;
        }

        // Plug the multi-step loop into fusillade's HttpClient seam. The
        // rest of the flow (state transition, abort, retry, persistence)
        // is identical to DefaultRequestProcessor — see this module's
        // doc-comment for the why.
        let loop_client = ResponseLoopHttpClient {
            response_store: self.response_store.clone(),
            tool_executor: self.tool_executor.clone(),
            inner_http: self.http_client.clone(),
            tool_resolver: self.tool_resolver.clone(),
            loop_config: self.loop_config,
        };

        let processing = request.process(loop_client, storage).await?;
        // `ShouldRetry` is an `Arc<dyn Fn>`; `complete` wants a bare `Fn`,
        // so deref through the Arc.
        processing.complete(storage, |resp| should_retry(resp), cancellation).await
    }
}

// Compile-time check that an unused import doesn't sneak in via
// rust-analyzer cleanup.
#[allow(dead_code)]
fn _smoke(_c: Canceled) {}
