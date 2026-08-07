//! Server-side tool resolution helpers.
//!
//! [`ResolvedToolSet`] holds the tools resolved for a request (from `tool_sources`
//! joined with the user's groups + deployment) and renders them into the OpenAI
//! Chat Completions `tools` array that the tool-injection middleware splices into
//! the request body. [`ResolvedTools`] is the request-extension wrapper the
//! middleware inserts. The server-side tool loop these fed was removed in
//! COR-517, leaving this injection path non-functional (it advertises tools
//! nothing executes); it is slated for removal with the rest of #878 in COR-548.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
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
    /// Tool dispatch kind from `tool_sources.kind` (`"http"` / `"agent"`).
    /// Only meaningful for server-side execution, which COR-517 removed; the
    /// inject-only path never reads it. Slated for removal with #878 (COR-548).
    pub kind: String,
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

    /// Render the resolved tool set in OpenAI Chat Completions
    /// `tools: [{ type: "function", function: {...} }]` format. The
    /// tool-injection middleware splices this into the request body so the
    /// registered tools reach the upstream model.
    pub fn to_openai_tools_array(&self) -> Vec<serde_json::Value> {
        self.tools
            .keys()
            .map(|name| {
                let (description, parameters) = self.metadata.get(name).cloned().unwrap_or((None, None));
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": description.unwrap_or_default(),
                        "parameters": parameters.unwrap_or(serde_json::json!({"type": "object"})),
                    }
                })
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Extension types inserted by middleware into RequestContext
// ---------------------------------------------------------------------------

/// Resolved tool set inserted into the request's extensions by the tool
/// injection middleware, so downstream layers can render the tools into the
/// request body without another DB round-trip.
#[derive(Debug, Clone)]
pub struct ResolvedTools(pub Arc<ResolvedToolSet>);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolved_tool_set_to_openai_tools_array() {
        let mut tools = HashMap::new();
        tools.insert(
            "weather".to_string(),
            ToolDefinition {
                kind: "http".to_string(),
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
        let array = tool_set.to_openai_tools_array();
        assert_eq!(array.len(), 1);
        assert_eq!(array[0]["type"], "function");
        assert_eq!(array[0]["function"]["name"], "weather");
        assert_eq!(array[0]["function"]["description"], "Get the weather");
    }
}
