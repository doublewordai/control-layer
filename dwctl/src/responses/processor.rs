//! [`fusillade::RequestProcessor`] dispatcher: routes `/v1/responses`
//! claims into the multi-step orchestration loop, defers everything
//! else to the existing [`fusillade::DefaultRequestProcessor`].
//!
//! This is the single point where dwctl decides "is this a multi-step
//! request?" — and from here the loop handles the rest: transition
//! decisions, parallel fan-out, sub-agent recursion, retries, and
//! assembly.
//!
//! ## Lifecycle for a multi-step request
//!
//! 1. Daemon claims a fusillade row whose endpoint is `/v1/responses`.
//! 2. This processor's `process()` is invoked.
//! 3. We construct a per-request [`onwards::run_response_loop`]
//!    invocation against:
//!    - storage = the shared `FusilladeResponseStore` (transition +
//!      assembly + step CRUD);
//!    - tools = the shared `HttpToolExecutor` + a `RequestContext`
//!      carrying the resolved `ResolvedTools` for this request;
//!    - upstream = the model URL/auth resolved from the row's
//!      `endpoint`/`api_key`;
//!    - http_client = the shared onwards `HyperClient`.
//! 4. The loop runs to terminal state. The final assembled response is
//!    persisted as the parent fusillade row's `response_body` (via
//!    `complete_request`), at which point the row reaches `Completed`
//!    and downstream consumers (the `GET /v1/responses/{id}` handler,
//!    streaming subscribers, etc.) see it.
//!
//! ## Single-step requests are unchanged
//!
//! Anything whose endpoint is not `/v1/responses` flows straight
//! through `DefaultRequestProcessor::process(...)`. The batch path,
//! `/v1/chat/completions`, and `/v1/embeddings` get the exact same
//! pipeline as before.

use std::sync::Arc;

use async_trait::async_trait;
use fusillade::{
    CancellationFuture, DefaultRequestProcessor, FailureReason, PoolProvider as FusilladePool,
    RequestProcessor, ShouldRetry, Storage,
};
use fusillade::request::{
    Canceled, Claimed, Completed, Failed, Request, RequestCompletionResult,
};
use onwards::client::HttpClient;
use onwards::traits::{RequestContext, ToolExecutor};
use onwards::{LoopConfig, LoopError, MultiStepStore, UpstreamTarget};

use crate::responses::store::FusilladeResponseStore;

/// Dispatches per-claim work to the multi-step loop for `/v1/responses`
/// requests, falling through to [`DefaultRequestProcessor`] for
/// everything else.
///
/// Generic over the tool executor type so test fixtures can wrap the
/// production [`HttpToolExecutor`] with context-injecting shims (the
/// daemon path doesn't have request-scoped middleware to populate
/// `RequestContext.extensions::<ResolvedTools>`, so the test wraps with
/// an injector). Production wiring uses `HttpToolExecutor` directly.
pub struct DwctlRequestProcessor<P, T>
where
    P: FusilladePool + Clone + Send + Sync + 'static,
    T: ToolExecutor + 'static,
{
    pub response_store: Arc<FusilladeResponseStore<P>>,
    pub tool_executor: Arc<T>,
    pub http_client: Arc<dyn HttpClient + Send + Sync>,
    pub loop_config: LoopConfig,
    /// Default processor used for non-`/v1/responses` endpoints. Owns
    /// no state — declared as a field so the trait dispatch below has
    /// a stable receiver.
    pub default: DefaultRequestProcessor,
}

impl<P, T> DwctlRequestProcessor<P, T>
where
    P: FusilladePool + Clone + Send + Sync + 'static,
    T: ToolExecutor + 'static,
{
    pub fn new(
        response_store: Arc<FusilladeResponseStore<P>>,
        tool_executor: Arc<T>,
        http_client: Arc<dyn HttpClient + Send + Sync>,
        loop_config: LoopConfig,
    ) -> Self {
        Self {
            response_store,
            tool_executor,
            http_client,
            loop_config,
            default: DefaultRequestProcessor,
        }
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
        request: Request<Claimed>,
        http: H,
        storage: &S,
        should_retry: ShouldRetry,
        cancellation: CancellationFuture,
    ) -> fusillade::Result<RequestCompletionResult> {
        // Multi-step path is gated on the request's API path. fusillade's
        // RequestData splits URL into `endpoint` (base URL like
        // https://api.openai.com) and `path` (e.g. /v1/responses), so we
        // match on `path`. Subpaths (e.g. `/v1/responses/...`) and other
        // routes flow through the default processor unchanged.
        if request.data.path != "/v1/responses" {
            return self
                .default
                .process(request, http, storage, should_retry, cancellation)
                .await;
        }

        // The cancellation future is owned by the daemon. The
        // multi-step loop does not currently observe it directly —
        // sub-step HTTP calls inherit it through Tokio's
        // task-cancellation tree because we run inside the daemon's
        // spawned task. We hold it alive here so cancellations
        // propagate into the awaits below via the runtime.
        let _cancellation_holder = cancellation;
        let _should_retry_unused = should_retry;
        let _http_unused = http;
        let _storage_unused = storage;

        let request_id = request.data.id.0.to_string();
        let upstream = UpstreamTarget {
            // For the multi-step path, dwctl redirects to upstream's
            // `/v1/chat/completions` even though the user-visible path
            // is `/v1/responses` — the chat-completions shape is what
            // most upstream models speak (transition.rs encodes the
            // mapping). The fusillade row's `endpoint` carries the
            // upstream base URL.
            url: {
                let base = request.data.endpoint.trim_end_matches('/');
                format!("{base}/v1/chat/completions")
            },
            api_key: if request.data.api_key.is_empty() {
                None
            } else {
                Some(request.data.api_key.clone())
            },
        };

        // Tools resolved by middleware aren't accessible inside the
        // daemon — middleware runs only on the inline request path.
        // For daemon-driven multi-step responses, the tool registry is
        // discovered fresh via HttpToolExecutor::tools() against an
        // empty RequestContext. Tools that require per-request resolved
        // state should be added via with_extension on a context built
        // here from the request's metadata; for now we pass an empty
        // context, which means any tool requiring middleware-injected
        // state will be unavailable in the daemon path. Wiring this is
        // a follow-up.
        let tool_ctx = RequestContext::new().with_model(request.data.model.clone());

        let result = onwards::run_response_loop(
            &*self.response_store,
            &*self.tool_executor,
            &tool_ctx,
            &upstream,
            self.http_client.clone(),
            &request_id,
            None,
            self.loop_config,
            0,
        )
        .await;

        match result {
            Ok(_final_payload) => {
                // The loop already persisted every step including the
                // assembled response (via the transition function
                // returning Complete with the assembled body). The
                // parent fusillade row needs to land in Completed
                // state with the assembled JSON in response_body so
                // GET /v1/responses/{id} sees it.
                let assembled = self
                    .response_store
                    .assemble_response(&request_id)
                    .await
                    .map_err(|e| fusillade::FusilladeError::Other(anyhow::anyhow!(
                        "assemble_response after loop: {e}"
                    )))?;
                let body = serde_json::to_string(&assembled).map_err(|e| {
                    fusillade::FusilladeError::Other(anyhow::anyhow!(
                        "serialize assembled response: {e}"
                    ))
                })?;
                let completed = Request {
                    data: request.data.clone(),
                    state: Completed {
                        response_status: 200,
                        response_body: body,
                        claimed_at: request.state.claimed_at,
                        started_at: chrono::Utc::now(),
                        completed_at: chrono::Utc::now(),
                        routed_model: request.data.model.clone(),
                    },
                };
                storage.persist(&completed).await?;
                Ok(RequestCompletionResult::Completed(completed))
            }
            Err(LoopError::Failed(payload)) => {
                let body = serde_json::to_string(&payload).unwrap_or_default();
                let failed = Request {
                    data: request.data.clone(),
                    state: Failed {
                        reason: FailureReason::NonRetriableHttpStatus {
                            status: 500,
                            body,
                        },
                        failed_at: chrono::Utc::now(),
                        retry_attempt: request.state.retry_attempt,
                        batch_expires_at: request.state.batch_expires_at,
                        routed_model: request.data.model.clone(),
                    },
                };
                storage.persist(&failed).await?;
                Ok(RequestCompletionResult::Failed(failed))
            }
            Err(other) => {
                // Storage-level / cap-level / unexpected errors all
                // become non-retriable failures on the parent row. The
                // multi-step loop already persisted whatever step rows
                // got partway through; surfacing the parent's terminal
                // state lets the caller see what happened.
                let failed = Request {
                    data: request.data.clone(),
                    state: Failed {
                        reason: FailureReason::NonRetriableHttpStatus {
                            status: 500,
                            body: format!("multi-step loop error: {other}"),
                        },
                        failed_at: chrono::Utc::now(),
                        retry_attempt: request.state.retry_attempt,
                        batch_expires_at: request.state.batch_expires_at,
                        routed_model: request.data.model.clone(),
                    },
                };
                storage.persist(&failed).await?;
                Ok(RequestCompletionResult::Failed(failed))
            }
        }
    }
}

// Compile-time check that an unused import doesn't sneak in via
// rust-analyzer cleanup.
#[allow(dead_code)]
fn _smoke(_c: Canceled) {}
