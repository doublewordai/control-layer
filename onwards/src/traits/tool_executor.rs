//! Tool execution trait for server-side tool handling
//!
//! Implement this trait to enable automatic tool execution during agent loops.
//! When the model calls a tool that your executor provides (via `tools()`),
//! the adapter will execute it locally and feed the result back to the model.

use async_trait::async_trait;
use std::fmt;

/// Per-request context threaded through the tool executor.
///
/// Carries the model name (for per-deployment resolution) and arbitrary
/// extension data inserted by middleware (e.g. resolved user/group info).
///
/// # Extensions
///
/// The `extensions` field is an in-process type map — it does **not** transit
/// HTTP. It exists primarily for **library mode**, where onwards is embedded in
/// another service: middleware layers can insert typed data (e.g. resolved
/// user/group info, tenant context) that the `ToolExecutor` implementation
/// reads when executing tools.
///
/// In **service mode** (onwards running as a standalone proxy), the
/// `ToolExecutor` implementation typically resolves tools from its own state
/// (database, config files, etc.) using information already present in the
/// request such as the model name or auth headers, and `extensions` is unused.
#[derive(Debug, Default)]
pub struct RequestContext {
    /// The model alias from the request body (if available).
    pub model: Option<String>,
    /// Arbitrary typed data inserted by middleware layers.
    pub extensions: axum::http::Extensions,
}

impl RequestContext {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn with_extension<T: Clone + Send + Sync + 'static>(mut self, val: T) -> Self {
        self.extensions.insert(val);
        self
    }
}

/// Dispatch kind for a server-side tool.
///
/// Onwards stays agnostic to the underlying implementation (HTTP, MCP,
/// sandboxed execution, etc.) — `Http` covers anything the executor
/// fires-and-returns through [`ToolExecutor::execute`]. `Agent` is the
/// only special case: when the multi-step orchestration loop encounters
/// a step whose tool has `kind = Agent`, it recurses into a sub-loop
/// scoped under that step instead of calling `execute`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolKind {
    /// Standard tool. The loop calls [`ToolExecutor::execute`] and
    /// persists the returned payload as the step's `response_payload`.
    #[default]
    Http,
    /// Sub-agent dispatch. The loop recurses with `scope_parent =
    /// Some(step_id)` and `depth + 1`. The sub-loop's final return value
    /// becomes the spawning step's `response_payload`.
    Agent,
}

/// Schema for a server-side tool, returned by [`ToolExecutor::tools`].
#[derive(Debug, Clone)]
pub struct ToolSchema {
    /// Tool name (must be unique within a request).
    pub name: String,
    /// Human-readable description shown to the model.
    pub description: String,
    /// JSON Schema for the tool's parameters.
    pub parameters: serde_json::Value,
    /// Whether to enforce strict schema adherence (OpenAI-specific).
    pub strict: bool,
    /// Dispatch kind. Defaults to [`ToolKind::Http`] so existing
    /// implementations that don't set this field continue to behave
    /// exactly as before.
    #[doc(hidden)] // hide from rendered docs while the field is opt-in
    pub kind: ToolKind,
}

/// Error type for tool execution
#[derive(Debug, Clone)]
pub enum ToolError {
    /// Tool is not recognized/supported
    NotFound(String),
    /// Tool execution failed
    ExecutionError(String),
    /// Invalid arguments provided to tool
    InvalidArguments(String),
    /// Tool execution timed out
    Timeout(String),
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolError::NotFound(name) => write!(f, "Tool not found: {}", name),
            ToolError::ExecutionError(msg) => write!(f, "Tool execution error: {}", msg),
            ToolError::InvalidArguments(msg) => write!(f, "Invalid arguments: {}", msg),
            ToolError::Timeout(msg) => write!(f, "Tool timeout: {}", msg),
        }
    }
}

impl std::error::Error for ToolError {}

/// Trait for executing tools server-side during agent loops.
///
/// Implement this to enable automatic tool execution in the Open Responses adapter.
/// The adapter will:
/// 1. Call `tools()` with the request context to discover available server-side tools
/// 2. Merge server-side tool schemas with any client-provided tools
/// 3. When the model returns a tool call for a server-side tool, call `execute()`
/// 4. Feed the result back to the model and continue until completion
///
/// Tools that the executor does not provide (i.e. client-side tools) will be
/// returned to the client with `requires_action` status.
///
/// # Example
///
/// ```ignore
/// use onwards::traits::{ToolExecutor, ToolError, ToolSchema, RequestContext};
/// use async_trait::async_trait;
///
/// struct WeatherTool;
///
/// #[async_trait]
/// impl ToolExecutor for WeatherTool {
///     async fn tools(&self, _ctx: &RequestContext) -> Vec<ToolSchema> {
///         vec![ToolSchema {
///             name: "get_weather".to_string(),
///             description: "Get current weather for a location".to_string(),
///             parameters: serde_json::json!({
///                 "type": "object",
///                 "properties": {"location": {"type": "string"}},
///                 "required": ["location"]
///             }),
///             strict: false,
///         }]
///     }
///
///     async fn execute(
///         &self,
///         tool_name: &str,
///         tool_call_id: &str,
///         arguments: &serde_json::Value,
///         ctx: &RequestContext,
///     ) -> Result<serde_json::Value, ToolError> {
///         if tool_name == "get_weather" {
///             let location = arguments["location"].as_str()
///                 .ok_or_else(|| ToolError::InvalidArguments("missing location".into()))?;
///             Ok(serde_json::json!({"temperature": 72, "conditions": "sunny"}))
///         } else {
///             Err(ToolError::NotFound(tool_name.to_string()))
///         }
///     }
/// }
/// ```
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Return the tool schemas available for this request context.
    ///
    /// Called once per request to discover server-side tools. The returned schemas
    /// are merged with any client-provided tools and sent to the upstream model.
    ///
    /// Return an empty vec if no server-side tools are available.
    async fn tools(&self, ctx: &RequestContext) -> Vec<ToolSchema>;

    /// Execute a tool call and return the result.
    ///
    /// # Arguments
    /// * `tool_name` - The name of the tool to execute
    /// * `tool_call_id` - The unique ID for this tool call (for correlation)
    /// * `arguments` - The arguments passed to the tool (as JSON)
    /// * `ctx` - The per-request context
    ///
    /// # Returns
    /// * `Ok(Value)` - The tool's output as JSON
    /// * `Err(ToolError)` - If execution failed
    async fn execute(
        &self,
        tool_name: &str,
        tool_call_id: &str,
        arguments: &serde_json::Value,
        ctx: &RequestContext,
    ) -> Result<serde_json::Value, ToolError>;
}

/// No-op implementation that handles no tools.
///
/// This is the default implementation used when no executor is configured.
/// All tool calls will be returned to the client with `requires_action` status.
#[derive(Debug, Clone, Default)]
pub struct NoOpToolExecutor;

#[async_trait]
impl ToolExecutor for NoOpToolExecutor {
    async fn tools(&self, _ctx: &RequestContext) -> Vec<ToolSchema> {
        Vec::new()
    }

    async fn execute(
        &self,
        tool_name: &str,
        _tool_call_id: &str,
        _arguments: &serde_json::Value,
        _ctx: &RequestContext,
    ) -> Result<serde_json::Value, ToolError> {
        Err(ToolError::NotFound(tool_name.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_noop_executor_returns_no_tools() {
        let executor = NoOpToolExecutor;
        let ctx = RequestContext::new();
        assert!(executor.tools(&ctx).await.is_empty());
    }

    #[tokio::test]
    async fn test_noop_executor_returns_not_found() {
        let executor = NoOpToolExecutor;
        let ctx = RequestContext::new();
        let result = executor
            .execute("test_tool", "call_123", &serde_json::json!({}), &ctx)
            .await;
        assert!(matches!(result, Err(ToolError::NotFound(_))));
    }

    #[test]
    fn test_request_context_builder() {
        let ctx = RequestContext::new()
            .with_model("gpt-4o")
            .with_extension(42u32);
        assert_eq!(ctx.model.as_deref(), Some("gpt-4o"));
        assert_eq!(ctx.extensions.get::<u32>(), Some(&42));
    }
}
