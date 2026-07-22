//! Cross-batch request query types and filters.
//!
//! These types support listing and retrieving individual requests across batches,
//! with server-side filtering, pagination, and sorting.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Default number of rows to return when limit is not specified.
const DEFAULT_LIMIT: i64 = 50;

/// Derive the service tier from the batch completion window.
/// "1h" → "flex" (async), everything else → NULL (batch).
pub fn service_tier_from_completion_window(completion_window: &str) -> Option<&'static str> {
    match completion_window {
        "1h" => Some("flex"),
        "0s" => Some("priority"),
        _ => None,
    }
}

/// Filter on `service_tier`.
///
/// `None` in the inner vec represents the batch tier (`service_tier IS NULL`);
/// named strings match specific tier values such as `"flex"` or `"priority"`.
///
/// `Default` is `Any` — no filter applied.
#[derive(Debug, Clone, Default)]
pub enum ServiceTierFilter {
    /// No filter — match all tiers including batch (NULL).
    #[default]
    Any,
    /// Match only rows whose tier is in this set. Empty matches nothing.
    Include(Vec<Option<String>>),
    /// Match all tiers except those in this set.
    Exclude(Vec<Option<String>>),
}

impl ServiceTierFilter {
    /// Split a list of `Option<String>` tiers into (named_tiers, includes_null).
    pub fn split(tiers: &[Option<String>]) -> (Vec<String>, bool) {
        let mut names = Vec::with_capacity(tiers.len());
        let mut has_null = false;
        for t in tiers {
            match t {
                Some(s) => names.push(s.clone()),
                None => has_null = true,
            }
        }
        (names, has_null)
    }
}

/// Filter parameters for listing requests across batches.
#[derive(Debug, Clone)]
pub struct ListRequestsFilter {
    /// Filter by request creator (user ID or org ID)
    pub created_by: Option<String>,
    /// Filter by request state (pending, claimed, processing, completed, failed, canceled)
    pub status: Option<String>,
    /// Filter by model(s) — when multiple, matches any.
    /// `None` disables model filtering. `Some(vec![])` matches no rows.
    pub models: Option<Vec<String>>,
    /// Only return requests created after this timestamp
    pub created_after: Option<DateTime<Utc>>,
    /// Only return requests created before this timestamp
    pub created_before: Option<DateTime<Utc>>,
    /// Filter by service tier(s). When set, only returns requests whose
    /// `service_tier` matches one of the provided values (e.g., `["flex", "priority"]`).
    /// Uses `= ANY($7)` which hits the composite `idx_requests_created_tier` index.
    /// `None` disables tier filtering. `Some(vec![])` matches no rows.
    pub service_tiers: Option<Vec<String>>,
    /// Sort active requests (pending/claimed/processing) first
    pub active_first: bool,
    /// Number of rows to skip (offset pagination)
    pub skip: i64,
    /// Maximum number of rows to return (defaults to 50)
    pub limit: i64,
}

impl Default for ListRequestsFilter {
    fn default() -> Self {
        Self {
            created_by: None,
            status: None,
            models: None,
            created_after: None,
            created_before: None,
            service_tiers: None,
            active_first: false,
            skip: 0,
            limit: DEFAULT_LIMIT,
        }
    }
}

/// Summary of an individual request, suitable for list views.
///
/// **Row scope: batchless-only.** `list_requests` filters
/// `WHERE r.created_by IS NOT NULL` so the planner can use the partial
/// index `idx_requests_user_*_sort`. Batched rows are not returned here.
/// For batched-row attribution, fetch the row's `batches.created_by`
/// separately or use `RequestDetail` (which joins batches).
///
/// Note: This type does not include user email or token/cost metrics.
/// Callers should enrich with data from their own tables (users, analytics).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx-postgres", derive(sqlx::FromRow))]
pub struct RequestSummary {
    pub id: Uuid,
    pub batch_id: Option<Uuid>,
    pub model: String,
    #[cfg_attr(feature = "sqlx-postgres", sqlx(rename = "state"))]
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub failed_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<f64>,
    pub response_status: Option<i16>,
    pub service_tier: Option<String>,
    /// Creator ID (user or org) for ownership checks and email lookup.
    /// Always set: `list_requests` only returns rows where
    /// `requests.created_by IS NOT NULL` (see struct-level docs).
    pub created_by: String,
}

/// Internal row shape used previously when `list_requests` computed the total
/// count via a `COUNT(*) OVER()` window function. Kept for backward
/// compatibility with the public re-export introduced in v16.1.0; no longer
/// constructed by this crate.
#[allow(deprecated)]
mod deprecated_types {
    use super::{DateTime, Deserialize, RequestSummary, Serialize, Utc, Uuid};

    #[deprecated(
        since = "16.1.1",
        note = "no longer used internally; use RequestSummary and RequestListResult.total_count"
    )]
    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[cfg_attr(feature = "sqlx-postgres", derive(sqlx::FromRow))]
    pub struct RequestSummaryWithCount {
        pub id: Uuid,
        pub batch_id: Uuid,
        pub model: String,
        #[cfg_attr(feature = "sqlx-postgres", sqlx(rename = "state"))]
        pub status: String,
        pub created_at: DateTime<Utc>,
        pub completed_at: Option<DateTime<Utc>>,
        pub failed_at: Option<DateTime<Utc>>,
        pub duration_ms: Option<f64>,
        pub response_status: Option<i16>,
        pub service_tier: Option<String>,
        pub batch_created_by: String,
        pub total_count: i64,
    }

    impl From<RequestSummaryWithCount> for RequestSummary {
        fn from(r: RequestSummaryWithCount) -> Self {
            Self {
                id: r.id,
                batch_id: Some(r.batch_id),
                model: r.model,
                status: r.status,
                created_at: r.created_at,
                completed_at: r.completed_at,
                failed_at: r.failed_at,
                duration_ms: r.duration_ms,
                response_status: r.response_status,
                service_tier: r.service_tier,
                created_by: r.batch_created_by,
            }
        }
    }
}

#[allow(deprecated)]
pub use deprecated_types::RequestSummaryWithCount;

/// Full detail of an individual request, including body and response.
///
/// **Row scope: batchless-only.** `get_request_detail` filters
/// `WHERE r.created_by IS NOT NULL`. Per-row inspection of batched
/// requests goes through `get_batch_results_stream` / `BatchResultItem`
/// instead. Used by the dashboard's response-detail page and the Open
/// Responses API.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "sqlx-postgres", derive(sqlx::FromRow))]
pub struct RequestDetail {
    pub id: Uuid,
    /// Always `None` — this query is scoped to batchless rows only.
    /// Per-row inspection of batched requests uses
    /// `get_batch_results_stream` / `BatchResultItem`.
    pub batch_id: Option<Uuid>,
    pub model: String,
    #[cfg_attr(feature = "sqlx-postgres", sqlx(rename = "state"))]
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub failed_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<f64>,
    pub response_status: Option<i16>,
    /// `None` when the template has been purged (file soft-deleted + orphan purge).
    pub body: Option<String>,
    pub response_body: Option<String>,
    pub error: Option<String>,
    pub service_tier: Option<String>,
    /// Creator ID (user or org). Always set: `get_request_detail` only
    /// returns rows where `requests.created_by IS NOT NULL`.
    pub created_by: String,
}

/// Input for creating a realtime response that the proxy is already handling.
///
/// Inserts a request template (no parent file) and a request row in
/// `processing` state with `batch_id = NULL` and `daemon_id = Uuid::nil()`.
/// The proxy completes/fails the row directly via `complete_request` /
/// `fail_request`; the daemon never claims it.
#[derive(Debug, Clone)]
pub struct CreateRealtimeInput {
    /// Pre-generated request ID. Becomes the request's primary key.
    pub request_id: Uuid,
    /// The request body as a JSON string.
    pub body: String,
    /// Model identifier.
    pub model: String,
    /// Base URL of the target endpoint (e.g., "http://localhost:3001/ai").
    pub endpoint: String,
    /// HTTP method (e.g., "POST").
    pub method: String,
    /// API path (e.g., "/v1/responses").
    pub path: String,
    /// API key for the request.
    pub api_key: String,
    /// User/org ID that owns this request.
    pub created_by: String,
}

/// Input for persisting a batch of already-completed realtime responses.
///
/// Used by the dwctl responses writer to flush a buffer of finished
/// realtime calls in one transaction.
///
/// Two cases are handled in the same batch:
///   * Background realtime: the row was pre-created in `processing` state by
///     `create_realtime`; we UPDATE it to `completed`.
///   * Non-background realtime: no row exists yet; we INSERT a template and
///     a request row directly in `completed` state.
///
/// All synthesize fields (`request_body`, `model`, `endpoint`, `started_at`,
/// `completed_at`, etc.) are only consulted on the INSERT path. On the UPDATE
/// path only `request_id`, `response_body`, and `status_code` are used — the
/// pre-existing row already carries a real `started_at` from `create_realtime`.
#[derive(Debug, Clone)]
pub struct PersistCompletedRealtimeInput {
    /// The request UUID (primary key).
    pub request_id: Uuid,
    /// Upstream response body to store.
    pub response_body: String,
    /// Upstream HTTP status code.
    pub status_code: u16,
    /// Original request body, stored on the synthesized template.
    pub request_body: String,
    /// Model identifier.
    pub model: String,
    /// Base URL of the target endpoint.
    pub endpoint: String,
    /// HTTP method (e.g., "POST").
    pub method: String,
    /// API path (e.g., "/v1/responses").
    pub path: String,
    /// API key for the request.
    pub api_key: String,
    /// User/org ID that owns this request.
    pub created_by: String,
    /// Wall-clock instant the request arrived, as recorded by the caller.
    /// INSERT path only: becomes the synthesized row's `created_at`,
    /// `claimed_at`, and `started_at`. Ignored on the UPDATE path, where the row
    /// already carries a real `started_at`.
    pub started_at: DateTime<Utc>,
    /// Wall-clock instant the response completed (`started_at` plus the caller's
    /// measured request duration). INSERT path only: stored as the row's `completed_at` on 2xx
    /// (so the listing's `duration_ms = completed_at - started_at` reflects the
    /// true latency instead of zero) or as `failed_at` on non-2xx. Note
    /// `duration_ms` is derived from the `completed_at` column, so it is NULL for
    /// failed rows — the completion instant is still recorded there, in
    /// `failed_at`. Ignored on the UPDATE path.
    pub completed_at: DateTime<Utc>,
}

/// Input for creating a flex (async) response that the daemon will process.
///
/// Inserts a request template (no parent file) and a request row in `pending`
/// state with `batch_id = NULL`. The daemon claims and processes it like any
/// other pending request.
#[derive(Debug, Clone)]
pub struct CreateFlexInput {
    /// Pre-generated request ID. Becomes the request's primary key.
    pub request_id: Uuid,
    /// The request body as a JSON string.
    pub body: String,
    /// Model identifier.
    pub model: String,
    /// Base URL of the target endpoint (e.g., "http://localhost:3001/ai").
    pub endpoint: String,
    /// HTTP method (e.g., "POST").
    pub method: String,
    /// API path (e.g., "/v1/responses").
    pub path: String,
    /// API key for the request.
    pub api_key: String,
    /// User/org ID that owns this request.
    pub created_by: String,
}

/// Command for creating either kind of durable response row.
#[derive(Debug, Clone)]
pub enum CreateResponseInput {
    Flex(CreateFlexInput),
    Realtime(CreateRealtimeInput),
}

/// Result of a paginated request list query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestListResult {
    pub data: Vec<RequestSummary>,
    /// Best-effort total row count for the full query result.
    ///
    /// Returns an exact count when the count query completes within a short
    /// internal timeout; otherwise falls back to a query-planner row estimate.
    /// Planner estimates are typically within a few percent when table
    /// statistics are current, but may diverge more if stats are stale.
    pub total_count: i64,
}
