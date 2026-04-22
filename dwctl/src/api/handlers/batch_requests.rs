//! Batch request handlers
//!
//! Endpoints for listing individual requests across fusillade batches.
//! Uses fusillade's Storage trait for request queries, enriches with
//! http_analytics data (tokens, cost) from the dwctl database.

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
/// Uses fusillade for core request listing, then enriches with token/cost
/// metrics from http_analytics and user emails from the users table.
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

    // Build ownership filter.
    // member_id is available in org context for any member, or in personal context for platform managers.
    let created_by_filter: Option<String> = if let Some(member_id) = query.member_id {
        if current_user.active_organization.is_none() && !can_read_all {
            return Err(Error::BadRequest {
                message: "member_id filter is only available in organization context or for platform managers".to_string(),
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
            completion_window: query.completion_window.clone(),
            status: query.status.clone(),
            models,
            created_after: query.created_after,
            created_before: query.created_before,
            service_tier: query.service_tier.clone(),
            active_first: query.active_first.unwrap_or(true),
            skip,
            limit,
        })
        .await
        .map_err(|e| Error::Internal {
            operation: format!("list batch requests: {}", e),
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
        .map(|r| r.batch_created_by.clone())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();

    let emails = if !unique_creator_ids.is_empty() {
        sqlx::query_as::<_, EmailRow>("SELECT id::text as user_id, email FROM users WHERE id::text = ANY($1)")
            .bind(&unique_creator_ids)
            .fetch_all(state.db.write())
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "failed to fetch creator emails for batch requests");
                vec![]
            })
    } else {
        vec![]
    };

    let email_map: std::collections::HashMap<String, String> = emails.into_iter().map(|e| (e.user_id, e.email)).collect();

    // Combine fusillade data with analytics enrichment
    let data: Vec<BatchRequestSummary> = result
        .data
        .into_iter()
        .map(|r| {
            let a = analytics_map.get(&r.id);
            let email = email_map.get(&r.batch_created_by).cloned();
            BatchRequestSummary {
                id: r.id,
                batch_id: r.batch_id,
                model: r.model,
                status: r.status,
                created_at: r.created_at,
                completed_at: r.completed_at,
                failed_at: r.failed_at,
                duration_ms: r.duration_ms,
                response_status: r.response_status,
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

/// Get individual batch request detail
///
/// Uses fusillade for core request detail, enriches with analytics.
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
    // Get core request detail from fusillade
    let detail = state
        .request_manager
        .get_request_detail(fusillade::RequestId(request_id))
        .await
        .map_err(|e| match e {
            fusillade::FusilladeError::RequestNotFound(_) => Error::NotFound {
                resource: "BatchRequest".to_string(),
                id: request_id.to_string(),
            },
            other => {
                tracing::error!(request_id = %request_id, error = %other, "failed to fetch batch request detail");
                Error::Internal {
                    operation: format!("get batch request detail: {}", other),
                }
            }
        })?;

    // Check ownership — fetch-then-check pattern matches get_batch handler.
    // The response is discarded on failure (returns 404, no data leakage).
    let can_read_all = can_read_all_resources(&current_user, Resource::Batches);
    if !can_read_all && !is_batch_owner(&current_user, &detail.batch_created_by) {
        return Err(Error::NotFound {
            resource: "BatchRequest".to_string(),
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
    // right after batch creation.
    let created_by_email = if let Ok(created_by_uuid) = Uuid::parse_str(&detail.batch_created_by) {
        sqlx::query_as::<_, EmailRow>("SELECT id::text as user_id, email FROM users WHERE id = $1")
            .bind(created_by_uuid)
            .fetch_optional(state.db.write())
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "failed to fetch creator email for batch request detail");
                None
            })
            .map(|row| row.email)
    } else {
        None
    };

    Ok(Json(BatchRequestDetail {
        id: detail.id,
        batch_id: detail.batch_id,
        model: detail.model,
        status: detail.status,
        created_at: detail.created_at,
        completed_at: detail.completed_at,
        failed_at: detail.failed_at,
        duration_ms: detail.duration_ms,
        response_status: detail.response_status,
        prompt_tokens: analytics.as_ref().and_then(|a| a.prompt_tokens),
        completion_tokens: analytics.as_ref().and_then(|a| a.completion_tokens),
        reasoning_tokens: analytics.as_ref().and_then(|a| a.reasoning_tokens),
        total_tokens: analytics.as_ref().and_then(|a| a.total_tokens),
        total_cost: analytics.as_ref().and_then(|a| a.total_cost),
        body: detail.body.unwrap_or_default(),
        response_body: detail.response_body,
        error: detail.error,
        completion_window: detail.completion_window,
        batch_created_by: detail.batch_created_by,
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
    use uuid::Uuid;

    async fn insert_batch_request(
        pool: &PgPool,
        created_by: Uuid,
        model: &str,
    ) -> Uuid {
        let file_id = Uuid::new_v4();
        let batch_id = Uuid::new_v4();
        let template_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();
        let body = serde_json::json!({"model": model, "messages": [{"role": "user", "content": "hi"}]});

        sqlx::query(
            "INSERT INTO fusillade.files (id, name, status, created_at, updated_at) VALUES ($1, 'test.jsonl', 'processed', NOW(), NOW())",
        )
        .bind(file_id)
        .execute(pool)
        .await
        .expect("insert file");

        sqlx::query(
            "INSERT INTO fusillade.batches (id, created_by, file_id, endpoint, completion_window, expires_at, created_at, total_requests) VALUES ($1, $2, $3, '/v1/chat/completions', '24h', NOW() + interval '24 hours', NOW(), 1)",
        )
        .bind(batch_id)
        .bind(created_by.to_string())
        .bind(file_id)
        .execute(pool)
        .await
        .expect("insert batch");

        sqlx::query(
            "INSERT INTO fusillade.request_templates (id, file_id, model, api_key, endpoint, path, body, custom_id, method) VALUES ($1, $2, $3, 'test-key', 'http://test', '/v1/chat/completions', $4, 'req-0', 'POST')",
        )
        .bind(template_id)
        .bind(file_id)
        .bind(model)
        .bind(serde_json::to_string(&body).unwrap())
        .execute(pool)
        .await
        .expect("insert template");

        sqlx::query(
            "INSERT INTO fusillade.requests (id, batch_id, template_id, model, state, created_at) VALUES ($1, $2, $3, $4, 'pending', NOW())",
        )
        .bind(request_id)
        .bind(batch_id)
        .bind(template_id)
        .bind(model)
        .execute(pool)
        .await
        .expect("insert request");

        request_id
    }

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
    async fn test_member_id_filter_rejected_outside_org_context_for_non_pm(pool: PgPool) {
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
    async fn test_member_id_filter_allowed_in_org_context_and_scopes_results(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;

        let owner = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let org = create_test_org(&pool, owner.id).await;
        let member_a = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let member_b = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        add_org_member(&pool, org.id, member_a.id, "member").await;
        add_org_member(&pool, org.id, member_b.id, "member").await;

        let request_a = insert_batch_request(&pool, member_a.id, "model-a").await;
        let _request_b = insert_batch_request(&pool, member_b.id, "model-b").await;

        let auth = add_auth_headers(&owner);
        let response = app
            .get(&format!(
                "/admin/api/v1/batches/requests?member_id={}",
                member_a.id
            ))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .add_header("X-Organization-Id", &org.id.to_string())
            .await;

        response.assert_status_ok();
        let body: serde_json::Value = response.json();
        let data = body["data"].as_array().expect("response data should be an array");
        assert_eq!(body["total_count"], 1);
        assert_eq!(data.len(), 1);
        assert_eq!(data[0]["id"], request_a.to_string());
        assert_eq!(data[0]["created_by_email"], member_a.email);
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

        let request_id = insert_batch_request(&pool, user.id, "test-model").await;

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
