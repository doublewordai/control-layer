//! Response (batchless fusillade request) handlers.
//!
//! Lists and fetches individual responses — fusillade rows with
//! `batch_id IS NULL` and `created_by IS NOT NULL`. Despite the legacy
//! `/admin/api/v1/batches/requests` route, these endpoints are
//! batchless-only: `list_requests` filters batched rows out at the SQL
//! layer, and `get_batch_request` rejects batched IDs with 404. Combines
//! fusillade data with `http_analytics` enrichment (tokens, cost) and
//! creator emails from the users table.

use axum::{
    extract::{Path, Query, State},
    response::Json,
};
use fusillade::Storage;
use sqlx_pool_router::PoolProvider;
use uuid::Uuid;

use crate::{
    AppState,
    api::models::{
        batch_requests::{ListBatchRequestsQuery, ResponseDetail, ResponseSummary},
        pagination::PaginatedResponse,
        users::CurrentUser,
    },
    auth::permissions::{RequiresPermission, can_read_all_resources, operation, resource},
    errors::{Error, Result},
    types::Resource,
};

/// Check if the current user "owns" a batch, considering org context.
fn is_batch_owner(current_user: &CurrentUser, created_by: &str) -> bool {
    let user_id = current_user.id.to_string();
    if created_by == user_id {
        return true;
    }
    if let Some(org_id) = current_user.active_organization
        && created_by == org_id.to_string()
    {
        return true;
    }
    false
}

/// List responses (batchless fusillade requests).
///
/// Returns only rows with `batch_id IS NULL` — fusillade's `list_requests`
/// filters batched rows at the SQL layer. Enriched with token/cost metrics
/// from `http_analytics` and creator emails from the users table.
#[utoipa::path(
    get,
    path = "/admin/api/v1/batches/requests",
    params(ListBatchRequestsQuery),
    responses(
        (status = 200, description = "Paginated list of responses", body = PaginatedResponse<ResponseSummary>),
        (status = 403, description = "Insufficient permissions"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "batch_requests",
)]
#[tracing::instrument(skip_all)]
pub async fn list_batch_requests<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Query(query): Query<ListBatchRequestsQuery>,
    current_user: CurrentUser,
    _: RequiresPermission<resource::Batches, operation::ReadOwn>,
) -> Result<Json<PaginatedResponse<ResponseSummary>>> {
    let skip = query.pagination.skip();
    let limit = query.pagination.limit();
    let can_read_all = can_read_all_resources(&current_user, Resource::Batches);

    // Build ownership filter — member_id only allowed for users with ReadAll permission
    let created_by_filter: Option<String> = if let Some(member_id) = query.member_id {
        if !can_read_all {
            return Err(Error::BadRequest {
                message: "member_id filter requires platform manager permissions".to_string(),
            });
        }
        Some(member_id.to_string())
    } else if can_read_all {
        None
    } else {
        Some(
            current_user
                .active_organization
                .map(|org_id| org_id.to_string())
                .unwrap_or_else(|| current_user.id.to_string()),
        )
    };

    // Parse comma-separated model filter
    let models: Option<Vec<String>> = query
        .model
        .as_ref()
        .map(|m| m.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect());

    // Query fusillade for core request data
    let result = state
        .request_manager
        .list_requests(fusillade::ListRequestsFilter {
            created_by: created_by_filter,
            status: query.status.clone(),
            models,
            created_after: query.created_after,
            created_before: query.created_before,
            service_tiers: query
                .service_tiers
                .as_ref()
                .map(|s| s.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect()),
            active_first: query.active_first.unwrap_or(true),
            skip,
            limit,
        })
        .await
        .map_err(|e| Error::Internal {
            operation: format!("list responses: {}", e),
        })?;

    // Enrich with http_analytics data (tokens, cost) and user emails
    let request_ids: Vec<Uuid> = result.data.iter().map(|r| r.id).collect();

    let analytics = if !request_ids.is_empty() {
        sqlx::query_as::<_, AnalyticsRow>(
            r#"
            SELECT DISTINCT ON (fusillade_request_id)
                fusillade_request_id as request_id,
                prompt_tokens,
                completion_tokens,
                reasoning_tokens,
                total_tokens,
                total_cost::float8 as total_cost
            FROM http_analytics
            WHERE fusillade_request_id = ANY($1)
            ORDER BY fusillade_request_id, timestamp DESC
            "#,
        )
        .bind(&request_ids)
        .fetch_all(state.db.read())
        .await
        .map_err(|e| Error::Database(e.into()))?
    } else {
        vec![]
    };

    let analytics_map: std::collections::HashMap<Uuid, AnalyticsRow> = analytics.into_iter().map(|a| (a.request_id, a)).collect();

    // Fetch creator emails
    let unique_creator_ids: Vec<String> = result
        .data
        .iter()
        .map(|r| r.created_by.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    let emails = if !unique_creator_ids.is_empty() {
        sqlx::query_as::<_, EmailRow>("SELECT id::text as user_id, email FROM users WHERE id::text = ANY($1)")
            .bind(&unique_creator_ids)
            .fetch_all(state.db.write())
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "failed to fetch creator emails for responses");
                vec![]
            })
    } else {
        vec![]
    };

    let email_map: std::collections::HashMap<String, String> = emails.into_iter().map(|e| (e.user_id, e.email)).collect();

    // Combine fusillade data with analytics enrichment
    let data: Vec<ResponseSummary> = result
        .data
        .into_iter()
        .map(|r| {
            let a = analytics_map.get(&r.id);
            let email: Option<String> = email_map.get(&r.created_by).cloned();
            ResponseSummary {
                id: r.id,
                batch_id: r.batch_id,
                model: r.model,
                status: r.status,
                created_at: r.created_at,
                completed_at: r.completed_at,
                failed_at: r.failed_at,
                duration_ms: r.duration_ms,
                response_status: r.response_status,
                service_tier: r.service_tier,
                prompt_tokens: a.and_then(|a| a.prompt_tokens),
                completion_tokens: a.and_then(|a| a.completion_tokens),
                reasoning_tokens: a.and_then(|a| a.reasoning_tokens),
                total_tokens: a.and_then(|a| a.total_tokens),
                total_cost: a.and_then(|a| a.total_cost),
                created_by_email: email,
            }
        })
        .collect();

    Ok(Json(PaginatedResponse::new(data, result.total_count, skip, limit)))
}

/// Get a response (batchless fusillade request) by ID.
///
/// Batched-row IDs return 404 — this endpoint is the detail view for
/// `list_batch_requests`, which is batchless-only.
#[utoipa::path(
    get,
    path = "/admin/api/v1/batches/requests/{request_id}",
    params(
        ("request_id" = Uuid, Path, description = "The request ID"),
    ),
    responses(
        (status = 200, description = "Response detail", body = ResponseDetail),
        (status = 404, description = "Response not found"),
        (status = 403, description = "Insufficient permissions"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "batch_requests",
)]
#[tracing::instrument(skip_all)]
pub async fn get_batch_request<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(request_id): Path<Uuid>,
    current_user: CurrentUser,
    _: RequiresPermission<resource::Batches, operation::ReadOwn>,
) -> Result<Json<ResponseDetail>> {
    // Get core request detail from fusillade
    let detail = state
        .request_manager
        .get_request_detail(fusillade::RequestId(request_id))
        .await
        .map_err(|e| match e {
            fusillade::FusilladeError::RequestNotFound(_) => Error::NotFound {
                resource: "Response".to_string(),
                id: request_id.to_string(),
            },
            other => {
                tracing::error!(request_id = %request_id, error = %other, "failed to fetch response detail");
                Error::Internal {
                    operation: format!("get response detail: {}", other),
                }
            }
        })?;

    // Check ownership — fetch-then-check pattern matches get_batch handler.
    // 404 (not 403) avoids leaking existence of other users' responses.
    // `detail.created_by` is guaranteed non-empty:
    //   1. `get_request_detail` filters `WHERE r.created_by IS NOT NULL`, so
    //      batched rows return `RequestNotFound`.
    //   2. `create_realtime` / `create_flex` coerce empty-string inputs to
    //      NULL, which the XOR CHECK constraint rejects at insert time.
    // So if the row is returned, its `created_by` came from a non-empty input.
    let can_read_all = can_read_all_resources(&current_user, Resource::Batches);
    if !can_read_all && !is_batch_owner(&current_user, &detail.created_by) {
        return Err(Error::NotFound {
            resource: "Response".to_string(),
            id: request_id.to_string(),
        });
    }

    // Enrich with analytics from http_analytics (dwctl-owned table)
    let analytics = sqlx::query_as::<_, AnalyticsRow>(
        r#"
        SELECT DISTINCT ON (fusillade_request_id)
            fusillade_request_id as request_id,
            prompt_tokens,
            completion_tokens,
            reasoning_tokens,
            total_tokens,
            total_cost::float8 as total_cost
        FROM http_analytics
        WHERE fusillade_request_id = $1
        ORDER BY fusillade_request_id, timestamp DESC
        "#,
    )
    .bind(request_id)
    .fetch_optional(state.db.read())
    .await
    .map_err(|e| Error::Database(e.into()))?;

    // Look up the creator's email via UUID primary-key lookup (org IDs and unparseable
    // values return None without a query). Uses the primary pool to avoid replica lag
    // right after response creation.
    let created_by_email = if let Ok(created_by_uuid) = Uuid::parse_str(&detail.created_by) {
        sqlx::query_as::<_, EmailRow>("SELECT id::text as user_id, email FROM users WHERE id = $1")
            .bind(created_by_uuid)
            .fetch_optional(state.db.write())
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "failed to fetch creator email for response detail");
                None
            })
            .map(|row| row.email)
    } else {
        None
    };

    Ok(Json(ResponseDetail {
        id: detail.id,
        batch_id: detail.batch_id,
        model: detail.model,
        status: detail.status,
        created_at: detail.created_at,
        completed_at: detail.completed_at,
        failed_at: detail.failed_at,
        duration_ms: detail.duration_ms,
        response_status: detail.response_status,
        service_tier: detail.service_tier,
        prompt_tokens: analytics.as_ref().and_then(|a| a.prompt_tokens),
        completion_tokens: analytics.as_ref().and_then(|a| a.completion_tokens),
        reasoning_tokens: analytics.as_ref().and_then(|a| a.reasoning_tokens),
        total_tokens: analytics.as_ref().and_then(|a| a.total_tokens),
        total_cost: analytics.as_ref().and_then(|a| a.total_cost),
        body: detail.body.unwrap_or_default(),
        response_body: detail.response_body,
        error: detail.error,
        created_by: detail.created_by,
        created_by_email,
    }))
}

/// Analytics data from http_analytics (dwctl-owned, not fusillade schema)
#[derive(sqlx::FromRow)]
struct AnalyticsRow {
    #[allow(dead_code)]
    request_id: Uuid,
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    total_tokens: Option<i64>,
    total_cost: Option<f64>,
}

/// User email lookup from users table (dwctl-owned)
#[derive(sqlx::FromRow)]
struct EmailRow {
    user_id: String,
    email: String,
}

#[cfg(test)]
mod tests {
    use crate::{api::models::users::Role, test::utils::*};
    use sqlx::PgPool;

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_batch_requests_requires_auth(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;

        let response = app.get("/admin/api/v1/batches/requests").await;
        response.assert_status_unauthorized();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_batch_requests_empty(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        let response = app
            .get("/admin/api/v1/batches/requests")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let body: serde_json::Value = response.json();
        assert_eq!(body["total_count"], 0);
        assert!(body["data"].as_array().unwrap().is_empty());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_member_id_filter_rejected_for_non_pm(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        let response = app
            .get(&format!("/admin/api/v1/batches/requests?member_id={}", uuid::Uuid::new_v4()))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status(axum::http::StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_batch_request_returns_404_for_missing(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let auth = add_auth_headers(&user);

        let response = app
            .get(&format!("/admin/api/v1/batches/requests/{}", uuid::Uuid::new_v4()))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;

        response.assert_status_not_found();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_batch_request_populates_created_by_email(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let auth = add_auth_headers(&user);

        // Insert a batchless fusillade response (template with file_id IS NULL,
        // request with batch_id IS NULL + created_by set), matching how realtime/
        // flex responses are stored after the responses-from-batches separation.
        let template_id = uuid::Uuid::new_v4();
        let request_id = uuid::Uuid::new_v4();
        let body = serde_json::json!({"model": "test-model", "messages": [{"role": "user", "content": "hi"}]});

        sqlx::query(
            "INSERT INTO fusillade.request_templates (id, file_id, model, api_key, endpoint, path, body, custom_id, method) VALUES ($1, NULL, 'test-model', 'test-key', 'http://test', '/v1/chat/completions', $2, NULL, 'POST')",
        )
        .bind(template_id)
        .bind(serde_json::to_string(&body).unwrap())
        .execute(&pool)
        .await
        .expect("insert template");

        sqlx::query(
            "INSERT INTO fusillade.requests (id, batch_id, template_id, model, state, created_at, created_by) VALUES ($1, NULL, $2, 'test-model', 'pending', NOW(), $3)",
        )
        .bind(request_id)
        .bind(template_id)
        .bind(user.id.to_string())
        .execute(&pool)
        .await
        .expect("insert request");

        let response = app
            .get(&format!("/admin/api/v1/batches/requests/{}", request_id))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;

        response.assert_status_ok();
        let body: serde_json::Value = response.json();
        assert_eq!(body["id"], request_id.to_string());
        assert_eq!(body["created_by_email"], user.email);
    }
}
