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
    http::StatusCode,
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
    auth::permissions::{RequiresPermission, can_delete_all_resources, can_read_all_resources, operation, resource},
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

    // Build ownership filter — member_id only allowed for users with ReadAll permission.
    //
    // Branch order matters: when an org is active, scope to that org even for
    // platform managers. The old order checked `can_read_all` first, so a PM
    // in org context fell through to `created_by_filter = None` and saw all
    // rows across all users and orgs in the Responses view. Mirrors the logic
    // in `batches.rs::list_batches` so both views stay consistent.
    let created_by_filter: Option<String> = if let Some(member_id) = query.member_id {
        if !can_read_all {
            return Err(Error::BadRequest {
                message: "member_id filter requires platform manager permissions".to_string(),
            });
        }
        Some(member_id.to_string())
    } else if let Some(org_id) = current_user.active_organization {
        Some(org_id.to_string())
    } else if can_read_all {
        None
    } else {
        Some(current_user.id.to_string())
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

    // Enrich with per-request token counts (best-effort, from http_analytics) and cost
    // (durable, from the credits ledger by fusillade_request_id — COR-524 follow-up) plus
    // creator emails.
    let request_ids: Vec<Uuid> = result.data.iter().map(|r| r.id).collect();

    let analytics = if !request_ids.is_empty() {
        sqlx::query_as::<_, AnalyticsRow>(
            r#"
            SELECT DISTINCT ON (fusillade_request_id)
                fusillade_request_id as request_id,
                prompt_tokens,
                completion_tokens,
                reasoning_tokens,
                total_tokens
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

    // Cost from the durable credits ledger, keyed by the denormalized fusillade_request_id.
    let costs = if !request_ids.is_empty() {
        sqlx::query_as::<_, CostRow>(
            r#"
            SELECT DISTINCT ON (fusillade_request_id)
                fusillade_request_id as request_id,
                amount::float8 as total_cost
            FROM credits_transactions
            WHERE fusillade_request_id = ANY($1) AND transaction_type = 'usage'
            ORDER BY fusillade_request_id, seq DESC
            "#,
        )
        .bind(&request_ids)
        .fetch_all(state.db.read())
        .await
        .map_err(|e| Error::Database(e.into()))?
    } else {
        vec![]
    };
    let cost_map: std::collections::HashMap<Uuid, f64> =
        costs.into_iter().filter_map(|c| c.total_cost.map(|v| (c.request_id, v))).collect();

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
                total_cost: cost_map.get(&r.id).copied(),
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

    // Token counts from http_analytics (best-effort; age out with retention).
    let analytics = sqlx::query_as::<_, AnalyticsRow>(
        r#"
        SELECT DISTINCT ON (fusillade_request_id)
            fusillade_request_id as request_id,
            prompt_tokens,
            completion_tokens,
            reasoning_tokens,
            total_tokens
        FROM http_analytics
        WHERE fusillade_request_id = $1
        ORDER BY fusillade_request_id, timestamp DESC
        "#,
    )
    .bind(request_id)
    .fetch_optional(state.db.read())
    .await
    .map_err(|e| Error::Database(e.into()))?;

    // Cost from the durable credits ledger (by denormalized fusillade_request_id).
    let cost = sqlx::query_as::<_, CostRow>(
        r#"
        SELECT DISTINCT ON (fusillade_request_id)
            fusillade_request_id as request_id,
            amount::float8 as total_cost
        FROM credits_transactions
        WHERE fusillade_request_id = $1 AND transaction_type = 'usage'
        ORDER BY fusillade_request_id, seq DESC
        "#,
    )
    .bind(request_id)
    .fetch_optional(state.db.write())
    .await
    .map_err(|e| Error::Database(e.into()))?
    .and_then(|c| c.total_cost);

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

    // ZDR bodies are stored as `dwzdr1:` ciphertext (and a failed request's
    // error carries an encrypted body inside it). This detail view is a plain
    // read - no keystore, so it never decrypts or shreds, and a dashboard GET
    // cannot disturb the consumer's one-shot `/v1/responses/{id}` retrieval.
    // Blank the ciphertext-bearing fields rather than surface the envelope; the
    // request is ZDR iff its stored request body is one.
    let is_zdr = detail.body.as_deref().is_some_and(crate::inference::zdr::is_zdr_body);

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
        total_cost: cost,
        body: if is_zdr { String::new() } else { detail.body.unwrap_or_default() },
        response_body: if is_zdr { None } else { detail.response_body },
        error: if is_zdr { None } else { detail.error },
        created_by: detail.created_by,
        created_by_email,
    }))
}

/// Delete a response (batchless fusillade request) by ID.
///
/// Hard-deletes for right-to-erasure compliance. The fusillade primitive
/// (`Storage::delete_request`) removes:
/// * the `requests` row,
/// * its dedicated batchless `request_templates` row (carrying the prompt
///   body — 1:1 with the request when `file_id IS NULL`), and
/// * all `response_steps` whose `request_id` matches (via FK cascade).
///
/// **Preserved**: `http_analytics` (token counts, cost, status code — no FK
/// to requests) and `credits_transactions` (immutable; denormalizes
/// `fusillade_batch_id` only, not `fusillade_request_id`). Billing and
/// usage records survive the erasure.
///
/// Multi-step Open Responses (with a step tree) are deleted via
/// `DELETE /ai/v1/responses/{id}` instead — that handler walks the chain
/// and calls this primitive for every backing sub-request.
#[utoipa::path(
    delete,
    path = "/admin/api/v1/batches/requests/{request_id}",
    params(
        ("request_id" = Uuid, Path, description = "The request ID"),
    ),
    responses(
        (status = 204, description = "Response deleted"),
        (status = 404, description = "Response not found"),
        (status = 403, description = "Insufficient permissions"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "batch_requests",
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id, request_id = %request_id))]
pub async fn delete_batch_request<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(request_id): Path<Uuid>,
    current_user: CurrentUser,
    _: RequiresPermission<resource::Batches, operation::DeleteOwn>,
) -> Result<StatusCode> {
    // Fetch first to verify existence + ownership. `get_request_detail` filters
    // batched rows (WHERE r.created_by IS NOT NULL), so we can't accidentally
    // delete a row that belongs to a batch via this endpoint.
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
                tracing::error!(request_id = %request_id, error = %other, "failed to fetch response for delete");
                Error::Internal {
                    operation: format!("get response for delete: {}", other),
                }
            }
        })?;

    // 404 (not 403) avoids leaking existence of other users' responses.
    let can_delete_all = can_delete_all_resources(&current_user, Resource::Batches);
    if !can_delete_all && !is_batch_owner(&current_user, &detail.created_by) {
        return Err(Error::NotFound {
            resource: "Response".to_string(),
            id: request_id.to_string(),
        });
    }

    state
        .request_manager
        .delete_request(fusillade::RequestId(request_id))
        .await
        .map_err(|e| match e {
            fusillade::FusilladeError::RequestNotFound(_) => Error::NotFound {
                resource: "Response".to_string(),
                id: request_id.to_string(),
            },
            other => Error::Internal {
                operation: format!("delete response: {}", other),
            },
        })?;

    Ok(StatusCode::NO_CONTENT)
}

/// Per-request token counts from http_analytics (dwctl-owned, not fusillade schema).
/// Best-effort: these age out with http_analytics retention, so a response older than the
/// retention window shows empty token counts. Cost is read separately off the durable ledger.
#[derive(sqlx::FromRow)]
struct AnalyticsRow {
    #[allow(dead_code)]
    request_id: Uuid,
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    total_tokens: Option<i64>,
}

/// Per-request cost from the credits ledger, keyed by the denormalized fusillade_request_id
/// (migration 119). Durable — survives http_analytics retention, unlike the token counts above.
#[derive(sqlx::FromRow)]
struct CostRow {
    request_id: Uuid,
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

    /// Regression test for the PM-in-org-context scoping bug fixed alongside
    /// this test. The bug: `list_batch_requests` checked `can_read_all`
    /// before the org-context arm, so a platform manager who had switched
    /// into an org context still got `created_by_filter = None` and saw
    /// rows from every user and every org. The fix reorders the branches
    /// to match `batches.rs::list_batches` — the org context arm now wins.
    ///
    /// To trigger the bug we need three populated `created_by` values and a
    /// PM caller:
    ///   - the PM themselves (personal-context batch)
    ///   - a different user (cross-user noise)
    ///   - the org the PM has switched into
    ///
    /// With the fix:
    ///   - PM personal context → all three rows (PM bypass, intentional)
    ///   - PM in org context   → only the org's row (the contract we're testing)
    #[sqlx::test]
    #[test_log::test]
    async fn test_list_batch_requests_pm_in_org_context_scopes_to_org(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;

        let pm_user = create_test_user_with_roles(&pool, vec![Role::PlatformManager, Role::StandardUser, Role::BatchAPIUser]).await;
        let other_user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let org = create_test_org(&pool, pm_user.id).await;
        let auth = add_auth_headers(&pm_user);

        // `list_batch_requests` powers the Responses view — fusillade's
        // `list_requests` returns *batchless* rows only (`created_by`
        // populated directly on the request, `batch_id IS NULL`). Mirror
        // the seed pattern from
        // `test_get_batch_request_populates_created_by_email` so we hit
        // the same code path the production view actually drives.
        let body = serde_json::json!({"model": "test-model", "messages": [{"role": "user", "content": "hi"}]});
        let owners = [
            ("pm_user", pm_user.id.to_string()),
            ("other_user", other_user.id.to_string()),
            ("org", org.id.to_string()),
        ];
        for (_label, owner) in &owners {
            let template_id = uuid::Uuid::new_v4();
            let request_id = uuid::Uuid::new_v4();
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
            .bind(owner)
            .execute(&pool)
            .await
            .expect("insert request");
        }

        // PM in personal context — `can_read_all` wins, no filter, all three
        // rows visible. This guards the intentional PM-bypass behavior.
        let personal_resp = app
            .get("/admin/api/v1/batches/requests")
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;
        personal_resp.assert_status_ok();
        let personal_body: serde_json::Value = personal_resp.json();
        assert_eq!(
            personal_body["total_count"], 3,
            "PM in personal context should see every row (read-all bypass)",
        );

        // PM in org context — `created_by_filter = Some(org.id)`, only the
        // org's row visible. This is the contract that regressed before
        // the fix; if branch ordering ever flips again this assertion fails.
        let org_cookie = format!("dw_active_org={}", org.id);
        let org_resp = app
            .get("/admin/api/v1/batches/requests")
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .add_header("cookie", &org_cookie)
            .await;
        org_resp.assert_status_ok();
        let org_body: serde_json::Value = org_resp.json();
        // Three rows were seeded, one per `created_by`. The org filter
        // queries `WHERE created_by = $1`, so getting exactly 1 row back
        // is sufficient evidence that the filter applied and the right
        // owner was picked — no need to spelunk into the response model's
        // creator-identity field.
        assert_eq!(org_body["total_count"], 1, "PM in org context should see only the org's row",);
        let org_rows = org_body["data"].as_array().expect("data array");
        assert_eq!(org_rows.len(), 1);
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

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_batch_request_removes_row(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let auth = add_auth_headers(&user);

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
            .delete(&format!("/admin/api/v1/batches/requests/{}", request_id))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;
        response.assert_status(axum::http::StatusCode::NO_CONTENT);

        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM fusillade.requests WHERE id = $1")
            .bind(request_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 0, "fusillade row should be hard-deleted");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_batch_request_404_for_other_users_row(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let intruder = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let intruder_auth = add_auth_headers(&intruder);

        let template_id = uuid::Uuid::new_v4();
        let request_id = uuid::Uuid::new_v4();
        let body = serde_json::json!({"model": "test-model", "messages": []});

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
        .bind(owner.id.to_string())
        .execute(&pool)
        .await
        .expect("insert request");

        let response = app
            .delete(&format!("/admin/api/v1/batches/requests/{}", request_id))
            .add_header(&intruder_auth[0].0, &intruder_auth[0].1)
            .add_header(&intruder_auth[1].0, &intruder_auth[1].1)
            .await;
        response.assert_status_not_found();

        // Row is still there — intruder couldn't delete it.
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM fusillade.requests WHERE id = $1")
            .bind(request_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 1, "row should remain — ownership check should reject");
    }
}
