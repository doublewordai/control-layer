//! Middleware for per-request tool resolution.
//!
//! This middleware runs on the `/ai/v1/*` routes (specifically `/v1/responses` once nested).
//! For each request it:
//!
//! 1. Extracts the API key secret from the `Authorization: Bearer` header.
//! 2. Peeks at the request body to extract the `model` field.
//! 3. Queries the database to resolve the *effective tool set* as the intersection of:
//!    - tools attached to the deployment matching the model alias (`deployment_tool_sources`)
//!    - tools attached to at least one group the user belongs to (`group_tool_sources`)
//! 4. Inserts the resolved tools into the request's `http::Extensions` as [`ResolvedTools`]
//!    so that onwards' `ToolExecutor::tools()` and `execute()` can access them via
//!    `RequestContext`.
//!
//! The middleware is a no-op for requests that carry no API key or target paths other
//! than `/v1/responses`.

use crate::tool_executor::{ResolvedToolSet, ResolvedTools, ToolDefinition};
use axum::{
    body::Body,
    extract::State,
    http::Request,
    middleware::Next,
    response::Response,
};
use serde_json::Value;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, warn};

/// Shared state threaded through the middleware.
#[derive(Clone)]
pub struct ToolInjectionState {
    pub db: PgPool,
}

/// Axum middleware function.
///
/// Resolves the effective tool set from the database and inserts it into the
/// request extensions as [`ResolvedTools`].
pub async fn tool_injection_middleware(
    State(state): State<ToolInjectionState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    // Only act on the Responses API path.
    if !is_responses_path(request.uri().path()) {
        return next.run(request).await;
    }

    // Extract bearer token from the Authorization header.
    let bearer_token = match extract_bearer_token(&request) {
        Some(t) => t,
        None => return next.run(request).await,
    };

    // Peek at the body to extract the model name for per-deployment resolution.
    let body_bytes = match axum::body::to_bytes(std::mem::take(request.body_mut()), usize::MAX).await {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, "Failed to read request body in tool injection middleware");
            return next.run(request).await;
        }
    };

    let model_alias = onwards::extract_model_from_request(request.headers(), &body_bytes);

    // Restore the body before proceeding.
    *request.body_mut() = Body::from(body_bytes);

    // Resolve deployment and group tool sets from the DB.
    match resolve_tools_for_request(&state.db, &bearer_token, model_alias.as_deref()).await {
        Ok(Some(resolved)) if !resolved.is_empty() => {
            debug!(
                tool_count = resolved.tools.len(),
                "Resolved server-side tools for request"
            );
            request
                .extensions_mut()
                .insert(ResolvedTools(Arc::new(resolved)));
        }
        Ok(_) => {
            debug!("No server-side tools for this request");
        }
        Err(e) => {
            warn!(error = %e, "Failed to resolve tool sources; proceeding without tools");
        }
    }

    next.run(request).await
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check whether the request path targets the Responses API.
fn is_responses_path(path: &str) -> bool {
    path.ends_with("/responses")
}

/// Extract the Bearer token from the request, case-insensitive.
fn extract_bearer_token(request: &Request<Body>) -> Option<String> {
    let auth = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let auth = auth.trim();
    if auth.len() > 7 && auth[..7].eq_ignore_ascii_case("bearer ") {
        Some(auth[7..].to_string())
    } else {
        None
    }
}

/// Resolve the effective tool set for a request identified by its API key secret and model alias.
///
/// Returns `None` if no tools are configured for this deployment/group combination.
async fn resolve_tools_for_request(
    db: &PgPool,
    bearer_token: &str,
    model_alias: Option<&str>,
) -> anyhow::Result<Option<ResolvedToolSet>> {
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
        INNER JOIN user_groups ug ON ug.user_id = ak.user_id
        INNER JOIN deployment_groups dg ON dg.group_id = ug.group_id
        INNER JOIN deployed_models dm ON dm.id = dg.deployment_id
        INNER JOIN deployment_tool_sources dts ON dts.deployment_id = dg.deployment_id
        INNER JOIN group_tool_sources gts ON gts.tool_source_id = dts.tool_source_id AND gts.group_id = ug.group_id
        INNER JOIN tool_sources ts ON ts.id = dts.tool_source_id
        WHERE ak.secret = $1
          AND ak.is_deleted = FALSE
          AND ($2::TEXT IS NULL OR dm.alias = $2)
        ORDER BY ts.name
        "#,
        bearer_token,
        model_alias,
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_responses_path() {
        assert!(is_responses_path("/v1/responses"));
        assert!(is_responses_path("/ai/v1/responses"));
        assert!(!is_responses_path("/v1/responses/resp_abc123"));
        assert!(!is_responses_path("/v1/chat/completions"));
        assert!(!is_responses_path("/v1/responsesX"));
    }

    fn make_request_with_auth(auth_value: &str) -> Request<Body> {
        Request::builder()
            .header(axum::http::header::AUTHORIZATION, auth_value)
            .body(Body::empty())
            .unwrap()
    }

    #[test]
    fn test_extract_bearer_token_standard() {
        let req = make_request_with_auth("Bearer sk-abc123");
        assert_eq!(extract_bearer_token(&req), Some("sk-abc123".to_string()));
    }

    #[test]
    fn test_extract_bearer_token_lowercase() {
        let req = make_request_with_auth("bearer sk-abc123");
        assert_eq!(extract_bearer_token(&req), Some("sk-abc123".to_string()));
    }

    #[test]
    fn test_extract_bearer_token_mixed_case() {
        let req = make_request_with_auth("BEARER sk-abc123");
        assert_eq!(extract_bearer_token(&req), Some("sk-abc123".to_string()));
    }

    #[test]
    fn test_extract_bearer_token_missing() {
        let req = Request::builder().body(Body::empty()).unwrap();
        assert_eq!(extract_bearer_token(&req), None);
    }

    #[test]
    fn test_extract_bearer_token_no_prefix() {
        let req = make_request_with_auth("sk-abc123");
        assert_eq!(extract_bearer_token(&req), None);
    }
}
