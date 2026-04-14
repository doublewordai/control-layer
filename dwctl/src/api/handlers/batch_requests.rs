//! Batch request handlers
//!
//! Endpoints for listing individual requests across fusillade batches.
//! Queries the fusillade schema directly for cross-batch request listing.

use axum::{
    extract::{Path, Query, State},
    response::Json,
};
use sqlx_pool_router::PoolProvider;
use uuid::Uuid;

use crate::{
    AppState,
    api::models::{
        batch_requests::{BatchRequestDetail, BatchRequestSummary, ListBatchRequestsQuery},
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

/// List individual batch requests across all batches
///
/// Returns a paginated list of individual requests from the fusillade requests table,
/// with optional filtering by completion window, status, and model. Supports
/// active-first sorting to show in-progress requests at the top.
#[utoipa::path(
    get,
    path = "/admin/api/v1/batches/requests",
    params(ListBatchRequestsQuery),
    responses(
        (status = 200, description = "Paginated list of batch requests", body = PaginatedResponse<BatchRequestSummary>),
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
) -> Result<Json<PaginatedResponse<BatchRequestSummary>>> {
    let skip = query.pagination.skip();
    let limit = query.pagination.limit();
    let can_read_all = can_read_all_resources(&current_user, Resource::Batches);
    let active_first = query.active_first.unwrap_or(true);

    // Build ownership filter — member_id overrides for PMs, otherwise own batches
    let created_by_filter: Option<String> = if let Some(member_id) = query.member_id {
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

    let pool = state.db.read();

    // Count query
    let total_count: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)::bigint
        FROM fusillade.requests r
        JOIN fusillade.batches b ON r.batch_id = b.id
        WHERE b.deleted_at IS NULL
          AND ($1::text IS NULL OR b.created_by = $1)
          AND ($2::text IS NULL OR b.completion_window = $2)
          AND ($3::text IS NULL OR r.state = $3)
          AND ($4::text IS NULL OR r.model = $4)
          AND ($5::timestamptz IS NULL OR r.created_at >= $5)
          AND ($6::timestamptz IS NULL OR r.created_at <= $6)
        "#,
    )
    .bind(created_by_filter.as_deref())
    .bind(query.completion_window.as_deref())
    .bind(query.status.as_deref())
    .bind(query.model.as_deref())
    .bind(query.created_after)
    .bind(query.created_before)
    .fetch_one(pool)
    .await
    .map_err(|e| Error::Database(e.into()))?;

    // Data query — use active_first to pick ordering
    let order_clause = if active_first {
        "CASE r.state WHEN 'processing' THEN 0 WHEN 'claimed' THEN 1 WHEN 'pending' THEN 2 ELSE 3 END ASC, r.created_at DESC"
    } else {
        "r.created_at DESC"
    };

    let requests: Vec<BatchRequestSummary> = sqlx::query_as(&format!(
        r#"
            SELECT
                r.id,
                r.batch_id,
                r.model,
                r.state,
                r.created_at,
                r.completed_at,
                r.failed_at,
                (CASE
                    WHEN r.completed_at IS NOT NULL AND r.started_at IS NOT NULL
                    THEN EXTRACT(EPOCH FROM (r.completed_at - r.started_at)) * 1000
                    ELSE NULL
                END)::float8 as duration_ms,
                r.response_status,
                ha.prompt_tokens,
                ha.completion_tokens,
                ha.reasoning_tokens,
                ha.total_tokens,
                ha.total_cost::float8 as total_cost,
                u.email as created_by_email
            FROM fusillade.requests r
            JOIN fusillade.batches b ON r.batch_id = b.id
            LEFT JOIN http_analytics ha ON ha.fusillade_request_id = r.id
            LEFT JOIN users u ON u.id::text = b.created_by
            WHERE b.deleted_at IS NULL
              AND ($1::text IS NULL OR b.created_by = $1)
              AND ($2::text IS NULL OR b.completion_window = $2)
              AND ($3::text IS NULL OR r.state = $3)
              AND ($4::text IS NULL OR r.model = $4)
              AND ($5::timestamptz IS NULL OR r.created_at >= $5)
              AND ($6::timestamptz IS NULL OR r.created_at <= $6)
            ORDER BY {order_clause}
            LIMIT $7 OFFSET $8
            "#
    ))
    .bind(created_by_filter.as_deref())
    .bind(query.completion_window.as_deref())
    .bind(query.status.as_deref())
    .bind(query.model.as_deref())
    .bind(query.created_after)
    .bind(query.created_before)
    .bind(limit)
    .bind(skip)
    .fetch_all(pool)
    .await
    .map_err(|e| Error::Database(e.into()))?;

    Ok(Json(PaginatedResponse::new(requests, total_count, skip, limit)))
}

/// Get individual batch request detail
///
/// Returns full request detail including the request body and response.
#[utoipa::path(
    get,
    path = "/admin/api/v1/batches/requests/{request_id}",
    params(
        ("request_id" = Uuid, Path, description = "The request ID"),
    ),
    responses(
        (status = 200, description = "Batch request detail", body = BatchRequestDetail),
        (status = 404, description = "Request not found"),
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
) -> Result<Json<BatchRequestDetail>> {
    let pool = state.db.read();

    let request: BatchRequestDetail = sqlx::query_as(
        r#"
        SELECT
            r.id,
            r.batch_id,
            r.model,
            r.state,
            r.created_at,
            r.completed_at,
            r.failed_at,
            (CASE
                WHEN r.completed_at IS NOT NULL AND r.started_at IS NOT NULL
                THEN EXTRACT(EPOCH FROM (r.completed_at - r.started_at)) * 1000
                ELSE NULL
            END)::float8 as duration_ms,
            r.response_status,
            ha.prompt_tokens,
            ha.completion_tokens,
            ha.reasoning_tokens,
            ha.total_tokens,
            ha.total_cost::float8 as total_cost,
            COALESCE(t.body, '') as body,
            r.response_body,
            r.error,
            b.completion_window,
            b.created_by as batch_created_by
        FROM fusillade.requests r
        JOIN fusillade.batches b ON r.batch_id = b.id
        LEFT JOIN fusillade.request_templates t ON r.template_id = t.id
        LEFT JOIN http_analytics ha ON ha.fusillade_request_id = r.id
        WHERE r.id = $1 AND b.deleted_at IS NULL
        "#,
    )
    .bind(request_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| Error::Database(e.into()))?
    .ok_or_else(|| Error::NotFound {
        resource: "BatchRequest".to_string(),
        id: request_id.to_string(),
    })?;

    // Check ownership
    let can_read_all = can_read_all_resources(&current_user, Resource::Batches);
    if !can_read_all && !is_batch_owner(&current_user, &request.batch_created_by) {
        return Err(Error::NotFound {
            resource: "BatchRequest".to_string(),
            id: request_id.to_string(),
        });
    }

    Ok(Json(request))
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
}
