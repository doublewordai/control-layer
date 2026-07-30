//! Queue monitoring handlers
//!
//! Endpoints for querying queue depth and pending request metrics from fusillade.

use axum::{
    extract::{Query, State},
    response::Json,
};
use fusillade::request::ServiceTierFilter;
use fusillade::{Storage, TrailingDemandCount};
use serde::{Deserialize, Serialize};
use sqlx_pool_router::PoolProvider;
use std::collections::HashMap;
use utoipa::{IntoParams, ToSchema};

use crate::{
    AppState,
    auth::permissions::{RequiresPermission, operation, resource},
    errors::Error,
};

/// Nested map of pending request counts: model -> completion_window -> count
type PendingCountsByModelAndWindow = HashMap<String, HashMap<String, i64>>;

/// Demand cube: model -> window -> service tier -> outcome -> count.
///
/// Tier keys are `batch` (the NULL tier), `flex`, `priority`, …; outcome keys
/// are `pending` (future windows: the pending/claimed/processing states,
/// collapsed) and `completed` / `failed` (trailing windows).
type GroupedDemandByModelAndWindow = HashMap<String, HashMap<String, HashMap<String, HashMap<String, i64>>>>;

/// Response body for [`get_demand`]: the flat shape by default, the cube when
/// `group_by=service_tier` is requested.
#[derive(Debug, Serialize, ToSchema)]
#[serde(untagged)]
pub enum DemandResponse {
    /// `model -> window -> count` (the default shape).
    Flat(PendingCountsByModelAndWindow),
    /// `model -> window -> tier -> outcome -> count` (`group_by=service_tier`).
    Grouped(GroupedDemandByModelAndWindow),
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

/// Strict duration parser for `/demand` window entries.
///
/// Unlike [`crate::api::handlers::sla_capacity::parse_window_to_seconds`] —
/// which is forgiving on purpose for
/// the batch API (zero/negative/malformed input defaults to 24h) — this
/// returns `None` for anything malformed so the handler can reject the
/// request with 400. Zero is accepted; it's a meaningful lower bound
/// (`0s:1h` = "strictly future 0..1h"). Negative durations are offsets into
/// the past (`-1h` = an hour ago).
fn parse_demand_duration(raw: &str) -> Option<i64> {
    let s = raw.trim();
    let (s, sign): (&str, i64) = match s.strip_prefix('-') {
        Some(rest) => (rest, -1),
        None => (s, 1),
    };
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
    n.checked_mul(mult)?.checked_mul(sign)
}

/// Query parameters for the demand endpoint.
#[derive(Debug, Deserialize, IntoParams)]
pub struct DemandQuery {
    /// Comma-separated windows, each either `<end>` or `<start>:<end>`.
    /// Bare positive `<end>` means "due by `now + end`" with no lower bound
    /// (overdue included); bare negative `<end>` is shorthand for
    /// `<end>:0s`. Both `start` and `end` are signed offsets from `now` and
    /// accept the same `<int><unit>` form as batch completion-window
    /// strings (`h`, `m`, `s`); negative offsets reach into the past
    /// (`-1h` = the trailing hour, `-1h:1h` spans now). Required.
    pub window: String,
    /// Comma-separated service tiers to include. Use `batch` for the null batch tier;
    /// `priority` is the realtime tier. Defaults to `batch`. Examples: `batch`,
    /// `batch,flex`, `flex,priority`.
    pub service_tiers: Option<String>,
    /// When set to `service_tier`, the response is the demand cube
    /// `model -> window -> tier -> outcome -> count` instead of the flat
    /// `model -> window -> count` map. Grouped responses skip the
    /// priority-decay top-up: ask for an explicit trailing flex window
    /// instead.
    pub group_by: Option<String>,
}

/// Parse one entry from the `window=` query list.
///
/// Returns `Ok(None)` for an empty (skipped) entry, `Ok(Some(...))` for a
/// valid entry, or `Err` for malformed input. Shorthand `<end>` with a
/// non-negative end returns `start = None` (no lower bound: counts everything
/// due by `now + end`, including overdue); with a negative end it is
/// shorthand for `<end>:0s` (`-1h` = the trailing hour). Explicit
/// `<start>:<end>` returns `start = Some(...)` and enforces the lower bound
/// strictly; explicit ranges must satisfy `start < end`, so inverted or
/// empty ranges are rejected rather than silently returning zero counts.
/// Bounds may be negative on either side (`-1h:1h` spans now).
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
            if e < 0 { (Some(e), 0) } else { (None, e) }
        }
    };
    Ok(Some((trimmed.to_string(), start_secs, end_secs)))
}

/// Get request demand bucketed by service-time window.
///
/// Returns, per model, counts of requests whose *service time* falls within
/// each caller-specified window. For future time that means
/// pending/claimed/processing requests bucketed by deadline
/// (`submitted_at + completion_window`); for past time (windows with an
/// explicit negative bound) it additionally means terminal requests bucketed
/// by when they completed or failed — observed demand, the persistence
/// forecast input for autoscaling. Failed rows count on purpose: refused
/// realtime traffic is unserved demand. Each window is either `<end>` (bare
/// positive: "due by `now + end`", no lower bound, overdue included; bare
/// negative: shorthand for `<end>:0s`) or `<start>:<end>` for a disjoint
/// range. Both bounds are signed offsets from `now`.
///
/// Trailing counts read the live `requests` table only. Batchless tiers
/// (`flex`, `priority`) are exact — their rows are never archived. Batch-tier
/// rows move to the archive once their parent batch is terminal and frozen,
/// so batch-tier trailing counts cover only not-yet-archived rows and decay
/// as the sweeper runs; treat them as a lower bound, not an exact history.
///
/// Windows can overlap or be disjoint — the caller chooses. The windows are
/// deliberately decoupled from `config.batches.allowed_completion_windows`
/// so replica-allocation consumers can pick the lookahead shape they care
/// about independently of whatever completion-window SLAs the batch API
/// exposes to users.
///
/// Service-tier filtering: only the batch tier (`service_tier IS NULL`) by
/// default, `service_tiers=batch,flex` to widen. When
/// `batches.priority_decay_window_secs` is configured and `flex` is
/// included, recently completed flex requests are added back into the `1h`
/// label for that many seconds.
///
/// Always excludes: escalated requests (racing duplicates), requests
/// without a template_id, and requests in batches being cancelled.
#[utoipa::path(
    get,
    path = "/admin/api/v1/monitoring/demand",
    params(DemandQuery),
    responses(
        (status = 200, description = "Demand counts by model and window: `model -> window -> count`, or `model -> window -> tier -> outcome -> count` with `group_by=service_tier`", body = DemandResponse),
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
) -> Result<Json<DemandResponse>, Error> {
    let config = state.current_config();

    let grouped = match params.group_by.as_deref() {
        None => false,
        Some("service_tier") => true,
        Some(other) => {
            return Err(Error::BadRequest {
                message: format!("unsupported group_by {:?}: only `service_tier` is supported", other),
            });
        }
    };

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

    // Trailing sub-windows: the sub-zero part of any window with an explicit
    // negative lower bound. The pending branch takes every window whole —
    // its deadline semantics already extend below zero (a still-pending row
    // whose deadline passed 30 minutes ago has its service time in
    // `-1h:1h`). Bare positive shorthands keep `start = None` ("overdue
    // included") and never reach the trailing branch, so pre-existing flat
    // responses are unchanged.
    let trailing_windows: Vec<(String, i64, i64)> = windows
        .iter()
        .filter_map(|(label, start, end)| match start {
            Some(s) if *s < 0 => Some((label.clone(), *s, (*end).min(0))),
            _ => None,
        })
        .collect();

    let states = vec!["pending".to_string(), "claimed".to_string(), "processing".to_string()];
    let model_filter: Vec<String> = Vec::new();
    let service_tiers = parse_service_tiers(params.service_tiers.as_deref());

    if !grouped {
        let priority_decay_window_secs = if service_tiers_include_flex(&service_tiers) {
            config.batches.priority_decay_window_secs
        } else {
            None
        };
        let service_tier_filter = ServiceTierFilter::Include(service_tiers);

        let mut counts = state
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

        if !trailing_windows.is_empty() {
            let trailing = state
                .request_manager
                .get_completed_request_counts_by_model_and_window(&trailing_windows, &model_filter, &service_tier_filter)
                .await
                .map_err(|e| Error::Internal {
                    operation: format!("get trailing demand by window: {}", e),
                })?;
            for row in trailing {
                *counts.entry(row.model).or_default().entry(row.window_label).or_insert(0) += row.count;
            }
        }

        return Ok(Json(DemandResponse::Flat(counts)));
    }

    // Grouped: the pending query returns tier-summed counts, so tier
    // attribution comes from running it once per included tier; the trailing
    // query carries tiers natively, so it runs once. No priority-decay
    // top-up here — it would smuggle completed rows into a `pending` cell,
    // and a trailing flex window expresses the same thing honestly.
    let mut cube: GroupedDemandByModelAndWindow = HashMap::new();
    let mut seen_tiers: Vec<Option<String>> = Vec::new();
    for tier in &service_tiers {
        if seen_tiers.contains(tier) {
            continue;
        }
        seen_tiers.push(tier.clone());
        let tier_key = tier.as_deref().unwrap_or("batch").to_string();
        let counts = state
            .request_manager
            .get_pending_request_counts_by_model_and_window(
                &windows,
                &states,
                &model_filter,
                &ServiceTierFilter::Include(vec![tier.clone()]),
                None,
                false,
            )
            .await
            .map_err(|e| Error::Internal {
                operation: format!("get demand by window for tier {}: {}", tier_key, e),
            })?;
        for (model, by_window) in counts {
            for (window_label, count) in by_window {
                cube.entry(model.clone())
                    .or_default()
                    .entry(window_label)
                    .or_default()
                    .entry(tier_key.clone())
                    .or_default()
                    .insert("pending".to_string(), count);
            }
        }
    }

    if !trailing_windows.is_empty() {
        let trailing: Vec<TrailingDemandCount> = state
            .request_manager
            .get_completed_request_counts_by_model_and_window(&trailing_windows, &model_filter, &ServiceTierFilter::Include(service_tiers))
            .await
            .map_err(|e| Error::Internal {
                operation: format!("get trailing demand by window: {}", e),
            })?;
        for row in trailing {
            let tier_key = row.service_tier.as_deref().unwrap_or("batch").to_string();
            cube.entry(row.model)
                .or_default()
                .entry(row.window_label)
                .or_default()
                .entry(tier_key)
                .or_default()
                .insert(row.outcome, row.count);
        }
    }

    Ok(Json(DemandResponse::Grouped(cube)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::users::Role;
    use crate::test::utils::*;
    use axum_test::TestServer;
    use sqlx::PgPool;

    #[sqlx::test]
    async fn test_demand_endpoint_requires_system_permission(pool: sqlx::PgPool) {
        let (server, _bg): (TestServer, _) = create_test_app(pool.clone(), false).await;

        // StandardUser should NOT have System::ReadAll permission
        let standard_user = create_test_user(&pool, Role::StandardUser).await;
        let response = server
            .get("/admin/api/v1/monitoring/demand?window=1h,24h")
            .add_header(&add_auth_headers(&standard_user)[0].0, &add_auth_headers(&standard_user)[0].1)
            .add_header(&add_auth_headers(&standard_user)[1].0, &add_auth_headers(&standard_user)[1].1)
            .await;
        response.assert_status(axum::http::StatusCode::FORBIDDEN);

        // PlatformManager should have System::ReadAll permission
        let platform_manager = create_test_user(&pool, Role::PlatformManager).await;
        let response = server
            .get("/admin/api/v1/monitoring/demand?window=1h,24h")
            .add_header(&add_auth_headers(&platform_manager)[0].0, &add_auth_headers(&platform_manager)[0].1)
            .add_header(&add_auth_headers(&platform_manager)[1].0, &add_auth_headers(&platform_manager)[1].1)
            .await;
        response.assert_status_ok();
    }

    #[sqlx::test]
    async fn test_demand_returns_empty_when_no_requests(pool: PgPool) {
        let (server, _bg): (TestServer, _) = create_test_app(pool.clone(), false).await;
        let admin = create_test_admin_user(&pool, Role::PlatformManager).await;

        // Query the endpoint
        let response = server
            .get("/admin/api/v1/monitoring/demand?window=1h,24h")
            .add_header(&add_auth_headers(&admin)[0].0, &add_auth_headers(&admin)[0].1)
            .add_header(&add_auth_headers(&admin)[1].0, &add_auth_headers(&admin)[1].1)
            .await;

        response.assert_status_ok();
        let counts: HashMap<String, HashMap<String, i64>> = response.json();

        // Should be empty when no requests exist
        assert_eq!(counts.len(), 0, "Should have no pending requests");
    }

    #[sqlx::test]
    async fn test_demand_defaults_to_batch_tier_only(pool: PgPool) {
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
            .get("/admin/api/v1/monitoring/demand?window=1h,24h")
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
    async fn test_demand_service_tiers_query_includes_requested_tiers(pool: PgPool) {
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
            .get("/admin/api/v1/monitoring/demand?window=1h,24h&service_tiers=batch,flex")
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
    async fn test_demand_priority_decay_window_requires_flex_tier(pool: PgPool) {
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
            .get("/admin/api/v1/monitoring/demand?window=1h,24h")
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
            .get("/admin/api/v1/monitoring/demand?window=1h,24h&service_tiers=flex")
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
            "window=2h:1h",
            "window=1h:1h",
            "window=-1h:-2h",
            "window=-1h:-1h",
            "window=--1h",
            "window=-",
            "window=1h&group_by=bogus",
        ] {
            let response = server
                .get(&format!("/admin/api/v1/monitoring/demand?{}", bad))
                .add_header(&add_auth_headers(&admin)[0].0, &add_auth_headers(&admin)[0].1)
                .add_header(&add_auth_headers(&admin)[1].0, &add_auth_headers(&admin)[1].1)
                .await;
            response.assert_status(axum::http::StatusCode::BAD_REQUEST);
        }
    }

    #[test]
    fn test_parse_demand_window_signed() {
        // Bare positive: unbounded below (overdue included) — unchanged.
        assert_eq!(parse_demand_window("1h"), Ok(Some(("1h".to_string(), None, 3600))));
        assert_eq!(parse_demand_window("30m"), Ok(Some(("30m".to_string(), None, 1800))));
        // Bare negative: shorthand for `<end>:0s`.
        assert_eq!(parse_demand_window("-1h"), Ok(Some(("-1h".to_string(), Some(-3600), 0))));
        assert_eq!(parse_demand_window("-30m"), Ok(Some(("-30m".to_string(), Some(-1800), 0))));
        // Explicit signed ranges.
        assert_eq!(parse_demand_window("-1h:1h"), Ok(Some(("-1h:1h".to_string(), Some(-3600), 3600))));
        assert_eq!(
            parse_demand_window("-2h:-1h"),
            Ok(Some(("-2h:-1h".to_string(), Some(-7200), -3600)))
        );
        assert_eq!(parse_demand_window("-1h:0s"), Ok(Some(("-1h:0s".to_string(), Some(-3600), 0))));
        // Inverted / empty / malformed stay rejected.
        assert!(parse_demand_window("-1h:-2h").is_err());
        assert!(parse_demand_window("-1h:-1h").is_err());
        assert!(parse_demand_window("--1h").is_err());
        assert!(parse_demand_window("-").is_err());
    }

    #[sqlx::test]
    async fn test_demand_trailing_windows_and_cube(pool: PgPool) {
        use fusillade::{BatchInput, PersistCompletedRealtimeInput, RequestTemplateInput, Storage};
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

        let model = "trailing-model";

        // One pending batch-tier request due in 30 minutes.
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
            .create_file("trailing-test".to_string(), None, vec![template])
            .await
            .expect("create_file");
        // 24h keeps the requests on the batch tier (a 1h completion window
        // maps them to flex); the pinned expires_at below is what places the
        // deadline inside the 1h future part of the spanning window.
        let batch = request_manager
            .create_batch(BatchInput {
                file_id,
                endpoint: "/v1/chat/completions".to_string(),
                completion_window: "24h".to_string(),
                metadata: None,
                created_by: None,
                api_key_id: None,
                api_key: None,
                total_requests: None,
            })
            .await
            .expect("create_batch");
        sqlx::query("UPDATE batches SET expires_at = NOW() + interval '30 minutes' WHERE id = $1")
            .bind(batch.id.0)
            .execute(&fusillade_pool)
            .await
            .expect("pin expires_at");

        // Two terminal realtime rows in the trailing hour: one served (200,
        // -> completed_at), one refused upstream (402 -> failed_at). Both are
        // observed demand.
        let now = chrono::Utc::now();
        let realtime_input = |status_code: u16| PersistCompletedRealtimeInput {
            request_id: uuid::Uuid::new_v4(),
            response_body: r#"{"ok":true}"#.to_string(),
            status_code,
            request_body: r#"{"input":"x"}"#.to_string(),
            model: model.to_string(),
            endpoint: "http://localhost".to_string(),
            method: "POST".to_string(),
            path: "/v1/responses".to_string(),
            api_key: String::new(),
            created_by: "queue-user".to_string(),
            started_at: now - chrono::Duration::seconds(90),
            completed_at: now - chrono::Duration::seconds(60),
        };
        request_manager
            .persist_completed_realtime_batch(&[realtime_input(200), realtime_input(402)])
            .await
            .expect("persist realtime rows");

        let get = |query: String| {
            let server = &server;
            let admin = &admin;
            async move {
                server
                    .get(&format!("/admin/api/v1/monitoring/demand?{query}"))
                    .add_header(&add_auth_headers(admin)[0].0, &add_auth_headers(admin)[0].1)
                    .add_header(&add_auth_headers(admin)[1].0, &add_auth_headers(admin)[1].1)
                    .await
            }
        };

        // Flat, default tiers: the trailing window exists but priority isn't
        // included, so the realtime rows stay invisible.
        let response = get("window=-1h".to_string()).await;
        response.assert_status_ok();
        let counts: HashMap<String, HashMap<String, i64>> = response.json();
        assert!(
            !counts.contains_key(model),
            "priority rows must not leak into the default batch tier: {counts:?}"
        );

        // Flat, priority only: both terminal rows under the caller's label.
        let response = get("window=-1h&service_tiers=priority".to_string()).await;
        response.assert_status_ok();
        let counts: HashMap<String, HashMap<String, i64>> = response.json();
        assert_eq!(counts.get(model).and_then(|m| m.get("-1h")), Some(&2), "{counts:?}");

        // Flat, spanning zero: 1 pending (due in 30m) + 2 trailing = 3.
        let response = get("window=-1h:1h&service_tiers=batch,priority".to_string()).await;
        response.assert_status_ok();
        let counts: HashMap<String, HashMap<String, i64>> = response.json();
        assert_eq!(counts.get(model).and_then(|m| m.get("-1h:1h")), Some(&3), "{counts:?}");

        // Grouped: the same window as a cube, tiers and outcomes broken out.
        let response = get("window=-1h:1h&service_tiers=batch,priority&group_by=service_tier".to_string()).await;
        response.assert_status_ok();
        let cube: GroupedDemandByModelAndWindow = response.json();
        let window = cube
            .get(model)
            .and_then(|m| m.get("-1h:1h"))
            .unwrap_or_else(|| panic!("expected {model}/-1h:1h in cube, got {cube:?}"));
        assert_eq!(window.get("batch").and_then(|t| t.get("pending")), Some(&1), "{cube:?}");
        assert_eq!(window.get("priority").and_then(|t| t.get("completed")), Some(&1), "{cube:?}");
        assert_eq!(window.get("priority").and_then(|t| t.get("failed")), Some(&1), "{cube:?}");
    }
}
