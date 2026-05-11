//! Queue monitoring handlers
//!
//! Endpoints for querying queue depth and pending request metrics from fusillade.

use axum::{extract::State, response::Json};
use fusillade::Storage;
use fusillade::request::ServiceTierFilter;
use sqlx_pool_router::PoolProvider;
use std::collections::HashMap;

use crate::api::handlers::sla_capacity::parse_window_to_seconds;

use crate::{
    AppState,
    auth::permissions::{RequiresPermission, operation, resource},
    errors::Error,
};

/// Nested map of pending request counts: model -> completion_window -> count
type PendingCountsByModelAndWindow = HashMap<String, HashMap<String, i64>>;

/// Get pending, claimed, and processing request counts grouped by model and completion window
///
/// Returns a nested map showing how many pending requests are queued for each
/// model and completion window combination. This excludes:
/// - Escalated requests (racing duplicate requests)
/// - Requests without a template_id
/// - Requests in batches being cancelled
/// - Realtime requests (`service_tier = 'priority'`), which are managed
///   externally rather than by the fusillade daemon and so should not drive
///   GPU scheduling decisions.
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
        .map(|window| (window.clone(), None, parse_window_to_seconds(window)))
        .collect::<Vec<_>>();
    let states = vec!["pending".to_string(), "claimed".to_string(), "processing".to_string()]; // Include claimed and processing to get a more complete picture of queue depth
    let model_filter: Vec<String> = Vec::new();
    let service_tier_filter = ServiceTierFilter::Exclude(vec![Some("priority".to_string())]);

    let counts = state
        .request_manager
        .get_pending_request_counts_by_model_and_window(&windows, &states, &model_filter, &service_tier_filter, false)
        .await
        .map_err(|e| Error::Internal {
            operation: format!("get pending request counts: {}", e),
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
    async fn test_pending_request_counts_excludes_priority_service_tier(pool: PgPool) {
        use fusillade::{BatchInput, RequestTemplateInput, Storage};
        use sqlx::postgres::PgConnectOptions;
        use sqlx_pool_router::TestDbPools;

        let (server, _bg): (TestServer, _) = create_test_app(pool.clone(), false).await;
        let admin = create_test_admin_user(&pool, Role::PlatformManager).await;

        // Connect a request_manager to the same `fusillade` schema the app
        // uses. Migrations are already run by the app's setup_database.
        let base_opts: PgConnectOptions = pool.connect_options().as_ref().clone();
        let fusillade_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .min_connections(0)
            .connect_with(base_opts.options([("search_path", "fusillade")]))
            .await
            .expect("Failed to create fusillade pool");
        let fusillade_pools = TestDbPools::new(fusillade_pool.clone()).await.expect("TestDbPools");
        let request_manager = fusillade::PostgresRequestManager::new(fusillade_pools, Default::default());

        // Create one batch per completion_window. The pending counts endpoint
        // partitions by service_tier; we expect:
        //   "24h" → service_tier IS NULL (batch tier — counted)
        //   "1h"  → service_tier = 'flex'   (counted)
        // "priority" (0s) is exercised separately via create_realtime; the
        // handler's ServiceTierFilter::Exclude(["priority"]) is what drops
        // those rows from the count — not their state (processing is included
        // in the queried states alongside pending and claimed).
        let model = "test-model";
        let mut batch_ids = Vec::new();
        for completion_window in ["24h", "1h"] {
            let template = RequestTemplateInput {
                custom_id: None,
                endpoint: "https://api.example.com".to_string(),
                method: "POST".to_string(),
                path: "/v1/chat/completions".to_string(),
                body: r#"{"input":"x"}"#.to_string(),
                model: model.to_string(),
                api_key: "key".to_string(),
            };
            let file_id = request_manager
                .create_file(format!("queue-test-{completion_window}"), None, vec![template])
                .await
                .expect("create_file");
            let batch = request_manager
                .create_batch(BatchInput {
                    file_id,
                    endpoint: "/v1/chat/completions".to_string(),
                    completion_window: completion_window.to_string(),
                    metadata: None,
                    created_by: None,
                    api_key_id: None,
                    api_key: None,
                    total_requests: None,
                })
                .await
                .expect("create_batch");
            batch_ids.push(batch.id.0);
        }

        // Realtime row in 'processing' — the priority tier shouldn't count
        // as pending.
        request_manager
            .create_realtime(fusillade::CreateRealtimeInput {
                request_id: uuid::Uuid::new_v4(),
                body: r#"{"input":"x"}"#.to_string(),
                model: model.to_string(),
                endpoint: "http://localhost".to_string(),
                method: "POST".to_string(),
                path: "/v1/responses".to_string(),
                api_key: String::new(),
                created_by: "queue-user".to_string(),
            })
            .await
            .expect("create_realtime");

        // Pin all batch expires_at into the configured 24h window so the
        // deadline predicate matches deterministically.
        for batch_id in &batch_ids {
            sqlx::query("UPDATE batches SET expires_at = NOW() + interval '30 minutes' WHERE id = $1")
                .bind(batch_id)
                .execute(&fusillade_pool)
                .await
                .expect("pin expires_at");
        }

        let response = server
            .get("/admin/api/v1/monitoring/pending-request-counts")
            .add_header(&add_auth_headers(&admin)[0].0, &add_auth_headers(&admin)[0].1)
            .add_header(&add_auth_headers(&admin)[1].0, &add_auth_headers(&admin)[1].1)
            .await;

        response.assert_status_ok();
        let counts: HashMap<String, HashMap<String, i64>> = response.json();

        // The default test config queries the "24h" window only. Both batches
        // expire in 30 min so they fall inside it. Priority must be excluded;
        // batch + flex remain.
        let model_counts = counts
            .get(model)
            .unwrap_or_else(|| panic!("expected '{model}' in response, got {counts:?}"));
        let count_24h = *model_counts.get("24h").unwrap_or(&0);
        assert_eq!(
            count_24h, 2,
            "expected 2 (batch + flex) within 24h window — priority must be excluded; got {count_24h} ({model_counts:?})"
        );
    }
}
