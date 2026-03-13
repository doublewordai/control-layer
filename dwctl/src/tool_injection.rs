//! Middleware for per-request tool schema injection and tool set resolution.
//!
//! This middleware runs on the `/ai/v1/*` routes (specifically `/v1/responses` once nested).
//! For each request it:
//!
//! 1. Extracts the API key secret from the `Authorization: Bearer` header.
//! 2. Queries the database to map the secret → user_id → group_ids and
//!    secret → deployment_id.
//! 3. Computes the *effective tool set* as the intersection of:
//!    - tools attached to the deployment (`deployment_tool_sources`)
//!    - tools attached to at least one group the user belongs to (`group_tool_sources`)
//! 4. If client-requested tools are present in the request body, further restricts to those.
//! 5. Injects the authorised tool schemas into the request body (adds/merges the `tools` array).
//! 6. Registers the resolved tool set in the [`PerRequestToolRegistry`] under a fresh UUID and
//!    stores that UUID in the [`CURRENT_TOOL_REQUEST_ID`] task-local for the executor to read.
//! 7. Cleans up the registry entry when the request completes.
//!
//! The middleware is a no-op for requests that carry no API key or for paths that do not need
//! tool injection (only `/v1/responses` supports server-side tools; other paths are passed
//! through unchanged).

use crate::tool_executor::{PerRequestToolRegistry, ResolvedToolSet, ToolDefinition, CURRENT_TOOL_REQUEST_ID};
use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Request},
    middleware::Next,
    response::Response,
};
use serde_json::Value;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, warn};
use uuid::Uuid;

/// Shared state threaded through the middleware.
#[derive(Clone)]
pub struct ToolInjectionState {
    pub db: PgPool,
    pub registry: Arc<PerRequestToolRegistry>,
}

/// Axum middleware function.
///
/// Extracts tool context from the database, injects schemas into the request body, and
/// wraps the `next.run(req)` call inside the task-local scope so the executor can resolve
/// the per-request tool set.
pub async fn tool_injection_middleware(
    State(state): State<ToolInjectionState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    // Only act on the Responses API path — chat/completions etc. don't go through the
    // tool loop orchestration in onwards' OpenResponsesAdapter.
    let path = request.uri().path();
    let is_responses_path = path.ends_with("/responses") || path.contains("/responses?");

    if !is_responses_path {
        return next.run(request).await;
    }

    // Extract bearer token from the Authorization header.
    let bearer_token = match extract_bearer_token(request.headers()) {
        Some(t) => t,
        None => return next.run(request).await,
    };

    // Resolve deployment and group tool sets from the DB.
    let resolved = match resolve_tools_for_request(&state.db, &bearer_token).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            // No tool sources configured for this deployment/group combination — pass through.
            return CURRENT_TOOL_REQUEST_ID.scope(None, next.run(request)).await;
        }
        Err(e) => {
            warn!(error = %e, "Failed to resolve tool sources for request; proceeding without tools");
            return CURRENT_TOOL_REQUEST_ID.scope(None, next.run(request)).await;
        }
    };

    // If no effective tools, skip injection.
    if resolved.is_empty() {
        return CURRENT_TOOL_REQUEST_ID.scope(None, next.run(request)).await;
    }

    // Read and potentially modify the request body to inject tool schemas.
    let (modified, resolved) = match inject_tool_schemas(request, resolved).await {
        Ok((req, r)) => {
            request = req;
            (true, r)
        }
        Err((req, r)) => {
            // Injection failed (e.g., body not parseable as JSON) — proceed without injection.
            request = req;
            (false, r)
        }
    };

    if !modified {
        return CURRENT_TOOL_REQUEST_ID.scope(None, next.run(request)).await;
    }

    // Register the resolved tool set and run the handler inside the task-local scope.
    let request_id = Uuid::new_v4();
    state.registry.insert(request_id, resolved);

    // Use scopeguard to ensure cleanup when the request completes.
    let registry = state.registry.clone();
    let _guard = scopeguard::guard((), move |_| {
        registry.remove(request_id);
    });

    CURRENT_TOOL_REQUEST_ID
        .scope(Some(request_id), async move { next.run(request).await })
        .await
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract the Bearer token from the `Authorization` header, stripping the prefix.
fn extract_bearer_token(headers: &HeaderMap) -> Option<String> {
    let auth = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    auth.strip_prefix("Bearer ").map(|s| s.to_string())
}

/// Resolve the effective tool set for a request identified by its API key secret.
///
/// Returns `None` if no tools are configured for this deployment/group combination.
async fn resolve_tools_for_request(
    db: &PgPool,
    bearer_token: &str,
) -> anyhow::Result<Option<ResolvedToolSet>> {
    // Query the intersection of deployment tools and group tools in one shot.
    //
    // The API key identifies:
    //  - a user (via api_keys.user_id)
    //  - we do NOT restrict to a specific deployment here because the model name is in the body,
    //    not easily available at middleware time. Instead we take the union of all deployments the
    //    key can access and intersect with the group tools.
    //
    // For a tighter intersection (per-deployment), the middleware would need to parse the body to
    // extract the model name and then join against deployed_models — that is left as a future
    // enhancement. For now we resolve tools that are attached to ANY deployment this key can
    // access AND to ANY group the user belongs to.
    let rows = sqlx::query!(
        r#"
        SELECT DISTINCT
            ts.id           AS "tool_source_id!",
            ts.name         AS "name!",
            ts.description,
            ts.parameters,
            ts.url          AS "url!",
            ts.api_key,
            ts.timeout_secs AS "timeout_secs!"
        FROM api_keys ak
        -- Resolve user's groups
        INNER JOIN user_groups ug ON ug.user_id = ak.user_id
        -- Find deployments accessible via those groups
        INNER JOIN deployment_groups dg ON dg.group_id = ug.group_id
        -- Tools attached to those deployments
        INNER JOIN deployment_tool_sources dts ON dts.deployment_id = dg.deployment_id
        -- Same tool must also be attached to one of the user's groups
        INNER JOIN group_tool_sources gts ON gts.tool_source_id = dts.tool_source_id AND gts.group_id = ug.group_id
        -- Tool source details
        INNER JOIN tool_sources ts ON ts.id = dts.tool_source_id
        WHERE ak.secret = $1
          AND ak.is_deleted = FALSE
        ORDER BY ts.name
        "#,
        bearer_token,
    )
    .fetch_all(db)
    .await?;

    if rows.is_empty() {
        return Ok(None);
    }

    let mut tools: HashMap<String, ToolDefinition> = HashMap::new();
    let mut metadata: HashMap<String, (Option<String>, Option<Value>)> = HashMap::new();

    for row in rows {
        let name = row.name;
        tools.insert(
            name.clone(),
            ToolDefinition {
                url: row.url,
                api_key: row.api_key,
                timeout_secs: row.timeout_secs as u64,
                tool_source_id: row.tool_source_id,
            },
        );
        metadata.insert(name, (row.description, row.parameters));
    }

    Ok(Some(ResolvedToolSet::new(tools, metadata)))
}

/// Inject tool schemas into the request body.
///
/// Returns `Ok((modified_request, tool_set))` on success or `Err((original_request, tool_set))`
/// if the body cannot be parsed as JSON.
async fn inject_tool_schemas(
    mut request: Request<Body>,
    resolved: ResolvedToolSet,
) -> Result<(Request<Body>, ResolvedToolSet), (Request<Body>, ResolvedToolSet)> {
    // Read the body.
    let body_bytes = match axum::body::to_bytes(std::mem::take(request.body_mut()), 10 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            // Restore empty body and bail.
            *request.body_mut() = Body::empty();
            return Err((request, resolved));
        }
    };

    // Parse as JSON.
    let mut json: Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(_) => {
            // Restore original body.
            *request.body_mut() = Body::from(body_bytes);
            return Err((request, resolved));
        }
    };

    // Get the client-requested tool names from the body, if any.
    let client_requested: Option<std::collections::HashSet<String>> = json
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()))
                .map(|s| s.to_string())
                .collect()
        });

    // Further restrict tool set to the intersection with client-requested tools.
    let resolved = if let Some(requested) = client_requested {
        let tools: HashMap<String, ToolDefinition> = resolved
            .tools
            .into_iter()
            .filter(|(name, _)| requested.contains(name))
            .collect();
        let metadata: HashMap<String, (Option<String>, Option<Value>)> = resolved
            .metadata
            .into_iter()
            .filter(|(name, _)| tools.contains_key(name))
            .collect();
        ResolvedToolSet::new(tools, metadata)
    } else {
        resolved
    };

    if resolved.is_empty() {
        // No tools left after intersection; restore the original body.
        *request.body_mut() = Body::from(body_bytes);
        return Err((request, resolved));
    }

    // Inject the authorised tool schemas, replacing any client-provided ones.
    let schemas: Vec<Value> = resolved.to_openai_schemas();
    debug!(tool_count = schemas.len(), "Injecting tool schemas into request body");

    if let Value::Object(ref mut map) = json {
        map.insert("tools".to_string(), Value::Array(schemas));
    }

    let new_body = match serde_json::to_vec(&json) {
        Ok(b) => b,
        Err(_) => {
            *request.body_mut() = Body::from(body_bytes);
            return Err((request, resolved));
        }
    };

    // Update Content-Length header to reflect the modified body.
    let new_len = new_body.len();
    if let Some(content_length) = request.headers_mut().get_mut(axum::http::header::CONTENT_LENGTH) {
        *content_length = new_len.to_string().parse().unwrap_or_else(|_| content_length.clone());
    }

    *request.body_mut() = Body::from(new_body);
    Ok((request, resolved))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_bearer_token() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer sk-abc123".parse().unwrap(),
        );
        assert_eq!(extract_bearer_token(&headers), Some("sk-abc123".to_string()));
    }

    #[test]
    fn test_extract_bearer_token_missing() {
        let headers = HeaderMap::new();
        assert_eq!(extract_bearer_token(&headers), None);
    }

    #[test]
    fn test_extract_bearer_token_no_prefix() {
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::AUTHORIZATION, "sk-abc123".parse().unwrap());
        assert_eq!(extract_bearer_token(&headers), None);
    }

    #[test]
    fn test_resolved_tool_set_empty() {
        let ts = ResolvedToolSet::new(HashMap::new(), HashMap::new());
        assert!(ts.is_empty());
    }

    #[test]
    fn test_resolved_tool_set_schemas() {
        let mut tools = HashMap::new();
        tools.insert(
            "my_tool".to_string(),
            ToolDefinition {
                url: "http://example.com".to_string(),
                api_key: None,
                timeout_secs: 30,
                tool_source_id: Uuid::nil(),
            },
        );
        let mut metadata = HashMap::new();
        metadata.insert("my_tool".to_string(), (Some("Does a thing".to_string()), None));

        let ts = ResolvedToolSet::new(tools, metadata);
        let schemas = ts.to_openai_schemas();
        assert_eq!(schemas.len(), 1);
        assert_eq!(schemas[0]["type"], "function");
        assert_eq!(schemas[0]["function"]["name"], "my_tool");
        assert_eq!(schemas[0]["function"]["description"], "Does a thing");
    }
}
