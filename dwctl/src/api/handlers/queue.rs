//! Queue monitoring handlers
//!
//! Endpoints for querying queue depth and pending request metrics from fusillade.

use axum::{
    extract::{Query, State},
    response::Json,
};
use fusillade::Storage;
use serde::Deserialize;
use sqlx_pool_router::PoolProvider;
use std::collections::HashMap;

use crate::api::handlers::sla_capacity::parse_window_to_seconds;

use crate::{
    AppState,
    auth::permissions::{RequiresPermission, operation, resource},
    errors::Error,
};

/// Nested map of pending request counts: model -> window_label -> count
type PendingCountsByModelAndWindow = HashMap<String, HashMap<String, i64>>;

/// Query parameters for the demand endpoint.
#[derive(Debug, Deserialize)]
pub struct DemandQuery {
    /// Comma-separated windows, each either `<end>` (shorthand for
    /// `0s:<end>`) or `<start>:<end>`. Both `start` and `end` are offsets
    /// from `now` and accept the same `<int><unit>` form as batch
    /// completion-window strings (`h`, `m`, `s`). Required.
    pub window: String,
}

/// Parse one entry from the `window=` query list.
///
/// Returns `(label, start_secs, end_secs)`. The label is the caller's raw
/// input so scouter can send `window=1h,24h` and still match `"1h"` /
/// `"24h"` keys on the response.
fn parse_demand_window(raw: &str) -> Option<(String, i64, i64)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (start_secs, end_secs) = match trimmed.split_once(':') {
        Some((start, end)) => (parse_window_to_seconds(start), parse_window_to_seconds(end)),
        None => (0, parse_window_to_seconds(trimmed)),
    };
    Some((trimmed.to_string(), start_secs, end_secs))
}

/// Get pending, claimed, and processing request counts grouped by model and completion window
///
/// Returns a nested map showing how many pending requests are queued for each
/// model and completion window combination. This excludes:
/// - Escalated requests (racing duplicate requests)
/// - Requests without a template_id
/// - Requests in batches being cancelled
///
/// Useful for monitoring queue depth and load distribution across models.
#[utoipa::path(
    get,
    path = "/admin/api/v1/monitoring/pending-request-counts",
    responses(
        (status = 200, description = "Pending request counts by model and completion window", body = HashMap<String, HashMap<String, i64>>),
        (status = 500, description = "Internal server error"),
    ),
    tag = "monitoring",
)]
#[tracing::instrument(skip_all)]
pub async fn get_pending_request_counts<P: PoolProvider>(
    State(state): State<AppState<P>>,
    _: RequiresPermission<resource::System, operation::ReadAll>,
) -> Result<Json<PendingCountsByModelAndWindow>, Error> {
    let config = state.current_config();

    // Call fusillade storage API to get pending request counts
    let windows = config
        .batches
        .allowed_completion_windows
        .iter()
        .map(|window| (window.clone(), 0i64, parse_window_to_seconds(window)))
        .collect::<Vec<_>>();
    let states = vec!["pending".to_string(), "claimed".to_string(), "processing".to_string()]; // Include claimed and processing to get a more complete picture of queue depth
    let model_filter: Vec<String> = Vec::new();

    let counts = state
        .request_manager
        .get_pending_request_counts_by_model_and_window(&windows, &states, &model_filter, false)
        .await
        .map_err(|e| Error::Internal {
            operation: format!("get pending request counts: {}", e),
        })?;

    Ok(Json(counts))
}

/// Get pending request demand bucketed by deadline window.
///
/// Returns, per model, counts of pending/claimed/processing requests whose
/// deadline (`submitted_at + completion_window`) falls within each
/// caller-specified window. Each window is either `<end>` (shorthand for
/// `0s:<end>`, matching the legacy "due within N" semantic) or
/// `<start>:<end>` for a disjoint range. Both bounds are offsets from
/// `now`.
///
/// Windows can overlap or be disjoint — the caller chooses. This endpoint
/// is deliberately decoupled from `config.batches.allowed_completion_windows`
/// so replica-allocation consumers can pick the lookahead shape they care
/// about independently of whatever completion-window SLAs the batch API
/// exposes to users.
///
/// Excludes the same categories as `/pending-request-counts`: escalated
/// requests, requests without a template_id, and requests in batches being
/// cancelled.
#[utoipa::path(
    get,
    path = "/admin/api/v1/monitoring/demand",
    params(
        (
            "window" = String,
            Query,
            description = "Comma-separated windows, e.g. `1h,24h` (cumulative) or `0s:1h,1h:24h` (disjoint)",
        ),
    ),
    responses(
        (status = 200, description = "Pending request counts by model and window", body = HashMap<String, HashMap<String, i64>>),
        (status = 400, description = "Missing or malformed window parameter"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "monitoring",
)]
#[tracing::instrument(skip_all)]
pub async fn get_demand<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Query(params): Query<DemandQuery>,
    _: RequiresPermission<resource::System, operation::ReadAll>,
) -> Result<Json<PendingCountsByModelAndWindow>, Error> {
    let windows: Vec<(String, i64, i64)> = params.window.split(',').filter_map(parse_demand_window).collect();

    if windows.is_empty() {
        return Err(Error::BadRequest {
            message: "window query parameter must list at least one window (e.g. `window=1h,24h` or `window=0s:1h,1h:24h`)".to_string(),
        });
    }

    let states = vec!["pending".to_string(), "claimed".to_string(), "processing".to_string()];
    let model_filter: Vec<String> = Vec::new();

    let counts = state
        .request_manager
        .get_pending_request_counts_by_model_and_window(&windows, &states, &model_filter, false)
        .await
        .map_err(|e| Error::Internal {
            operation: format!("get demand by window: {}", e),
        })?;

    Ok(Json(counts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::users::Role;
    use crate::test::utils::*;
    use axum_test::TestServer;
    use sqlx::PgPool;

    #[sqlx::test]
    async fn test_pending_request_counts_requires_system_permission(pool: sqlx::PgPool) {
        let (server, _bg): (TestServer, _) = create_test_app(pool.clone(), false).await;

        // StandardUser should NOT have System::ReadAll permission
        let standard_user = create_test_user(&pool, Role::StandardUser).await;
        let response = server
            .get("/admin/api/v1/monitoring/pending-request-counts")
            .add_header(&add_auth_headers(&standard_user)[0].0, &add_auth_headers(&standard_user)[0].1)
            .add_header(&add_auth_headers(&standard_user)[1].0, &add_auth_headers(&standard_user)[1].1)
            .await;
        response.assert_status(axum::http::StatusCode::FORBIDDEN);

        // PlatformManager should have System::ReadAll permission
        let platform_manager = create_test_user(&pool, Role::PlatformManager).await;
        let response = server
            .get("/admin/api/v1/monitoring/pending-request-counts")
            .add_header(&add_auth_headers(&platform_manager)[0].0, &add_auth_headers(&platform_manager)[0].1)
            .add_header(&add_auth_headers(&platform_manager)[1].0, &add_auth_headers(&platform_manager)[1].1)
            .await;
        response.assert_status_ok();
    }

    #[sqlx::test]
    async fn test_pending_request_counts_returns_empty_when_no_requests(pool: PgPool) {
        let (server, _bg): (TestServer, _) = create_test_app(pool.clone(), false).await;
        let admin = create_test_admin_user(&pool, Role::PlatformManager).await;

        // Query the endpoint
        let response = server
            .get("/admin/api/v1/monitoring/pending-request-counts")
            .add_header(&add_auth_headers(&admin)[0].0, &add_auth_headers(&admin)[0].1)
            .add_header(&add_auth_headers(&admin)[1].0, &add_auth_headers(&admin)[1].1)
            .await;

        response.assert_status_ok();
        let counts: HashMap<String, HashMap<String, i64>> = response.json();

        // Should be empty when no requests exist
        assert_eq!(counts.len(), 0, "Should have no pending requests");
    }

    #[sqlx::test]
    async fn test_demand_requires_system_permission(pool: sqlx::PgPool) {
        let (server, _bg): (TestServer, _) = create_test_app(pool.clone(), false).await;

        let standard_user = create_test_user(&pool, Role::StandardUser).await;
        let response = server
            .get("/admin/api/v1/monitoring/demand?window=1h,24h")
            .add_header(&add_auth_headers(&standard_user)[0].0, &add_auth_headers(&standard_user)[0].1)
            .add_header(&add_auth_headers(&standard_user)[1].0, &add_auth_headers(&standard_user)[1].1)
            .await;
        response.assert_status(axum::http::StatusCode::FORBIDDEN);

        let platform_manager = create_test_user(&pool, Role::PlatformManager).await;
        let response = server
            .get("/admin/api/v1/monitoring/demand?window=1h,24h")
            .add_header(&add_auth_headers(&platform_manager)[0].0, &add_auth_headers(&platform_manager)[0].1)
            .add_header(&add_auth_headers(&platform_manager)[1].0, &add_auth_headers(&platform_manager)[1].1)
            .await;
        response.assert_status_ok();
    }

    #[sqlx::test]
    async fn test_demand_rejects_missing_window(pool: PgPool) {
        let (server, _bg): (TestServer, _) = create_test_app(pool.clone(), false).await;
        let admin = create_test_admin_user(&pool, Role::PlatformManager).await;

        let response = server
            .get("/admin/api/v1/monitoring/demand")
            .add_header(&add_auth_headers(&admin)[0].0, &add_auth_headers(&admin)[0].1)
            .add_header(&add_auth_headers(&admin)[1].0, &add_auth_headers(&admin)[1].1)
            .await;
        response.assert_status(axum::http::StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn test_demand_rejects_empty_window(pool: PgPool) {
        let (server, _bg): (TestServer, _) = create_test_app(pool.clone(), false).await;
        let admin = create_test_admin_user(&pool, Role::PlatformManager).await;

        let response = server
            .get("/admin/api/v1/monitoring/demand?window=")
            .add_header(&add_auth_headers(&admin)[0].0, &add_auth_headers(&admin)[0].1)
            .add_header(&add_auth_headers(&admin)[1].0, &add_auth_headers(&admin)[1].1)
            .await;
        response.assert_status(axum::http::StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn test_demand_accepts_arbitrary_windows(pool: PgPool) {
        // Caller-supplied windows don't need to match
        // config.batches.allowed_completion_windows — the point of this
        // endpoint is to decouple the two. Mixing cumulative (`2h`) and
        // disjoint (`1h:72h`) shapes should work in the same request.
        let (server, _bg): (TestServer, _) = create_test_app(pool.clone(), false).await;
        let admin = create_test_admin_user(&pool, Role::PlatformManager).await;

        let response = server
            .get("/admin/api/v1/monitoring/demand?window=15m,2h,1h:72h")
            .add_header(&add_auth_headers(&admin)[0].0, &add_auth_headers(&admin)[0].1)
            .add_header(&add_auth_headers(&admin)[1].0, &add_auth_headers(&admin)[1].1)
            .await;
        response.assert_status_ok();
        let counts: HashMap<String, HashMap<String, i64>> = response.json();
        assert_eq!(counts.len(), 0, "no pending requests exist in a clean database");
    }
}
