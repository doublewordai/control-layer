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
use fusillade::request::{Canceled, Claimed, Completed, Failed, Request, RequestCompletionResult};
use fusillade::{
    CancellationFuture, DefaultRequestProcessor, FailureReason, PoolProvider as FusilladePool, RequestProcessor, ShouldRetry, Storage,
};
use onwards::client::HttpClient;
use onwards::traits::{RequestContext, ToolExecutor};
use onwards::{LoopConfig, LoopError, MultiStepStore, UpstreamTarget};

use crate::responses::store::{FusilladeResponseStore, PendingResponseInput};
use crate::tool_executor::ResolvedTools;

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
    pub http_client: Arc<dyn HttpClient + Send + Sync>,
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
    /// Default processor used for non-`/v1/responses` endpoints. Owns
    /// no state — declared as a field so the trait dispatch below has
    /// a stable receiver.
    pub default: DefaultRequestProcessor,
}

/// Resolve the tool set for a daemon-claimed request. Called once per
/// `/v1/responses` claim before the loop runs. The default production
/// implementation (see [`DbToolResolver`]) runs the same DB join the
/// realtime middleware does, scoped to the row's API key and model
/// alias.
#[async_trait]
pub trait DaemonToolResolver: Send + Sync {
    async fn resolve(&self, api_key: &str, model_alias: &str) -> Result<Option<crate::tool_executor::ResolvedToolSet>, anyhow::Error>;
}

/// Production [`DaemonToolResolver`] backed by the same query the
/// realtime tool injection middleware uses.
pub struct DbToolResolver {
    pub pool: sqlx::PgPool,
}

#[async_trait]
impl DaemonToolResolver for DbToolResolver {
    async fn resolve(&self, api_key: &str, model_alias: &str) -> Result<Option<crate::tool_executor::ResolvedToolSet>, anyhow::Error> {
        crate::tool_injection::resolve_tools_for_request(&self.pool, api_key, Some(model_alias)).await
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
        http_client: Arc<dyn HttpClient + Send + Sync>,
        loop_config: LoopConfig,
    ) -> Self {
        Self {
            response_store,
            tool_executor,
            http_client,
            loop_config,
            tool_resolver: None,
            default: DefaultRequestProcessor,
        }
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
            return self.default.process(request, http, storage, should_retry, cancellation).await;
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

        // Build the per-request RequestContext the same way the
        // realtime middleware does. Tools today are scoped per
        // (api_key, model_alias); the resolver runs the same DB join
        // and produces a ResolvedToolSet, which is injected into the
        // context via the same `ResolvedTools` extension that
        // HttpToolExecutor reads at execute() time. This means
        // daemon-driven multi-step requests see exactly the same
        // tool set their original /v1/responses POST would have seen
        // — no daemon-vs-realtime tool-availability divergence.
        let mut tool_ctx = RequestContext::new().with_model(request.data.model.clone());
        let mut resolved_tool_names = std::collections::HashSet::new();
        if let Some(resolver) = &self.tool_resolver {
            match resolver.resolve(&request.data.api_key, &request.data.model).await {
                Ok(Some(resolved)) => {
                    resolved_tool_names = resolved.tools.keys().cloned().collect();
                    tool_ctx = tool_ctx.with_extension(ResolvedTools(Arc::new(resolved)));
                }
                Ok(None) => {
                    tracing::debug!(
                        request_id = %request.data.id,
                        model = %request.data.model,
                        "no tools resolved for daemon-driven /v1/responses request"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        request_id = %request.data.id,
                        "tool resolution failed for daemon path; running loop with no tools"
                    );
                }
            }
        }

        // The transition function (`next_action_for`) re-parses the
        // user's `/v1/responses` body on every iteration to build the
        // chat-completions messages array; without a `pending_input`
        // registered under this request_id it errors at the first
        // iteration. The warm path stashes this synchronously before
        // kicking off the loop; the daemon path has to do the same
        // here, sourcing the body and per-request fields from the
        // claimed fusillade row. Without this, daemon-driven
        // multi-step requests fail with "no pending input registered"
        // immediately — which is why `service_tier:"flex"` couldn't
        // safely fall through to `handle_flex` until now.
        //
        // The fusillade row's `id` is the same UUID the loop uses as
        // its `request_id` (and that `record_step` reuses as the head
        // step's id), so we register under exactly that key.
        let pending = PendingResponseInput {
            body: request.data.body.clone(),
            api_key: if request.data.api_key.is_empty() {
                None
            } else {
                Some(request.data.api_key.clone())
            },
            created_by: if request.data.created_by.is_empty() {
                None
            } else {
                Some(request.data.created_by.clone())
            },
            base_url: request.data.endpoint.clone(),
            resolved_tool_names,
        };
        if let Err(e) = self.response_store.register_pending_with_id(request.data.id.0, pending) {
            // Fail the request immediately with the real cause rather
            // than letting the loop surface a confusing
            // "no pending input registered" error that doesn't
            // mention the lock at all. Lock poisoning is rare in
            // practice (would require a panic while holding the
            // mutex), but when it does happen we want the failure
            // mode to be diagnosable from the audit log alone.
            return Err(fusillade::FusilladeError::Other(anyhow::anyhow!(
                "register pending input for daemon-driven /v1/responses: {e}"
            )));
        }

        // RAII cleanup: even if the loop panics or the daemon task
        // is cancelled mid-await, the side-channel entry must be
        // dropped — otherwise `pending_inputs` grows unbounded
        // across daemon-claimed requests. An explicit
        // `unregister_pending` call after the loop wouldn't run on
        // task cancellation (the future is dropped at the await
        // point); the guard's `Drop` runs in either path.
        let cleanup_store = self.response_store.clone();
        let cleanup_id = request_id.clone();
        let _pending_guard = scopeguard::guard((), move |_| {
            cleanup_store.unregister_pending(&cleanup_id);
        });

        // Daemon path: no event sink — the user's HTTP connection is
        // long gone by the time we claim. Streaming requests use the
        // warm path (responses::streaming module) which runs the loop
        // inline with a sink wired to the SSE response.
        let result = onwards::run_response_loop(
            &*self.response_store,
            &*self.tool_executor,
            &tool_ctx,
            &upstream,
            self.http_client.clone(),
            None,
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
                    .map_err(|e| fusillade::FusilladeError::Other(anyhow::anyhow!("assemble_response after loop: {e}")))?;
                // Finalize the head step's sub-request row before the
                // parent. Sub-request rows are created in `processing`
                // state by `create_sub_request_row` and rely on the
                // loop's caller to UPDATE them on terminal — see the
                // comment block in `responses/store.rs` next to the
                // `initial_state: "processing"` literal. Without this,
                // the row that `GET /v1/responses/{id}` reads from
                // (head_step.request_id → fusillade.requests row) stays
                // stuck in `processing` forever, and a polling client
                // sees `"status":"in_progress"` indefinitely even after
                // the parent below transitions to Completed. The warm
                // path does the same call via
                // `streaming::persist_terminal_completed`; the daemon
                // path is the only multi-step caller that was missing
                // it.
                self.response_store
                    .finalize_head_request(&request_id, 200, assembled.clone())
                    .await
                    .map_err(|e| fusillade::FusilladeError::Other(anyhow::anyhow!("finalize head sub-request: {e}")))?;
                let body = serde_json::to_string(&assembled)
                    .map_err(|e| fusillade::FusilladeError::Other(anyhow::anyhow!("serialize assembled response: {e}")))?;
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
                // Mirror the success path: finalize the head sub-request
                // row before transitioning the parent. Without this,
                // GET /v1/responses/{id} would keep returning
                // `"status":"in_progress"` after the loop has already
                // failed.
                if let Err(e) = self.response_store.finalize_head_request(&request_id, 500, payload.clone()).await {
                    tracing::warn!(error = %e, request_id = %request_id, "Failed to finalize head sub-request after loop failure");
                }
                let body = serde_json::to_string(&payload).unwrap_or_default();
                let failed = Request {
                    data: request.data.clone(),
                    state: Failed {
                        reason: FailureReason::NonRetriableHttpStatus { status: 500, body },
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
                //
                // Same head-sub-request finalization concern as the
                // success / `LoopError::Failed` arms above — without
                // this, polling clients see stale `in_progress`. We
                // do best-effort here: the head step / sub-request
                // row may not exist at all if the loop crashed before
                // the first record_step, which is exactly the no-op
                // path inside `finalize_head_request`.
                let payload = serde_json::json!({
                    "type": "loop_error",
                    "message": other.to_string(),
                });
                if let Err(e) = self.response_store.finalize_head_request(&request_id, 500, payload).await {
                    tracing::warn!(error = %e, request_id = %request_id, "Failed to finalize head sub-request after unexpected loop error");
                }
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
