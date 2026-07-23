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
//!   `process()` UPDATE). Healthy processing is owner-liveness fenced rather
//!   than age-reclaimed, so slow upstream work is not duplicated by a timeout.
//! - `abort_handle` cancellation: cancelling the spawned task drops
//!   the loop future, which cascades into the in-flight upstream
//!   request being cancelled.
//! - `should_retry` policy via `Request<Processing>::complete_unpersisted`;
//!   the daemon durably commits that typed outcome — same path every other
//!   request goes through.
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
use fusillade::request::{Canceled, Claimed, Failed, FailureReason, Request, RequestCompletionResult};
use fusillade::{CancellationFuture, DefaultRequestProcessor, HttpClient, RequestProcessor, ReqwestHttpClient, ShouldRetry, Storage};
use fusillade_arsenal::PoolProvider as FusilladePool;
use onwards::LoopConfig;
use onwards::traits::ToolExecutor;

use crate::inference::engine::loop_http_client::ResponseLoopHttpClient;
use crate::inference::store::FusilladeResponseStore;

fn parse_jit_image_token(value: &str) -> fusillade::Result<crate::image_normalizer::ImageToken> {
    value.parse().map_err(|_: crate::image_normalizer::TokenParseError| {
        fusillade::FusilladeError::ValidationError("JIT image signing encountered an invalid image token".to_string())
    })
}

fn classify_request_keystore_error(error: &crate::keystore::KeystoreError) -> FailureReason {
    if error.is_unreachable() {
        FailureReason::NetworkError {
            error: "Zero-data-retention request could not be processed".to_string(),
        }
    } else {
        FailureReason::RequestBuilderError {
            error: "Zero-data-retention request could not be processed".to_string(),
        }
    }
}

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
            let Some(keystore) = self.keystore.as_ref() else {
                // A ZDR ciphertext was claimed but no keystore is configured, so the
                // prompt can never be decrypted. Terminalize as a non-retriable
                // failure rather than a bare error, which cannot produce a durable
                // client outcome. The
                // client-facing reason is deliberately generic - the real cause (a
                // server misconfiguration) is logged for operators, never surfaced.
                crate::background_error!(
                    crate::metrics::errors::component::ZDR_DISPATCH,
                    "keystore_missing",
                    Error,
                    request_id = %request.data.id.0,
                    "ZDR request claimed but keystore is not configured; failing request"
                );
                let failed = Request {
                    state: Failed {
                        reason: FailureReason::RequestBuilderError {
                            error: "Zero-data-retention request could not be processed".to_string(),
                        },
                        failed_at: chrono::Utc::now(),
                        retry_attempt: request.state.retry_attempt,
                        batch_expires_at: request.state.batch_expires_at,
                        routed_model: request.data.model.clone(),
                    },
                    data: request.data,
                };
                return Ok(RequestCompletionResult::Failed(failed));
            };
            let key_id = crate::inference::zdr::key_id(&request.data.id.0, crate::inference::zdr::KeyKind::Request);
            match keystore.get(&key_id).await {
                Ok(Some(key)) => {
                    match crate::inference::zdr::decrypt_body(&key, &request.data.body) {
                        Ok(plaintext) => request.data.body = plaintext,
                        Err(e) => {
                            // Ciphertext present but undecryptable (corrupt envelope or
                            // wrong wrap key) - never succeeds on retry. Terminalize as a
                            // non-retriable failure instead of a bare Err that would strand
                            // the row in `processing`. Client reason stays generic; the
                            // crypto detail is logged for operators only.
                            crate::background_error!(
                                crate::metrics::errors::component::ZDR_DISPATCH,
                                "decrypt_failed",
                                Error,
                                request_id = %request.data.id.0,
                                error = %e,
                                "ZDR request body could not be decrypted; failing request"
                            );
                            let failed = Request {
                                state: Failed {
                                    reason: FailureReason::RequestBuilderError {
                                        error: "Zero-data-retention request could not be processed".to_string(),
                                    },
                                    failed_at: chrono::Utc::now(),
                                    retry_attempt: request.state.retry_attempt,
                                    batch_expires_at: request.state.batch_expires_at,
                                    routed_model: request.data.model.clone(),
                                },
                                data: request.data,
                            };
                            return Ok(RequestCompletionResult::Failed(failed));
                        }
                    }
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
                    // Key expired/deleted before dispatch: the prompt is gone and
                    // this request can never be processed. Terminalize it as a
                    // non-retriable failure so the daemon records it as terminally
                    // failed for this exact attempt.
                    // Returning a bare error would not produce the definitive
                    // client outcome this permanent key loss requires.
                    let failed = Request {
                        state: Failed {
                            reason: FailureReason::RequestBuilderError {
                                error: "ZDR request key expired before dispatch; cannot decrypt".to_string(),
                            },
                            failed_at: chrono::Utc::now(),
                            retry_attempt: request.state.retry_attempt,
                            batch_expires_at: request.state.batch_expires_at,
                            routed_model: request.data.model.clone(),
                        },
                        data: request.data,
                    };
                    return Ok(RequestCompletionResult::Failed(failed));
                }
                Err(e) => {
                    // Only transport unavailability is retriable. A malformed
                    // envelope, retired wrap key, crypto failure, or invalid
                    // configuration cannot improve on another dispatch attempt
                    // and must terminalize without opening the upstream gate.
                    if e.is_unreachable() {
                        crate::background_error!(
                            crate::metrics::errors::component::ZDR_DISPATCH,
                            "keystore_unreachable",
                            Warning,
                            request_id = %request.data.id.0,
                            error = %e,
                            "ZDR keystore unreachable during dispatch; scheduling retry"
                        );
                    } else {
                        crate::background_error!(
                            crate::metrics::errors::component::ZDR_DISPATCH,
                            "keystore_invalid_value",
                            Error,
                            request_id = %request.data.id.0,
                            error = %e,
                            "ZDR request key could not be prepared; failing request"
                        );
                    }
                    let failed = Request {
                        state: Failed {
                            reason: classify_request_keystore_error(&e),
                            failed_at: chrono::Utc::now(),
                            retry_attempt: request.state.retry_attempt,
                            batch_expires_at: request.state.batch_expires_at,
                            routed_model: request.data.model.clone(),
                        },
                        data: request.data,
                    };
                    return Ok(RequestCompletionResult::Failed(failed));
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
            // Use ValidationError (not Other): malformed JSON and malformed
            // opaque tokens are deterministic input failures that will never
            // succeed on retry.
            let mut body_value: serde_json::Value = serde_json::from_str(&request.data.body).map_err(|_| {
                fusillade::FusilladeError::ValidationError("JIT image signing requires a valid JSON request body".to_string())
            })?;
            let result = crate::image_normalizer::walker::substitute_with(
                &mut body_value,
                crate::image_normalizer::Mode::TokensOnly,
                |maybe_token| {
                    let normalizer = Arc::clone(&normalizer);
                    async move {
                        let token = parse_jit_image_token(&maybe_token)?;
                        let signed = normalizer
                            .sign(token, ttl)
                            .await
                            .map_err(|_| fusillade::FusilladeError::Other(anyhow::anyhow!("JIT image signing backend unavailable")))?;
                        Ok::<String, fusillade::FusilladeError>(signed.url)
                    }
                },
            )
            .await;
            match result {
                Ok(count) if count > 0 => {
                    request.data.body = serde_json::to_string(&body_value)?;
                }
                Ok(_) => {} // no tokens found, leave body alone
                Err(error) => return Err(error),
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

        let request_data = request.data.clone();
        let response_fut = async move { loop_client.execute(&request_data, &request_data.api_key).await };
        let processing = request.process(storage, response_fut).await?;
        // `ShouldRetry` is an `Arc<dyn Fn>`; `complete` wants a bare `Fn`,
        // so deref through the Arc.
        processing.complete_unpersisted(|resp| should_retry(resp), cancellation).await
    }
}

// Compile-time check that an unused import doesn't sneak in via
// rust-analyzer cleanup.
#[allow(dead_code)]
fn _smoke(_c: Canceled) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_jit_image_tokens_are_validation_errors() {
        assert!(matches!(
            parse_jit_image_token("dw-img://not-a-valid-token"),
            Err(fusillade::FusilladeError::ValidationError(_))
        ));
    }

    #[test]
    fn only_unreachable_request_keystore_errors_are_retriable() {
        assert!(matches!(
            classify_request_keystore_error(&crate::keystore::KeystoreError::Unreachable("offline".to_string())),
            FailureReason::NetworkError { .. }
        ));

        for definitive in [
            crate::keystore::KeystoreError::UnknownWrapKeyId("retired".to_string()),
            crate::keystore::KeystoreError::MalformedWrappedValue,
            crate::keystore::KeystoreError::MalformedEnvelope,
            crate::keystore::KeystoreError::Config("invalid".to_string()),
            crate::keystore::KeystoreError::Crypto(crate::encryption::EncryptionError::DecryptionFailed),
        ] {
            assert!(matches!(
                classify_request_keystore_error(&definitive),
                FailureReason::RequestBuilderError { .. }
            ));
        }
    }
}
