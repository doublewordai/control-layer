//! [`onwards::StepExecutor`] implementation backed by dwctl's tool registry.
//!
//! This is the bridge between the multi-step orchestration loop and dwctl's
//! existing tool execution machinery. The implementation is deliberately a
//! thin wrapper:
//!
//! - **Tool dispatch** — looks up the tool in the per-request
//!   [`ResolvedToolSet`], reads its `kind`, and either delegates to
//!   [`HttpToolExecutor::execute`] (for `kind = "http"`) or signals a
//!   sub-agent recursion (for `kind = "agent"`).
//! - **Model call** — fires upstream HTTP via a configured client. The
//!   integration test plugs in a wiremock URL; production wiring will
//!   route through onwards' load balancer (the same mechanism the
//!   single-step path uses) and is the focus of follow-up issue COR-349.
//!
//! The split mirrors the trait split in onwards: storage primitives go
//! through [`crate::responses::store::FusilladeResponseStore`]; tool and
//! model execution go through this type. Together they satisfy
//! `run_response_loop`'s two-trait signature without duplicating any of
//! dwctl's existing tool resolution or HTTP execution code.

use std::sync::Arc;

use async_trait::async_trait;
use onwards::traits::{RequestContext, ToolError, ToolExecutor};
use onwards::{ExecutorError, StepExecutor, ToolDispatch};
use serde_json::Value;

use crate::tool_executor::{HttpToolExecutor, ResolvedToolSet, ResolvedTools};

/// Per-request step executor. Constructed once per `/v1/responses`
/// request after the tool injection middleware has resolved the
/// effective tool set.
pub struct DwctlStepExecutor {
    /// Wraps existing `HttpToolExecutor` for non-sub-agent tool dispatch.
    /// Reusing the same type means tool-call analytics, headers, timeout
    /// handling, and JSON shape are all identical to the single-step
    /// path — no behavior split between in-process and multi-step tool
    /// execution.
    tool_executor: Arc<HttpToolExecutor>,
    /// Per-request resolved tools. Carries the tool kinds we need to
    /// distinguish HTTP tools from sub-agents at dispatch time.
    resolved_tools: Arc<ResolvedToolSet>,
    /// Model call execution. The integration test plugs in a wiremock
    /// URL; production wiring will use onwards' load balancer (COR-349).
    model_caller: Arc<dyn ModelCaller>,
}

/// Pluggable model-call execution.
///
/// Decouples [`DwctlStepExecutor`] from any specific upstream-routing
/// implementation. The integration test uses [`StaticModelCaller`]
/// pointing at a wiremock; production will pass an adapter over onwards'
/// load balancer + outlet middleware.
#[async_trait]
pub trait ModelCaller: Send + Sync {
    async fn call(&self, request_payload: &Value) -> Result<Value, ExecutorError>;
}

/// Single-target model caller. POSTs the request payload to a fixed URL
/// and returns the response JSON. Suitable for tests; production should
/// route through the load balancer.
pub struct StaticModelCaller {
    pub client: reqwest::Client,
    pub url: String,
    pub api_key: Option<String>,
}

#[async_trait]
impl ModelCaller for StaticModelCaller {
    async fn call(&self, request_payload: &Value) -> Result<Value, ExecutorError> {
        let mut req = self.client.post(&self.url).json(request_payload);
        if let Some(key) = &self.api_key {
            req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {key}"));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| ExecutorError::ExecutionError(format!("model call HTTP error: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ExecutorError::ExecutionError(format!(
                "model call returned HTTP {status}: {body}"
            )));
        }

        resp.json::<Value>()
            .await
            .map_err(|e| ExecutorError::ExecutionError(format!("model call body parse: {e}")))
    }
}

impl DwctlStepExecutor {
    pub fn new(
        tool_executor: Arc<HttpToolExecutor>,
        resolved_tools: Arc<ResolvedToolSet>,
        model_caller: Arc<dyn ModelCaller>,
    ) -> Self {
        Self {
            tool_executor,
            resolved_tools,
            model_caller,
        }
    }

    fn make_tool_ctx(&self) -> RequestContext {
        // The underlying HttpToolExecutor reads ResolvedTools from the
        // RequestContext extensions. We synthesize the context here so
        // we can reuse the same execute() method that the single-step
        // path uses — no behavior fork between paths.
        RequestContext::new().with_extension(ResolvedTools(self.resolved_tools.clone()))
    }
}

#[async_trait]
impl StepExecutor for DwctlStepExecutor {
    async fn execute_model_call(
        &self,
        _step_id: &str,
        request_payload: &Value,
    ) -> Result<Value, ExecutorError> {
        self.model_caller.call(request_payload).await
    }

    async fn dispatch_tool_call(
        &self,
        step_id: &str,
        request_payload: &Value,
    ) -> Result<ToolDispatch, ExecutorError> {
        // Convention: the transition function persists the tool call as
        // `{"name": "<tool>", "args": {...}}` (mirroring OpenAI's
        // function_call.arguments shape). The transition function owns
        // this convention; the executor just reads it.
        let tool_name = request_payload
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ExecutorError::ExecutionError(
                    "tool_call request_payload missing 'name'".into(),
                )
            })?;
        let args = request_payload
            .get("args")
            .cloned()
            .unwrap_or(serde_json::json!({}));

        let definition = self
            .resolved_tools
            .tools
            .get(tool_name)
            .ok_or_else(|| ExecutorError::NotFound(tool_name.to_string()))?;

        match definition.kind.as_str() {
            "agent" => {
                // Sub-agent dispatch: signal recursion. The loop will
                // call run_response_loop with scope_parent = Some(step_id).
                // Tool args (e.g. agent name, initial prompt) are
                // recorded on the spawning step's request_payload, so
                // when next_action_for is called inside the sub-loop the
                // transition function can read them via list_chain →
                // step at id step_id.
                Ok(ToolDispatch::Recurse)
            }
            "http" | _ => {
                let ctx = self.make_tool_ctx();
                let payload = self
                    .tool_executor
                    .execute(tool_name, step_id, &args, &ctx)
                    .await
                    .map_err(translate_tool_error)?;
                Ok(ToolDispatch::Executed(payload))
            }
        }
    }
}

fn translate_tool_error(e: ToolError) -> ExecutorError {
    match e {
        ToolError::NotFound(name) => ExecutorError::NotFound(name),
        ToolError::ExecutionError(msg) | ToolError::InvalidArguments(msg) | ToolError::Timeout(msg) => {
            ExecutorError::ExecutionError(msg)
        }
    }
}
