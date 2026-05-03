//! Queue monitoring handlers
//!
//! Endpoints for querying queue depth and pending request metrics from fusillade.

use axum::{
    extract::{Query, State},
    response::Json,
};
use fusillade::Storage;
use fusillade::request::ServiceTierFilter;
use serde::Deserialize;
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

/// Query parameters accepted by `get_pending_request_counts`.
#[derive(Debug, Deserialize, Default)]
pub struct PendingCountsQuery {
    /// When true, apply the per-user fair-share policy described in
    /// `fusillade::Storage::get_effective_pending_request_counts_by_model_and_window`.
    /// When false or omitted, return raw pending counts.
    #[serde(default)]
    pub fairness: bool,
}

/// Get pending, claimed, and processing request counts grouped by model and completion window
///
/// Returns a nested map showing how many pending requests are queued for each
/// model and completion window combination. This excludes:
/// - Escalated requests (racing duplicate requests)
/// - Requests without a template_id
/// - Requests in batches being cancelled
/// - Realtime requests (`service_tier = 'priority'`), which are managed
///   externally and so should not appear in queue depth.
///
/// **Query parameters:**
/// - `fairness` (bool, default `false`): when `true`, returns *effective*
///   counts where each user's contribution to a (model, window) bucket is
///   capped at that bucket's average pending count. See the trait method
///   `get_effective_pending_request_counts_by_model_and_window` for the
///   exact policy. Use this mode when the consumer needs a depth signal
///   that reflects per-user fair scheduling rather than raw queue length.
#[utoipa::path(
    get,
    path = "/admin/api/v1/monitoring/pending-request-counts",
    params(
        ("fairness" = Option<bool>, Query, description = "Apply per-user fair-share cap")
    ),
    responses(
        (status = 200, description = "Pending request counts by model and completion window", body = HashMap<String, HashMap<String, i64>>),
        (status = 500, description = "Internal server error"),
    ),
    tag = "monitoring",
)]
#[tracing::instrument(skip_all, fields(fairness = q.fairness))]
pub async fn get_pending_request_counts<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Query(q): Query<PendingCountsQuery>,
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

    let counts = if q.fairness {
        state
            .request_manager
            .get_effective_pending_request_counts_by_model_and_window(&windows, &states, &model_filter, &service_tier_filter, false)
            .await
    } else {
        state
            .request_manager
            .get_pending_request_counts_by_model_and_window(&windows, &states, &model_filter, &service_tier_filter, false)
            .await
    }
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
        use fusillade::{CreateSingleRequestBatchInput, Storage};
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

        // Create three single-request batches for the same model with
        // different completion windows. fusillade derives service_tier from
        // completion_window:
        //   "24h" → service_tier IS NULL (batch tier — counted)
        //   "1h"  → service_tier = 'flex'   (counted)
        //   "0s"  → service_tier = 'priority' (EXCLUDED — managed externally)
        let model = "test-model";
        let mut batch_ids = Vec::new();
        for completion_window in ["24h", "1h", "0s"] {
            let batch_id = uuid::Uuid::new_v4();
            request_manager
                .create_single_request_batch(CreateSingleRequestBatchInput {
                    batch_id: Some(batch_id),
                    request_id: uuid::Uuid::new_v4(),
                    body: r#"{"input":"x"}"#.to_string(),
                    model: model.to_string(),
                    base_url: "http://localhost".to_string(),
                    endpoint: "/v1/chat/completions".to_string(),
                    completion_window: completion_window.to_string(),
                    initial_state: "pending".to_string(),
                    api_key: None,
                    created_by: None,
                })
                .await
                .expect("create single-request batch");
            batch_ids.push(batch_id);
        }

        // Pin all expires_at into the configured 24h window so the deadline
        // predicate matches deterministically regardless of the original
        // completion_window.
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

        // The default test config queries the "24h" window only. All three
        // batches expire in 30 min so they all fall inside it. Priority must
        // be excluded; batch + flex remain.
        let model_counts = counts
            .get(model)
            .unwrap_or_else(|| panic!("expected '{model}' in response, got {counts:?}"));
        let count_24h = *model_counts.get("24h").unwrap_or(&0);
        assert_eq!(
            count_24h, 2,
            "expected 2 (batch + flex) within 24h window — priority must be excluded; got {count_24h} ({model_counts:?})"
        );
    }

    #[sqlx::test]
    async fn test_pending_request_counts_fairness_caps_dominant_user(pool: PgPool) {
        use fusillade::{CreateSingleRequestBatchInput, Storage};
        use sqlx::postgres::PgConnectOptions;
        use sqlx_pool_router::TestDbPools;

        let (server, _bg): (TestServer, _) = create_test_app(pool.clone(), false).await;
        let admin = create_test_admin_user(&pool, Role::PlatformManager).await;

        let base_opts: PgConnectOptions = pool.connect_options().as_ref().clone();
        let fusillade_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .min_connections(0)
            .connect_with(base_opts.options([("search_path", "fusillade")]))
            .await
            .expect("Failed to create fusillade pool");
        let fusillade_pools = TestDbPools::new(fusillade_pool.clone()).await.expect("TestDbPools");
        let request_manager = fusillade::PostgresRequestManager::new(fusillade_pools, Default::default());

        // user-a: 6 pending; user-b: 2 pending; one model.
        async fn seed(
            mgr: &fusillade::PostgresRequestManager<sqlx_pool_router::TestDbPools, fusillade::http::ReqwestHttpClient>,
            user: &str,
            n: usize,
        ) -> Vec<uuid::Uuid> {
            let mut ids = Vec::new();
            for _ in 0..n {
                let batch_id = uuid::Uuid::new_v4();
                mgr.create_single_request_batch(CreateSingleRequestBatchInput {
                    batch_id: Some(batch_id),
                    request_id: uuid::Uuid::new_v4(),
                    body: r#"{"input":"x"}"#.to_string(),
                    model: "model-x".to_string(),
                    base_url: "http://localhost".to_string(),
                    endpoint: "/v1/chat/completions".to_string(),
                    completion_window: "24h".to_string(),
                    initial_state: "pending".to_string(),
                    api_key: None,
                    created_by: Some(user.to_string()),
                })
                .await
                .expect("create batch");
                ids.push(batch_id);
            }
            ids
        }

        let mut all = seed(&request_manager, "user-a", 6).await;
        all.extend(seed(&request_manager, "user-b", 2).await);
        // Pin expires_at into the 24h window
        for batch_id in &all {
            sqlx::query("UPDATE batches SET expires_at = NOW() + interval '30 minutes' WHERE id = $1")
                .bind(batch_id)
                .execute(&fusillade_pool)
                .await
                .expect("pin expires_at");
        }

        // Raw: total = 8
        let resp = server
            .get("/admin/api/v1/monitoring/pending-request-counts")
            .add_header(&add_auth_headers(&admin)[0].0, &add_auth_headers(&admin)[0].1)
            .add_header(&add_auth_headers(&admin)[1].0, &add_auth_headers(&admin)[1].1)
            .await;
        resp.assert_status_ok();
        let raw: HashMap<String, HashMap<String, i64>> = resp.json();
        assert_eq!(*raw.get("model-x").unwrap().get("24h").unwrap(), 8);

        // Fairness: fair_share=ceil(8/2)=4 → effective = 4 + 2 = 6
        let resp = server
            .get("/admin/api/v1/monitoring/pending-request-counts?fairness=true")
            .add_header(&add_auth_headers(&admin)[0].0, &add_auth_headers(&admin)[0].1)
            .add_header(&add_auth_headers(&admin)[1].0, &add_auth_headers(&admin)[1].1)
            .await;
        resp.assert_status_ok();
        let eff: HashMap<String, HashMap<String, i64>> = resp.json();
        assert_eq!(*eff.get("model-x").unwrap().get("24h").unwrap(), 6);
    }
}
