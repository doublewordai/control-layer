//! [`fusillade::HttpClient`] implementation that runs the multi-step
//! Open Responses loop instead of making a single HTTP call.
//!
//! Plugged into [`fusillade::Request::process`] by
//! [`DwctlRequestProcessor`](super::processor::DwctlRequestProcessor) so
//! the parent fusillade row transitions `claimed → processing` and
//! inherits fusillade's spawned-task / abort_handle / cancellation /
//! retry / persistence machinery — the loop just provides the body of
//! the would-be HTTP call.
//!
//! The synthesized response is the assembled `/v1/responses` JSON
//! (status 200 on success; status 500 + structured error body on
//! `LoopError`). `should_retry` decides the disposition like any other
//! upstream response.
//!
//! See `docs/responses-processor-design.md` for the broader design.

use std::sync::Arc;

use async_trait::async_trait;
use fusillade::http::{HttpClient as FusilladeHttpClient, HttpResponse};
use fusillade::{FusilladeError, PoolProvider as FusilladePool, RequestData, Result as FusilladeResult};
use onwards::client::HttpClient as OnwardsHttpClient;
use onwards::traits::{RequestContext, ToolExecutor};
use onwards::{LoopConfig, LoopError, MultiStepStore, UpstreamTarget};

use crate::responses::processor::DaemonToolResolver;
use crate::responses::store::{FusilladeResponseStore, PendingResponseInput};
use crate::tool_executor::ResolvedTools;

/// Runs the Open Responses multi-step loop as a single
/// [`fusillade::HttpClient::execute`] call.
///
/// Cloned per-request by `fusillade::Request::process`. All fields are
/// `Arc` / `Copy` so cloning is cheap.
pub struct ResponseLoopHttpClient<P, T>
where
    P: FusilladePool + Clone + Send + Sync + 'static,
    T: ToolExecutor + 'static,
{
    pub response_store: Arc<FusilladeResponseStore<P>>,
    pub tool_executor: Arc<T>,
    pub inner_http: Arc<dyn OnwardsHttpClient + Send + Sync>,
    pub tool_resolver: Option<Arc<dyn DaemonToolResolver>>,
    pub loop_config: LoopConfig,
}

impl<P, T> Clone for ResponseLoopHttpClient<P, T>
where
    P: FusilladePool + Clone + Send + Sync + 'static,
    T: ToolExecutor + 'static,
{
    fn clone(&self) -> Self {
        Self {
            response_store: self.response_store.clone(),
            tool_executor: self.tool_executor.clone(),
            inner_http: self.inner_http.clone(),
            tool_resolver: self.tool_resolver.clone(),
            loop_config: self.loop_config,
        }
    }
}

#[async_trait]
impl<P, T> FusilladeHttpClient for ResponseLoopHttpClient<P, T>
where
    P: FusilladePool + Clone + Send + Sync + 'static,
    T: ToolExecutor + 'static,
{
    async fn execute(&self, request: &RequestData, api_key: &str) -> FusilladeResult<HttpResponse> {
        let request_id = request.id.0.to_string();

        // Resolve tools. Tools today are scoped per (api_key, model alias)
        // — same join the realtime middleware does — so daemon-driven
        // multi-step requests see exactly the tools the original POST
        // would have seen.
        let mut tool_ctx = RequestContext::new().with_model(request.model.clone());
        let mut resolved_tool_names = std::collections::HashSet::new();
        if let Some(resolver) = &self.tool_resolver {
            match resolver.resolve(api_key, &request.model).await {
                Ok(Some(resolved)) => {
                    resolved_tool_names = resolved.tools.keys().cloned().collect();
                    tool_ctx = tool_ctx.with_extension(ResolvedTools(Arc::new(resolved)));
                }
                Ok(None) => {
                    tracing::debug!(
                        request_id = %request.id,
                        model = %request.model,
                        "no tools resolved for daemon-driven /v1/responses request"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        request_id = %request.id,
                        "tool resolution failed for daemon path; running loop with no tools"
                    );
                }
            }
        }

        // The transition function (`next_action_for`) re-parses the user's
        // `/v1/responses` body on every iteration — without a pending_input
        // registered under this request_id it errors at the first iteration.
        // The fusillade row's id is the same UUID the loop uses as its
        // request_id (and `record_step` reuses as the head step's id), so we
        // register under exactly that key.
        let api_key_opt = (!api_key.is_empty()).then(|| api_key.to_string());
        let created_by_opt = (!request.created_by.is_empty()).then(|| request.created_by.clone());
        let pending = PendingResponseInput {
            body: request.body.clone(),
            api_key: api_key_opt.clone(),
            created_by: created_by_opt,
            base_url: request.endpoint.clone(),
            resolved_tool_names,
        };
        if let Err(e) = self.response_store.register_pending_with_id(request.id.0, pending) {
            return Err(FusilladeError::Other(anyhow::anyhow!(
                "register pending input for daemon-driven /v1/responses: {e}"
            )));
        }

        // RAII drop of the side-channel entry: covers normal return, panic,
        // and abort_handle.abort() cancellation paths uniformly.
        let cleanup_store = self.response_store.clone();
        let cleanup_id = request_id.clone();
        let _pending_guard = scopeguard::guard((), move |_| {
            cleanup_store.unregister_pending(&cleanup_id);
        });

        // dwctl rewrites /v1/responses → /v1/chat/completions on the wire
        // since most upstreams speak chat-completions (transition.rs builds
        // the messages array). The row's `endpoint` carries the base URL.
        let upstream = UpstreamTarget {
            url: {
                let base = request.endpoint.trim_end_matches('/');
                format!("{base}/v1/chat/completions")
            },
            api_key: api_key_opt,
        };

        // Daemon path: no event sink. Streaming requests run inline on the
        // warm path (responses::streaming) with a sink wired to the SSE
        // response.
        let result = onwards::run_response_loop(
            &*self.response_store,
            &*self.tool_executor,
            &tool_ctx,
            &upstream,
            self.inner_http.clone(),
            None,
            &request_id,
            None,
            self.loop_config,
            0,
        )
        .await;

        // Synthesize the HttpResponse fusillade's Processing::complete
        // expects. The head sub-request row is finalized here (before
        // returning) so `GET /v1/responses/{id}` sees a terminal sub-row
        // regardless of how `should_retry` then classifies the parent.
        match result {
            Ok(_final_payload) => {
                let assembled = self
                    .response_store
                    .assemble_response(&request_id)
                    .await
                    .map_err(|e| FusilladeError::Other(anyhow::anyhow!("assemble_response after loop: {e}")))?;
                self.response_store
                    .finalize_head_request(&request_id, 200, assembled.clone())
                    .await
                    .map_err(|e| FusilladeError::Other(anyhow::anyhow!("finalize head sub-request: {e}")))?;
                let body = serde_json::to_string(&assembled)
                    .map_err(|e| FusilladeError::Other(anyhow::anyhow!("serialize assembled response: {e}")))?;
                Ok(HttpResponse { status: 200, body })
            }
            Err(LoopError::Failed(payload)) => {
                if let Err(e) = self.response_store.finalize_head_request(&request_id, 500, payload.clone()).await {
                    tracing::warn!(error = %e, request_id = %request_id, "Failed to finalize head sub-request after loop failure");
                }
                let body = serde_json::to_string(&payload).unwrap_or_default();
                Ok(HttpResponse { status: 500, body })
            }
            Err(other) => {
                let payload = serde_json::json!({
                    "type": "loop_error",
                    "message": other.to_string(),
                });
                if let Err(e) = self.response_store.finalize_head_request(&request_id, 500, payload.clone()).await {
                    tracing::warn!(error = %e, request_id = %request_id, "Failed to finalize head sub-request after unexpected loop error");
                }
                let body = serde_json::to_string(&payload).unwrap_or_default();
                Ok(HttpResponse { status: 500, body })
            }
        }
    }
}
