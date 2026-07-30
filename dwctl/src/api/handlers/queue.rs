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
use utoipa::IntoParams;

use crate::api::handlers::sla_capacity::parse_window_to_seconds;

use crate::{
    AppState,
    auth::permissions::{RequiresPermission, operation, resource},
    errors::Error,
};

/// Nested map of pending request counts: model -> completion_window -> count
type PendingCountsByModelAndWindow = HashMap<String, HashMap<String, i64>>;

/// Query params for pending request counts.
#[derive(Debug, Default, Deserialize, IntoParams)]
pub struct PendingRequestCountsQuery {
    /// Comma-separated service tiers to include. Use `batch` for the null batch tier.
    /// Defaults to `batch`. Examples: `batch`, `batch,flex`, `flex`.
    pub service_tiers: Option<String>,
}

fn parse_service_tiers(raw: Option<&str>) -> Vec<Option<String>> {
    let mut tiers = Vec::new();

    if let Some(raw) = raw {
        for tier in raw.split(',').map(str::trim).filter(|tier| !tier.is_empty()) {
            if tier.eq_ignore_ascii_case("batch") || tier.eq_ignore_ascii_case("null") || tier.eq_ignore_ascii_case("none") {
                tiers.push(None);
            } else {
                tiers.push(Some(tier.to_ascii_lowercase()));
            }
        }
    }

    if tiers.is_empty() {
        tiers.push(None);
    }

    tiers
}

fn service_tiers_include_flex(tiers: &[Option<String>]) -> bool {
    tiers.iter().any(|tier| tier.as_deref() == Some("flex"))
}

/// Get pending, claimed, and processing request counts grouped by model and completion window
///
/// Returns a nested map showing how many active requests exist for each
/// model and completion window combination. By default it includes only the
/// batch tier (`service_tier IS NULL`). Pass `service_tiers` to include other
/// tiers, for example `service_tiers=batch,flex`. This always excludes:
/// - Escalated requests (racing duplicate requests)
/// - Requests without a template_id
/// - Requests in batches being cancelled
///
/// Background counts include the complete active backlog, including work for
/// models that are not currently live or dispatchable.
///
/// When `batches.priority_decay_window_secs` is configured, recently completed
/// flex requests are added back into the `1h` count for their model for that
/// many seconds only when `flex` is included in `service_tiers`.
///
/// Useful for monitoring queue depth and load distribution across models.
#[utoipa::path(
    get,
    path = "/admin/api/v1/monitoring/pending-request-counts",
    params(PendingRequestCountsQuery),
    responses(
        (status = 200, description = "Pending request counts by model and completion window", body = HashMap<String, HashMap<String, i64>>),
        (status = 500, description = "Internal server error"),
    ),
    tag = "monitoring",
)]
#[tracing::instrument(skip_all)]
pub async fn get_pending_request_counts<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Query(query): Query<PendingRequestCountsQuery>,
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
    let service_tiers = parse_service_tiers(query.service_tiers.as_deref());
    let priority_decay_window_secs = if service_tiers_include_flex(&service_tiers) {
        config.batches.priority_decay_window_secs
    } else {
        None
    };
    let service_tier_filter = ServiceTierFilter::Include(service_tiers);

    let counts = state
        .request_manager
        .get_pending_request_counts_by_model_and_window(
            &windows,
            &states,
            &model_filter,
            &service_tier_filter,
            priority_decay_window_secs,
            false,
        )
        .await
        .map_err(|e| Error::Internal {
            operation: format!("get pending request counts: {}", e),
        })?;

    Ok(Json(counts))
}

/// Strict duration parser for `/demand` window entries.
///
/// Unlike [`parse_window_to_seconds`] — which is forgiving on purpose for
/// the batch API (zero/negative/malformed input defaults to 24h) — this
/// returns `None` for anything malformed so the handler can reject the
/// request with 400. Zero is accepted; it's a meaningful lower bound
/// (`0s:1h` = "strictly future 0..1h").
fn parse_demand_duration(raw: &str) -> Option<i64> {
    let s = raw.trim();
    let (digits, mult): (&str, i64) = if let Some(d) = s.strip_suffix('h') {
        (d, 3600)
    } else if let Some(d) = s.strip_suffix('m') {
        (d, 60)
    } else {
        (s.strip_suffix('s')?, 1)
    };
    let n: i64 = digits.parse().ok()?;
    if n < 0 {
        return None;
    }
    n.checked_mul(mult)
}

/// Query parameters for the demand endpoint.
#[derive(Debug, Deserialize, IntoParams)]
pub struct DemandQuery {
    /// Comma-separated windows, each either `<end>` (shorthand for
    /// `0s:<end>`) or `<start>:<end>`. Both `start` and `end` are offsets
    /// from `now` and accept the same `<int><unit>` form as batch
    /// completion-window strings (`h`, `m`, `s`). Required.
    pub window: String,
    /// Comma-separated service tiers to include. Use `batch` for the null batch tier.
    /// Defaults to `batch`. Examples: `batch`, `batch,flex`, `flex`.
    pub service_tiers: Option<String>,
}

/// Parse one entry from the `window=` query list.
///
/// Returns `Ok(None)` for an empty (skipped) entry, `Ok(Some(...))` for a
/// valid entry, or `Err` for malformed input. Shorthand `<end>` returns
/// `start = None` (no lower bound, including overdue — matches the legacy
/// `<= now + N` behaviour of `/pending-request-counts`). Explicit
/// `<start>:<end>` returns `start = Some(...)` and enforces the lower bound
/// strictly; explicit ranges must satisfy `start < end`, so inverted or
/// empty ranges are rejected rather than silently returning zero counts.
/// The label is the caller's raw input so scouter can send
/// `window=1h,24h` and still match `"1h"` / `"24h"` keys on the response.
fn parse_demand_window(raw: &str) -> Result<Option<(String, Option<i64>, i64)>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let (start_secs, end_secs) = match trimmed.split_once(':') {
        Some((start, end)) => {
            let s = parse_demand_duration(start).ok_or_else(|| format!("malformed window start in {:?}", trimmed))?;
            let e = parse_demand_duration(end).ok_or_else(|| format!("malformed window end in {:?}", trimmed))?;
            if s >= e {
                return Err(format!("window start must be before end in {:?}", trimmed));
            }
            (Some(s), e)
        }
        None => {
            let e = parse_demand_duration(trimmed).ok_or_else(|| format!("malformed window {:?}", trimmed))?;
            (None, e)
        }
    };
    Ok(Some((trimmed.to_string(), start_secs, end_secs)))
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
/// Service-tier filtering matches `/pending-request-counts`: only the batch
/// tier by default, `service_tiers=batch,flex` to widen, and the
/// priority-decay top-up applies to the `1h` label when `flex` is included.
///
/// Excludes the same categories as `/pending-request-counts`: escalated
/// requests, requests without a template_id, and requests in batches being
/// cancelled.
#[utoipa::path(
    get,
    path = "/admin/api/v1/monitoring/demand",
    params(DemandQuery),
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
    let config = state.current_config();

    let windows: Vec<(String, Option<i64>, i64)> = params
        .window
        .split(',')
        .map(parse_demand_window)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|message| Error::BadRequest { message })?
        .into_iter()
        .flatten()
        .collect();

    if windows.is_empty() {
        return Err(Error::BadRequest {
            message: "window query parameter must list at least one window (e.g. `window=1h,24h` or `window=0s:1h,1h:24h`)".to_string(),
        });
    }

    let states = vec!["pending".to_string(), "claimed".to_string(), "processing".to_string()];
    let model_filter: Vec<String> = Vec::new();
    let service_tiers = parse_service_tiers(params.service_tiers.as_deref());
    let priority_decay_window_secs = if service_tiers_include_flex(&service_tiers) {
        config.batches.priority_decay_window_secs
    } else {
        None
    };
    let service_tier_filter = ServiceTierFilter::Include(service_tiers);

    let counts = state
        .request_manager
        .get_pending_request_counts_by_model_and_window(
            &windows,
            &states,
            &model_filter,
            &service_tier_filter,
            priority_decay_window_secs,
            false,
        )
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
    async fn test_pending_request_counts_defaults_to_batch_tier_only(pool: PgPool) {
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
        let request_manager = fusillade_arsenal::PostgresRequestManager::new(fusillade_pools, Default::default());

        // Create one batch per completion_window. The pending counts endpoint
        // partitions by service_tier; by default it should include only the
        // batch tier (`service_tier IS NULL`) and exclude flex/priority.
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
        // expire in 30 min so they fall inside it, but only the batch-tier row
        // should count by default.
        let model_counts = counts
            .get(model)
            .unwrap_or_else(|| panic!("expected '{model}' in response, got {counts:?}"));
        let count_24h = *model_counts.get("24h").unwrap_or(&0);
        assert_eq!(
            count_24h, 1,
            "expected only the batch-tier request within 24h; got {count_24h} ({model_counts:?})"
        );
    }

    #[sqlx::test]
    async fn test_pending_request_counts_service_tiers_query_includes_requested_tiers(pool: PgPool) {
        use fusillade::{BatchInput, RequestTemplateInput, Storage};
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
        let request_manager = fusillade_arsenal::PostgresRequestManager::new(fusillade_pools, Default::default());

        let model = "query-tier-model";
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
                .create_file(format!("queue-query-test-{completion_window}"), None, vec![template])
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

        for batch_id in &batch_ids {
            sqlx::query("UPDATE batches SET expires_at = NOW() + interval '30 minutes' WHERE id = $1")
                .bind(batch_id)
                .execute(&fusillade_pool)
                .await
                .expect("pin expires_at");
        }

        let response = server
            .get("/admin/api/v1/monitoring/pending-request-counts?service_tiers=batch,flex")
            .add_header(&add_auth_headers(&admin)[0].0, &add_auth_headers(&admin)[0].1)
            .add_header(&add_auth_headers(&admin)[1].0, &add_auth_headers(&admin)[1].1)
            .await;

        response.assert_status_ok();
        let counts: HashMap<String, HashMap<String, i64>> = response.json();
        let model_counts = counts
            .get(model)
            .unwrap_or_else(|| panic!("expected '{model}' in response, got {counts:?}"));

        assert_eq!(
            *model_counts.get("24h").unwrap_or(&0),
            2,
            "batch + flex should count when explicitly requested, while priority remains excluded"
        );
    }

    #[sqlx::test]
    async fn test_pending_request_counts_priority_decay_window_requires_flex_tier(pool: PgPool) {
        use fusillade::{CreateFlexInput, RequestId, Storage};
        use sqlx::postgres::PgConnectOptions;
        use sqlx_pool_router::TestDbPools;

        let mut config = create_test_config();
        config.batches.allowed_completion_windows = vec!["1h".to_string(), "24h".to_string()];
        config.batches.priority_decay_window_secs = Some(600);
        let (server, _bg): (TestServer, _) = create_test_app_with_config(pool.clone(), config, false).await;
        let admin = create_test_admin_user(&pool, Role::PlatformManager).await;

        let base_opts: PgConnectOptions = pool.connect_options().as_ref().clone();
        let fusillade_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .min_connections(0)
            .connect_with(base_opts.options([("search_path", "fusillade")]))
            .await
            .expect("Failed to create fusillade pool");
        let fusillade_pools = TestDbPools::new(fusillade_pool.clone()).await.expect("TestDbPools");
        let request_manager = fusillade_arsenal::PostgresRequestManager::new(fusillade_pools, Default::default());

        let model = "flex-decay-model";
        let recent_id = uuid::Uuid::new_v4();
        request_manager
            .create_flex(CreateFlexInput {
                request_id: recent_id,
                body: r#"{"input":"recent"}"#.to_string(),
                model: model.to_string(),
                endpoint: "http://localhost".to_string(),
                method: "POST".to_string(),
                path: "/v1/responses".to_string(),
                api_key: String::new(),
                created_by: "queue-user".to_string(),
            })
            .await
            .expect("create recent flex");
        mark_fusillade_request_processing(&fusillade_pool, recent_id)
            .await
            .expect("start recent flex");
        request_manager
            .complete_request(RequestId(recent_id), r#"{"output":"recent"}"#, 200)
            .await
            .expect("complete recent flex");

        let old_id = uuid::Uuid::new_v4();
        request_manager
            .create_flex(CreateFlexInput {
                request_id: old_id,
                body: r#"{"input":"old"}"#.to_string(),
                model: model.to_string(),
                endpoint: "http://localhost".to_string(),
                method: "POST".to_string(),
                path: "/v1/responses".to_string(),
                api_key: String::new(),
                created_by: "queue-user".to_string(),
            })
            .await
            .expect("create old flex");
        mark_fusillade_request_processing(&fusillade_pool, old_id)
            .await
            .expect("start old flex");
        request_manager
            .complete_request(RequestId(old_id), r#"{"output":"old"}"#, 200)
            .await
            .expect("complete old flex");

        let failed_id = uuid::Uuid::new_v4();
        request_manager
            .create_flex(CreateFlexInput {
                request_id: failed_id,
                body: r#"{"input":"failed"}"#.to_string(),
                model: model.to_string(),
                endpoint: "http://localhost".to_string(),
                method: "POST".to_string(),
                path: "/v1/responses".to_string(),
                api_key: String::new(),
                created_by: "queue-user".to_string(),
            })
            .await
            .expect("create failed flex");
        mark_fusillade_request_processing(&fusillade_pool, failed_id)
            .await
            .expect("start failed flex");
        request_manager
            .fail_request(RequestId(failed_id), r#"{"error":"failed"}"#, 500)
            .await
            .expect("fail flex");

        let canceled_id = uuid::Uuid::new_v4();
        request_manager
            .create_flex(CreateFlexInput {
                request_id: canceled_id,
                body: r#"{"input":"canceled"}"#.to_string(),
                model: model.to_string(),
                endpoint: "http://localhost".to_string(),
                method: "POST".to_string(),
                path: "/v1/responses".to_string(),
                api_key: String::new(),
                created_by: "queue-user".to_string(),
            })
            .await
            .expect("create canceled flex");

        sqlx::query("UPDATE requests SET completed_at = NOW() - INTERVAL '5 minutes' WHERE id = $1")
            .bind(recent_id)
            .execute(&fusillade_pool)
            .await
            .expect("age recent completion");
        sqlx::query("UPDATE requests SET completed_at = NOW() - INTERVAL '20 minutes' WHERE id = $1")
            .bind(old_id)
            .execute(&fusillade_pool)
            .await
            .expect("age old completion");
        sqlx::query("UPDATE requests SET failed_at = NOW() - INTERVAL '5 minutes' WHERE id = $1")
            .bind(failed_id)
            .execute(&fusillade_pool)
            .await
            .expect("age failed request");
        sqlx::query("UPDATE requests SET state = 'canceled', canceled_at = NOW() - INTERVAL '5 minutes' WHERE id = $1")
            .bind(canceled_id)
            .execute(&fusillade_pool)
            .await
            .expect("cancel request");

        let response = server
            .get("/admin/api/v1/monitoring/pending-request-counts")
            .add_header(&add_auth_headers(&admin)[0].0, &add_auth_headers(&admin)[0].1)
            .add_header(&add_auth_headers(&admin)[1].0, &add_auth_headers(&admin)[1].1)
            .await;
        response.assert_status_ok();
        let counts: HashMap<String, HashMap<String, i64>> = response.json();

        assert!(
            !counts.contains_key(model),
            "default batch-tier counts should not include completed flex decay"
        );

        let response = server
            .get("/admin/api/v1/monitoring/pending-request-counts?service_tiers=flex")
            .add_header(&add_auth_headers(&admin)[0].0, &add_auth_headers(&admin)[0].1)
            .add_header(&add_auth_headers(&admin)[1].0, &add_auth_headers(&admin)[1].1)
            .await;
        response.assert_status_ok();
        let counts: HashMap<String, HashMap<String, i64>> = response.json();
        let model_counts = counts
            .get(model)
            .unwrap_or_else(|| panic!("expected '{model}' in response, got {counts:?}"));
        assert_eq!(
            *model_counts.get("1h").unwrap_or(&0),
            1,
            "only completed flex requests within the 10 minute decay window should count"
        );
    }

    #[test]
    fn test_parse_service_tiers_defaults_to_batch_tier() {
        assert_eq!(parse_service_tiers(None), vec![None]);
        assert_eq!(parse_service_tiers(Some("")), vec![None]);
        assert_eq!(parse_service_tiers(Some("   ")), vec![None]);
    }

    #[test]
    fn test_parse_service_tiers_maps_batch_aliases_to_null_tier() {
        assert_eq!(parse_service_tiers(Some("batch")), vec![None]);
        assert_eq!(parse_service_tiers(Some("null,none")), vec![None, None]);
    }

    #[test]
    fn test_parse_service_tiers_parses_named_tiers() {
        assert_eq!(
            parse_service_tiers(Some("batch, flex, PRIORITY")),
            vec![None, Some("flex".to_string()), Some("priority".to_string())]
        );
        assert!(service_tiers_include_flex(&parse_service_tiers(Some("batch,flex"))));
        assert!(!service_tiers_include_flex(&parse_service_tiers(Some("batch,priority"))));
    }

    async fn mark_fusillade_request_processing(pool: &PgPool, id: uuid::Uuid) -> sqlx::Result<()> {
        sqlx::query(
            r#"
            UPDATE requests
            SET state = 'processing',
                daemon_id = gen_random_uuid(),
                claimed_at = NOW(),
                started_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(id)
        .execute(pool)
        .await?;

        Ok(())
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

    #[sqlx::test]
    async fn test_demand_accepts_zero_start(pool: PgPool) {
        // `0s:1h` must parse `0s` as zero seconds (not coerce to 24h like
        // the lenient batch-window parser does). Regression guard.
        let (server, _bg): (TestServer, _) = create_test_app(pool.clone(), false).await;
        let admin = create_test_admin_user(&pool, Role::PlatformManager).await;

        let response = server
            .get("/admin/api/v1/monitoring/demand?window=0s:1h")
            .add_header(&add_auth_headers(&admin)[0].0, &add_auth_headers(&admin)[0].1)
            .add_header(&add_auth_headers(&admin)[1].0, &add_auth_headers(&admin)[1].1)
            .await;
        response.assert_status_ok();
    }

    #[sqlx::test]
    async fn test_demand_accepts_service_tiers(pool: PgPool) {
        // service_tiers filtering matches /pending-request-counts semantics;
        // scouter sends `none,flex` today and must keep working via /demand.
        let (server, _bg): (TestServer, _) = create_test_app(pool.clone(), false).await;
        let admin = create_test_admin_user(&pool, Role::PlatformManager).await;

        let response = server
            .get("/admin/api/v1/monitoring/demand?window=1h,24h&service_tiers=none,flex")
            .add_header(&add_auth_headers(&admin)[0].0, &add_auth_headers(&admin)[0].1)
            .add_header(&add_auth_headers(&admin)[1].0, &add_auth_headers(&admin)[1].1)
            .await;
        response.assert_status_ok();
    }

    #[sqlx::test]
    async fn test_demand_rejects_malformed_window(pool: PgPool) {
        let (server, _bg): (TestServer, _) = create_test_app(pool.clone(), false).await;
        let admin = create_test_admin_user(&pool, Role::PlatformManager).await;

        for bad in [
            "window=foo",
            "window=1x",
            "window=1h,bad",
            "window=-1h:1h",
            "window=2h:1h",
            "window=1h:1h",
        ] {
            let response = server
                .get(&format!("/admin/api/v1/monitoring/demand?{}", bad))
                .add_header(&add_auth_headers(&admin)[0].0, &add_auth_headers(&admin)[0].1)
                .add_header(&add_auth_headers(&admin)[1].0, &add_auth_headers(&admin)[1].1)
                .await;
            response.assert_status(axum::http::StatusCode::BAD_REQUEST);
        }
    }
}
