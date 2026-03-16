//! HTTP-based tool executor for server-side tool calling.
//!
//! [`HttpToolExecutor`] implements `onwards::ToolExecutor` by POSTing tool arguments as JSON to
//! a configured HTTP endpoint.  Per-request tool resolution is done via the `RequestContext`
//! that onwards threads through `tools()` and `execute()` — the context carries the model name
//! and an `http::Extensions` map where middleware inserts resolved user/group/deployment info.
//!
//! # Analytics
//!
//! Each tool call records a `tool_call_analytics` row via a fire-and-forget background write.

use async_trait::async_trait;
use chrono::Utc;
use onwards::traits::{RequestContext, ToolError, ToolExecutor, ToolSchema};
use serde_json::Value;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tracing::{Instrument, debug, info_span, instrument};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Per-tool configuration
// ---------------------------------------------------------------------------

/// Per-tool configuration resolved from the database.
#[derive(Debug, Clone)]
pub struct ToolDefinition {
    /// URL to POST tool arguments to.
    pub url: String,
    /// Optional Bearer token for the `Authorization` header.
    pub api_key: Option<String>,
    /// HTTP request timeout in seconds.
    pub timeout_secs: u64,
    /// Foreign key into `tool_sources` for analytics.
    pub tool_source_id: Uuid,
}

/// Full set of tools resolved for a single request.
#[derive(Debug, Clone)]
pub struct ResolvedToolSet {
    /// Resolved tool definitions: name → config.
    pub tools: HashMap<String, ToolDefinition>,
    /// Tool source metadata for schema injection: name → (description, parameters).
    pub metadata: HashMap<String, (Option<String>, Option<Value>)>,
}

impl ResolvedToolSet {
    pub fn new(tools: HashMap<String, ToolDefinition>, metadata: HashMap<String, (Option<String>, Option<Value>)>) -> Self {
        Self { tools, metadata }
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Convert resolved tools into `ToolSchema` values for onwards.
    pub fn to_tool_schemas(&self) -> Vec<ToolSchema> {
        self.tools
            .keys()
            .map(|name| {
                let (description, parameters) = self.metadata.get(name).cloned().unwrap_or((None, None));
                ToolSchema {
                    name: name.clone(),
                    description: description.unwrap_or_default(),
                    parameters: parameters.unwrap_or(serde_json::json!({"type": "object"})),
                    strict: false,
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Extension types inserted by middleware into RequestContext
// ---------------------------------------------------------------------------

/// Resolved tool set inserted into `RequestContext.extensions` by middleware.
///
/// The tool injection middleware resolves the effective tool set from the DB
/// and inserts this struct so that `HttpToolExecutor::tools()` and `execute()`
/// can access it without another DB round-trip.
#[derive(Debug, Clone)]
pub struct ResolvedTools(pub Arc<ResolvedToolSet>);

// ---------------------------------------------------------------------------
// HttpToolExecutor
// ---------------------------------------------------------------------------

/// Global `ToolExecutor` registered in the onwards `AppState`.
///
/// Uses `RequestContext.extensions` to access the per-request `ResolvedTools`
/// inserted by middleware.
pub struct HttpToolExecutor {
    client: reqwest::Client,
    pool: Option<Arc<PgPool>>,
}

impl std::fmt::Debug for HttpToolExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpToolExecutor").finish()
    }
}

impl HttpToolExecutor {
    pub fn new(client: reqwest::Client, pool: Option<Arc<PgPool>>) -> Self {
        Self { client, pool }
    }
}

#[async_trait]
impl ToolExecutor for HttpToolExecutor {
    async fn tools(&self, ctx: &RequestContext) -> Vec<ToolSchema> {
        match ctx.extensions.get::<ResolvedTools>() {
            Some(resolved) => resolved.0.to_tool_schemas(),
            None => {
                debug!("No ResolvedTools in RequestContext, returning empty tool set");
                Vec::new()
            }
        }
    }

    #[instrument(skip(self, arguments, ctx), fields(tool.name = %tool_name), err)]
    async fn execute(&self, tool_name: &str, _tool_call_id: &str, arguments: &Value, ctx: &RequestContext) -> Result<Value, ToolError> {
        let resolved = ctx
            .extensions
            .get::<ResolvedTools>()
            .ok_or_else(|| ToolError::ExecutionError("no tool set available for this request".to_string()))?;

        let definition = resolved
            .0
            .tools
            .get(tool_name)
            .ok_or_else(|| ToolError::NotFound(tool_name.to_string()))?;

        let started_at = Utc::now();
        let wall_start = Instant::now();

        let span = info_span!(
            "tool.execute",
            tool.name = %tool_name,
            tool.source_id = %definition.tool_source_id,
            http.url = %definition.url,
        );

        let (result, http_status, error_kind) = async {
            let mut req = self
                .client
                .post(&definition.url)
                .timeout(std::time::Duration::from_secs(definition.timeout_secs))
                .json(arguments);

            if let Some(key) = &definition.api_key {
                req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {key}"));
            }

            match req.send().await {
                Err(e) if e.is_timeout() => {
                    let msg = format!("tool '{}' timed out after {}s", tool_name, definition.timeout_secs);
                    (Err(ToolError::Timeout(msg)), None, Some("timeout"))
                }
                Err(e) => {
                    let msg = format!("tool '{}' connection error: {}", tool_name, e);
                    (Err(ToolError::ExecutionError(msg)), None, Some("connection_error"))
                }
                Ok(resp) => {
                    let status = resp.status();
                    let status_u16 = status.as_u16();

                    if status.is_success() {
                        match resp.bytes().await {
                            Err(e) => {
                                let msg = format!("failed to read tool response body: {e}");
                                (Err(ToolError::ExecutionError(msg)), Some(status_u16), Some("read_error"))
                            }
                            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                                Ok(json) => (Ok(json), Some(status_u16), None),
                                Err(_) => {
                                    let body = String::from_utf8_lossy(&bytes).into_owned();
                                    (Ok(serde_json::json!({"result": body})), Some(status_u16), None)
                                }
                            },
                        }
                    } else {
                        let body = resp.text().await.unwrap_or_default();
                        let msg = format!("HTTP {}: {}", status_u16, body);
                        (Err(ToolError::ExecutionError(msg)), Some(status_u16), Some("http_error"))
                    }
                }
            }
        }
        .instrument(span)
        .await;

        let duration_ms = wall_start.elapsed().as_millis() as i64;
        let success = result.is_ok();
        let http_status_i32 = http_status.map(|s| s as i32);
        let tool_source_id = definition.tool_source_id;
        let tool_name_owned = tool_name.to_string();
        let error_kind_owned = error_kind.map(|s| s.to_string());

        // Fire-and-forget analytics write.
        if let Some(pool) = &self.pool {
            let pool = pool.clone();
            tokio::spawn(async move {
                let res = sqlx::query!(
                    r#"
                    INSERT INTO tool_call_analytics
                        (analytics_id, tool_source_id, tool_name, started_at, duration_ms,
                         http_status_code, success, error_kind)
                    VALUES (NULL, $1, $2, $3, $4, $5, $6, $7)
                    "#,
                    tool_source_id,
                    tool_name_owned,
                    started_at,
                    duration_ms,
                    http_status_i32,
                    success,
                    error_kind_owned.as_deref(),
                )
                .execute(&*pool)
                .await;

                if let Err(e) = res {
                    tracing::warn!(error = %e, "Failed to record tool_call_analytics");
                }
            });
        }

        result
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn make_executor_and_ctx(tool_name: &str, server_url: &str, api_key: Option<String>) -> (HttpToolExecutor, RequestContext) {
        let client = reqwest::Client::new();
        let executor = HttpToolExecutor::new(client, None);

        let mut tools = HashMap::new();
        tools.insert(
            tool_name.to_string(),
            ToolDefinition {
                url: format!("{server_url}/tool"),
                api_key,
                timeout_secs: 5,
                tool_source_id: Uuid::nil(),
            },
        );
        let resolved = ResolvedToolSet::new(tools, HashMap::new());
        let ctx = RequestContext::new().with_extension(ResolvedTools(Arc::new(resolved)));

        (executor, ctx)
    }

    #[tokio::test]
    async fn test_execute_returns_json_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tool"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"result": 42})))
            .mount(&server)
            .await;

        let (executor, ctx) = make_executor_and_ctx("test_tool", &server.uri(), None);
        let result = executor.execute("test_tool", "id1", &serde_json::json!({"x": 1}), &ctx).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), serde_json::json!({"result": 42}));
    }

    #[tokio::test]
    async fn test_execute_wraps_non_json_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tool"))
            .respond_with(ResponseTemplate::new(200).set_body_string("hello world"))
            .mount(&server)
            .await;

        let (executor, ctx) = make_executor_and_ctx("test_tool", &server.uri(), None);
        let result = executor.execute("test_tool", "id1", &serde_json::json!({}), &ctx).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), serde_json::json!({"result": "hello world"}));
    }

    #[tokio::test]
    async fn test_execute_sends_auth_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tool"))
            .and(header("authorization", "Bearer my-secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;

        let (executor, ctx) = make_executor_and_ctx("test_tool", &server.uri(), Some("my-secret".to_string()));
        let result = executor.execute("test_tool", "id1", &serde_json::json!({}), &ctx).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_execute_returns_error_on_4xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tool"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
            .mount(&server)
            .await;

        let (executor, ctx) = make_executor_and_ctx("test_tool", &server.uri(), None);
        let result = executor.execute("test_tool", "id1", &serde_json::json!({}), &ctx).await;
        assert!(matches!(result, Err(ToolError::ExecutionError(_))));
    }

    #[tokio::test]
    async fn test_execute_returns_error_on_5xx() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tool"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal error"))
            .mount(&server)
            .await;

        let (executor, ctx) = make_executor_and_ctx("test_tool", &server.uri(), None);
        let result = executor.execute("test_tool", "id1", &serde_json::json!({}), &ctx).await;
        assert!(matches!(result, Err(ToolError::ExecutionError(_))));
    }

    #[tokio::test]
    async fn test_tools_returns_schemas_from_context() {
        let client = reqwest::Client::new();
        let executor = HttpToolExecutor::new(client, None);

        let mut tools = HashMap::new();
        tools.insert(
            "my_tool".to_string(),
            ToolDefinition {
                url: "http://example.com".to_string(),
                api_key: None,
                timeout_secs: 5,
                tool_source_id: Uuid::nil(),
            },
        );
        let mut metadata = HashMap::new();
        metadata.insert("my_tool".to_string(), (Some("Does stuff".to_string()), None));
        let resolved = ResolvedToolSet::new(tools, metadata);
        let ctx = RequestContext::new().with_extension(ResolvedTools(Arc::new(resolved)));

        let schemas = executor.tools(&ctx).await;
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, "my_tool");
        assert_eq!(schemas[0].description, "Does stuff");
    }

    #[tokio::test]
    async fn test_tools_returns_empty_without_context() {
        let client = reqwest::Client::new();
        let executor = HttpToolExecutor::new(client, None);
        let ctx = RequestContext::new();

        let schemas = executor.tools(&ctx).await;
        assert!(schemas.is_empty());
    }

    #[tokio::test]
    async fn test_not_found_for_unknown_tool() {
        let (executor, ctx) = make_executor_and_ctx("test_tool", "http://localhost:1", None);
        let result = executor.execute("unknown", "id1", &serde_json::json!({}), &ctx).await;
        assert!(matches!(result, Err(ToolError::NotFound(_))));
    }

    #[tokio::test]
    async fn test_execute_returns_timeout_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/tool"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": true}))
                    .set_delay(std::time::Duration::from_secs(3)),
            )
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let executor = HttpToolExecutor::new(client, None);

        let mut tools = HashMap::new();
        tools.insert(
            "test_tool".to_string(),
            ToolDefinition {
                url: format!("{}/tool", server.uri()),
                api_key: None,
                timeout_secs: 1,
                tool_source_id: Uuid::nil(),
            },
        );
        let resolved = ResolvedToolSet::new(tools, HashMap::new());
        let ctx = RequestContext::new().with_extension(ResolvedTools(Arc::new(resolved)));

        let result = executor.execute("test_tool", "id1", &serde_json::json!({}), &ctx).await;
        assert!(matches!(result, Err(ToolError::Timeout(_))));
    }

    #[tokio::test]
    async fn test_execute_returns_connection_error() {
        let (executor, ctx) = make_executor_and_ctx("test_tool", "http://127.0.0.1:1", None);
        let result = executor.execute("test_tool", "id1", &serde_json::json!({}), &ctx).await;
        assert!(matches!(result, Err(ToolError::ExecutionError(_))));
    }

    #[test]
    fn test_resolved_tool_set_to_tool_schemas() {
        let mut tools = HashMap::new();
        tools.insert(
            "weather".to_string(),
            ToolDefinition {
                url: "http://example.com".to_string(),
                api_key: None,
                timeout_secs: 30,
                tool_source_id: Uuid::nil(),
            },
        );
        let mut metadata = HashMap::new();
        metadata.insert(
            "weather".to_string(),
            (
                Some("Get the weather".to_string()),
                Some(serde_json::json!({
                    "type": "object",
                    "properties": {"location": {"type": "string"}},
                    "required": ["location"]
                })),
            ),
        );

        let tool_set = ResolvedToolSet::new(tools, metadata);
        let schemas = tool_set.to_tool_schemas();
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0].name, "weather");
        assert_eq!(schemas[0].description, "Get the weather");
    }
}
