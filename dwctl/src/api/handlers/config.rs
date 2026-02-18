//! HTTP handlers for configuration retrieval endpoints.

use axum::{Json, extract::State, response::IntoResponse};
use serde::Serialize;
use utoipa::ToSchema;

use crate::{
    AppState,
    api::models::{completion_window::format_completion_window, users::CurrentUser},
};

/// Batch processing configuration
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BatchConfigResponse {
    /// Whether batch processing is enabled on this instance
    pub enabled: bool,
    /// Available priority options in display format: "Standard (24h)", "High (1h)".
    /// Frontend should display these values as-is in dropdowns.
    pub allowed_completion_windows: Vec<String>,
}

/// Instance configuration and capabilities
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ConfigResponse {
    /// Cloud region identifier (e.g., "us-east-1"), if configured
    pub region: Option<String>,
    /// Organization name for this instance, if configured
    pub organization: Option<String>,
    /// Whether payment processing is enabled
    pub payment_enabled: bool,
    /// URL to JSONL documentation for batch file format, if available
    pub docs_jsonl_url: Option<String>,
    /// URL to the documentation site
    pub docs_url: String,
    /// Batch processing configuration, only present if batches are enabled
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batches: Option<BatchConfigResponse>,
    /// Base URL for AI API endpoints (files, batches, daemons)
    /// If not set, the frontend should use relative paths (same-origin requests)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ai_api_base_url: Option<String>,
}

#[utoipa::path(
    get,
    path = "/config",
    tag = "config",
    summary = "Get instance configuration",
    description = "Returns the current instance configuration including region, organization, \
        payment status, and batch processing capabilities. Use this endpoint to discover \
        what features are available on this Control Layer deployment.",
    responses(
        (status = 200, description = "Instance configuration", body = ConfigResponse),
        (status = 401, description = "Authentication required"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn get_config(State(state): State<AppState>, _user: CurrentUser) -> impl IntoResponse {
    let metadata = &state.config.metadata;

    let batches_config = if state.config.batches.enabled {
        // Format internal completion windows for display in UI
        let allowed_completion_windows = state
            .config
            .batches
            .allowed_completion_windows
            .iter()
            .map(|w| format_completion_window(w))
            .collect();

        Some(BatchConfigResponse {
            enabled: state.config.batches.enabled,
            allowed_completion_windows,
        })
    } else {
        None
    };

    let response = ConfigResponse {
        region: metadata.region.clone(),
        organization: metadata.organization.clone(),
        // Compute payment_enabled based on whether payment_processor is configured
        payment_enabled: state.config.payment.is_some(),
        docs_url: metadata.docs_url.clone(),
        docs_jsonl_url: metadata.docs_jsonl_url.clone(),
        batches: batches_config,
        ai_api_base_url: metadata.ai_api_base_url.clone(),
    };

    Json(response)
}

#[cfg(test)]
mod tests {
    use crate::api::models::users::Role;
    use crate::test::utils::{add_auth_headers, create_test_app, create_test_user};
    use axum::http::StatusCode;
    use serde_json::Value;
    use sqlx::PgPool;

    #[sqlx::test]
    async fn test_get_config_returns_metadata(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        let headers = add_auth_headers(&user);
        let response = app
            .get("/admin/api/v1/config")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .await;

        response.assert_status(StatusCode::OK);
        let json: Value = response.json();

        // Check that metadata fields are present
        assert!(json.get("region").is_some());
        assert!(json.get("organization").is_some());
    }

    #[sqlx::test]
    async fn test_get_config_requires_authentication(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;

        let response = app.get("/admin/api/v1/config").await;

        response.assert_status(StatusCode::UNAUTHORIZED);
    }

    #[sqlx::test]
    async fn test_get_config_includes_batch_slas(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        let headers = add_auth_headers(&user);
        let response = app
            .get("/admin/api/v1/config")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .await;

        response.assert_status(StatusCode::OK);
        let json: Value = response.json();

        // Check that batches config is present and includes allowed_completion_windows
        let batches = json.get("batches").expect("batches field should exist");
        assert_eq!(batches.get("enabled").and_then(|v| v.as_bool()), Some(true));

        let slas = batches
            .get("allowed_completion_windows")
            .and_then(|v| v.as_array())
            .expect("allowed_completion_windows should be an array");

        // Default config should have formatted "Standard (24h)" priority (converted from "24h")
        assert!(slas.iter().any(|v| v.as_str() == Some("Standard (24h)")));
    }
}
