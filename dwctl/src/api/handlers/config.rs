//! HTTP handlers for configuration retrieval endpoints.

use axum::{Json, extract::State, response::IntoResponse};
use serde::Serialize;
use utoipa::ToSchema;

use crate::{AppState, api::models::users::CurrentUser};

/// Batch processing configuration
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BatchConfigResponse {
    /// Whether batch processing is enabled on this instance
    pub enabled: bool,
    /// Available completion windows (e.g., "24h", "1h").
    pub allowed_completion_windows: Vec<String>,
    /// Allowed endpoint URL paths (e.g., "/v1/chat/completions", "/v1/responses").
    pub allowed_url_paths: Vec<String>,
}

/// Onwards AI proxy configuration
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct OnwardsConfigResponse {
    /// Whether strict mode is enabled (uses trusted flag, otherwise uses sanitize_responses)
    pub strict_mode: bool,
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
    /// Onwards AI proxy configuration
    pub onwards: OnwardsConfigResponse,
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
    let config = state.current_config();
    let metadata = &config.metadata;

    let batches_config = if config.batches.enabled {
        Some(BatchConfigResponse {
            enabled: config.batches.enabled,
            allowed_completion_windows: config.batches.allowed_completion_windows.clone(),
            allowed_url_paths: config.batches.allowed_url_paths.clone(),
        })
    } else {
        None
    };

    let response = ConfigResponse {
        region: metadata.region.clone(),
        organization: metadata.organization.clone(),
        // Compute payment_enabled based on whether payment_processor is configured
        payment_enabled: config.payment.is_some(),
        docs_url: metadata.docs_url.clone(),
        docs_jsonl_url: metadata.docs_jsonl_url.clone(),
        batches: batches_config,
        ai_api_base_url: metadata.ai_api_base_url.clone(),
        onwards: OnwardsConfigResponse {
            strict_mode: config.onwards.strict_mode,
        },
    };

    Json(response)
}

#[cfg(test)]
mod tests {
    use crate::Application;
    use crate::api::models::users::Role;
    use crate::test::utils::{add_auth_headers, create_test_app, create_test_user};
    use axum::http::StatusCode;
    use serde_json::Value;
    use sqlx::PgPool;
    use tempfile::tempdir;

    fn write_test_config(path: &std::path::Path, organization: &str, docs_url: &str) {
        let config = format!(
            r#"host: "127.0.0.1"
port: 0
dashboard_url: "http://localhost:3001"
database:
  type: external
  url: "postgres://ignored"
admin_email: "admin@test.com"
secret_key: "test-secret-key-for-testing-only"
model_sources: []
metadata:
  organization: "{organization}"
  docs_url: "{docs_url}"
auth:
  native:
    enabled: false
  proxy_header:
    enabled: true
batches:
  enabled: true
  allowed_completion_windows:
    - "24h"
  allowed_url_paths:
    - "/v1/chat/completions"
background_services:
  onwards_sync:
    enabled: false
  probe_scheduler:
    enabled: false
  batch_daemon:
    enabled: never
  leader_election:
    enabled: false
onwards:
  strict_mode: false
"#
        );

        std::fs::write(path, config).expect("failed to write test config");
    }

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

        // Default config should have "24h" completion window
        assert!(slas.iter().any(|v| v.as_str() == Some("24h")));

        let url_paths = batches
            .get("allowed_url_paths")
            .and_then(|v| v.as_array())
            .expect("allowed_url_paths should be an array");

        // Default config should include /v1/chat/completions
        assert!(url_paths.iter().any(|v| v.as_str() == Some("/v1/chat/completions")));
    }

    #[sqlx::test]
    async fn test_get_config_reflects_live_config_file_changes(pool: PgPool) {
        let tempdir = tempdir().expect("failed to create tempdir");
        let config_path = tempdir.path().join("config.yaml");
        write_test_config(&config_path, "Initial Org", "https://docs.example.com/initial");

        let config =
            crate::config::Config::load_from_path(config_path.to_string_lossy().into_owned()).expect("failed to load initial test config");
        let app = Application::new_with_pool_and_config_path(config, Some(config_path.clone()), Some(pool.clone()), None)
            .await
            .expect("failed to create application");
        let (server, bg_services) = app.into_test_server();

        let user = create_test_user(&pool, Role::StandardUser).await;
        let headers = add_auth_headers(&user);

        let initial_response = server
            .get("/admin/api/v1/config")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .await;
        initial_response.assert_status(StatusCode::OK);
        let initial_json: Value = initial_response.json();
        assert_eq!(initial_json.get("organization").and_then(|v| v.as_str()), Some("Initial Org"));
        assert_eq!(
            initial_json.get("docs_url").and_then(|v| v.as_str()),
            Some("https://docs.example.com/initial")
        );

        write_test_config(&config_path, "Updated Org", "https://docs.example.com/updated");

        let mut final_json = None;
        for _ in 0..50 {
            let response = server
                .get("/admin/api/v1/config")
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
                .await;
            response.assert_status(StatusCode::OK);
            let json: Value = response.json();

            if json.get("organization").and_then(|v| v.as_str()) == Some("Updated Org")
                && json.get("docs_url").and_then(|v| v.as_str()) == Some("https://docs.example.com/updated")
            {
                final_json = Some(json);
                break;
            }

            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        let final_json = final_json.expect("config endpoint never reflected updated config file");
        assert_eq!(final_json.get("organization").and_then(|v| v.as_str()), Some("Updated Org"));
        assert_eq!(
            final_json.get("docs_url").and_then(|v| v.as_str()),
            Some("https://docs.example.com/updated")
        );

        bg_services.shutdown().await;
    }
}
