// This file deals with the Batches API.
//! This is designed to match (as far as possible) the OpenAI Batches
//! [API](https://platform.openai.com/docs/api-reference/batch/).
//!
//! Repository methods are delegated to the fusillade/ crate.

use crate::AppState;
use crate::api::models::batches::{
    BatchAnalytics, BatchErrors, BatchListResponse, BatchObjectType, BatchResponse, CreateBatchRequest, ListBatchesQuery, ListObjectType,
    RequestCounts,
};
use crate::auth::permissions::{RequiresPermission, can_read_all_resources, has_permission, operation, resource};
use crate::errors::{Error, Result};
use crate::types::{Operation, Resource};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use fusillade::Storage;
use std::collections::HashMap;
use uuid::Uuid;

/// Fetch aggregated analytics metrics for a batch from the http_analytics table
async fn fetch_batch_analytics(pool: &sqlx::PgPool, request_manager: &impl fusillade::Storage, batch_id: Uuid) -> Result<BatchAnalytics> {
    // Get all request IDs for this batch from fusillade
    let requests = request_manager
        .get_batch_requests(fusillade::BatchId(batch_id))
        .await
        .map_err(|e| Error::Internal {
            operation: format!("get batch requests: {}", e),
        })?;

    // Extract request IDs
    let request_ids: Vec<Uuid> = requests.iter().map(|r| r.id().0).collect();

    // If no requests, return zero metrics
    if request_ids.is_empty() {
        return Ok(BatchAnalytics {
            total_requests: 0,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_tokens: 0,
            avg_duration_ms: None,
            avg_ttfb_ms: None,
            total_cost: None,
        });
    }

    // Query analytics for these specific request IDs
    let metrics = sqlx::query!(
        r#"
        SELECT
            COUNT(*) as "total_requests!",
            COALESCE(SUM(prompt_tokens), 0) as "total_prompt_tokens!",
            COALESCE(SUM(completion_tokens), 0) as "total_completion_tokens!",
            COALESCE(SUM(total_tokens), 0) as "total_tokens!",
            AVG(duration_ms) as "avg_duration_ms",
            AVG(duration_to_first_byte_ms) as "avg_ttfb_ms",
            SUM((prompt_tokens * COALESCE(input_price_per_token, 0)) +
                (completion_tokens * COALESCE(output_price_per_token, 0))) as "total_cost"
        FROM http_analytics
        WHERE fusillade_request_id = ANY($1)
        "#,
        &request_ids
    )
    .fetch_one(pool)
    .await
    .map_err(|e| Error::Internal {
        operation: format!("fetch analytics: {}", e),
    })?;

    Ok(BatchAnalytics {
        total_requests: metrics.total_requests,
        total_prompt_tokens: metrics.total_prompt_tokens.to_i64().unwrap_or(0),
        total_completion_tokens: metrics.total_completion_tokens.to_i64().unwrap_or(0),
        total_tokens: metrics.total_tokens.to_i64().unwrap_or(0),
        avg_duration_ms: metrics.avg_duration_ms.and_then(|d| d.to_f64()),
        avg_ttfb_ms: metrics.avg_ttfb_ms.and_then(|d| d.to_f64()),
        total_cost: metrics.total_cost.map(|d| d.to_string()),
    })
}

/// Helper function to convert fusillade Batch to OpenAI BatchResponse
fn to_batch_response(batch: fusillade::Batch) -> BatchResponse {
    // Convert metadata from serde_json::Value to HashMap<String, String>
    let metadata: Option<HashMap<String, String>> = batch.metadata.and_then(|m| {
        m.as_object().map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
    });

    // Determine OpenAI status from request counts
    let is_finished = batch.pending_requests == 0 && batch.in_progress_requests == 0;
    let openai_status = if batch.cancelling_at.is_some() {
        // If cancelling_at is set, check if batch is finished
        if is_finished {
            // All requests are in terminal state, batch is now cancelled
            "cancelled"
        } else {
            // Still cancelling
            "cancelling"
        }
    } else if batch.total_requests == 0 {
        "validating"
    } else if is_finished && batch.failed_requests == batch.total_requests {
        "failed"
    } else if is_finished {
        "completed"
    } else {
        // Any batch that has been validated (total_requests > 0) but not finished
        // is considered "in_progress". This includes:
        // - Batches actively processing (in_progress_requests > 0)
        // - Batches with some completed work (completed_requests > 0)
        // - Batches queued and waiting for capacity (only pending_requests > 0)
        "in_progress"
    };

    // Compute timestamps based on status
    let in_progress_at = if openai_status != "validating" {
        batch.requests_started_at.map(|dt| dt.timestamp())
    } else {
        None
    };

    // Terminal state timestamps from batch table
    let finalizing_at = batch.finalizing_at.map(|dt| dt.timestamp());
    let completed_at = batch.completed_at.map(|dt| dt.timestamp());
    let failed_at = batch.failed_at.map(|dt| dt.timestamp());
    let cancelled_at = batch.cancelled_at.map(|dt| dt.timestamp());

    // Parse errors from JSON if present
    let errors = batch.errors.and_then(|e| serde_json::from_value::<BatchErrors>(e).ok());

    // Check if batch has expired
    let expired_at = batch.expires_at.and_then(|expires| {
        if chrono::Utc::now() > expires {
            Some(expires.timestamp())
        } else {
            None
        }
    });

    BatchResponse {
        id: batch.id.0.to_string(),
        object_type: BatchObjectType::Batch,
        endpoint: batch.endpoint.clone(),
        errors,
        input_file_id: batch.file_id.0.to_string(),
        completion_window: batch.completion_window.clone(),
        status: openai_status.to_string(),
        output_file_id: batch.output_file_id.map(|id| id.0.to_string()),
        error_file_id: batch.error_file_id.map(|id| id.0.to_string()),
        created_at: batch.created_at.timestamp(),
        in_progress_at,
        expires_at: batch.expires_at.map(|dt| dt.timestamp()),
        finalizing_at,
        completed_at,
        failed_at,
        expired_at,
        cancelling_at: batch.cancelling_at.map(|dt| dt.timestamp()),
        cancelled_at,
        request_counts: RequestCounts {
            total: batch.total_requests,
            completed: batch.completed_requests,
            failed: batch.failed_requests,
        },
        metadata,
    }
}

#[utoipa::path(
    post,
    path = "/batches",
    tag = "batches",
    summary = "Create batch",
    description = "Creates and executes a batch from an uploaded file of requests",
    request_body = CreateBatchRequest,
    responses(
        (status = 201, description = "Batch created successfully", body = BatchResponse),
        (status = 400, description = "Invalid request"),
        (status = 404, description = "Input file not found"),
        (status = 500, description = "Internal server error")
    )
)]
#[tracing::instrument(skip(state, current_user), fields(user_id = %current_user.id, input_file_id = %req.input_file_id))]
pub async fn create_batch(
    State(state): State<AppState>,
    current_user: RequiresPermission<resource::Batches, operation::CreateOwn>,
    Json(req): Json<CreateBatchRequest>,
) -> Result<(StatusCode, Json<BatchResponse>)> {
    // Validate completion_window
    if req.completion_window != "24h" {
        return Err(Error::BadRequest {
            message: "Only '24h' completion_window is currently supported".to_string(),
        });
    }

    // Validate endpoint
    let supported_endpoints = ["/v1/chat/completions", "/v1/completions", "/v1/embeddings", "/v1/moderations"];
    if !supported_endpoints.contains(&req.endpoint.as_str()) {
        return Err(Error::BadRequest {
            message: format!(
                "Unsupported endpoint '{}'. Supported: {}",
                req.endpoint,
                supported_endpoints.join(", ")
            ),
        });
    }

    // Parse file ID
    let file_id = Uuid::parse_str(&req.input_file_id).map_err(|_| Error::BadRequest {
        message: "Invalid input_file_id format".to_string(),
    })?;

    // Verify file exists and user has access
    let file = state
        .request_manager
        .get_file(fusillade::FileId(file_id))
        .await
        .map_err(|_| Error::NotFound {
            resource: "File".to_string(),
            id: req.input_file_id.clone(),
        })?;

    // Check file ownership if user doesn't have ReadAll permission
    use crate::types::Resource;
    let has_read_all = can_read_all_resources(&current_user, Resource::Files);
    if !has_read_all {
        // Verify user owns the file
        let user_id_str = current_user.id.to_string();
        if file.uploaded_by.as_deref() != Some(&user_id_str) {
            use crate::types::{Operation, Permission};
            return Err(Error::InsufficientPermissions {
                required: Permission::Allow(Resource::Files, Operation::ReadAll),
                action: Operation::CreateOwn,
                resource: format!("batch using file {}", req.input_file_id),
            });
        }
    }

    // Convert metadata to serde_json::Value
    let metadata = req.metadata.and_then(|m| serde_json::to_value(m).ok());

    // Create batch input
    let batch_input = fusillade::BatchInput {
        file_id: fusillade::FileId(file_id),
        endpoint: req.endpoint.clone(),
        completion_window: req.completion_window.clone(),
        metadata,
        created_by: Some(current_user.id.to_string()),
    };

    // Create the batch
    let batch = state.request_manager.create_batch(batch_input).await.map_err(|e| Error::Internal {
        operation: format!("create batch: {}", e),
    })?;

    tracing::info!("Batch {} created successfully", batch.id);

    Ok((StatusCode::CREATED, Json(to_batch_response(batch))))
}

#[utoipa::path(
    get,
    path = "/batches/{batch_id}",
    tag = "batches",
    summary = "Retrieve batch",
    description = "Retrieves a batch by ID",
    responses(
        (status = 200, description = "Batch retrieved successfully", body = BatchResponse),
        (status = 404, description = "Batch not found"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("batch_id" = String, Path, description = "The ID of the batch to retrieve")
    )
)]
#[tracing::instrument(skip(state, current_user), fields(user_id = %current_user.id, batch_id = %batch_id_str))]
pub async fn get_batch(
    State(state): State<AppState>,
    Path(batch_id_str): Path<String>,
    current_user: RequiresPermission<resource::Batches, operation::ReadOwn>,
) -> Result<Json<BatchResponse>> {
    let batch_id = Uuid::parse_str(&batch_id_str).map_err(|_| Error::BadRequest {
        message: "Invalid batch ID format".to_string(),
    })?;

    // Get batch
    let batch = state
        .request_manager
        .get_batch(fusillade::BatchId(batch_id))
        .await
        .map_err(|_| Error::NotFound {
            resource: "Batch".to_string(),
            id: batch_id_str.clone(),
        })?;

    // Check ownership: users without ReadAll permission can only see their own batches
    let can_read_all = can_read_all_resources(&current_user, Resource::Batches);
    if !can_read_all {
        let user_id = current_user.id.to_string();
        if batch.created_by.as_deref() != Some(user_id.as_str()) {
            return Err(Error::NotFound {
                resource: "Batch".to_string(),
                id: batch_id_str,
            });
        }
    }

    Ok(Json(to_batch_response(batch)))
}

#[utoipa::path(
    get,
    path = "/batches/{batch_id}/analytics",
    tag = "batches",
    summary = "Get batch analytics",
    description = "Retrieves aggregated analytics metrics for a batch, including token usage, costs, and performance metrics",
    responses(
        (status = 200, description = "Batch analytics retrieved successfully", body = BatchAnalytics),
        (status = 404, description = "Batch not found or no analytics available"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("batch_id" = String, Path, description = "The ID of the batch to retrieve analytics for")
    )
)]
#[tracing::instrument(skip(state, current_user), fields(user_id = %current_user.id, batch_id = %batch_id_str))]
pub async fn get_batch_analytics(
    State(state): State<AppState>,
    Path(batch_id_str): Path<String>,
    current_user: RequiresPermission<resource::Batches, operation::ReadOwn>,
) -> Result<Json<BatchAnalytics>> {
    let batch_id = Uuid::parse_str(&batch_id_str).map_err(|_| Error::BadRequest {
        message: "Invalid batch ID format".to_string(),
    })?;

    // Get batch first to verify it exists and check permissions
    let batch = state
        .request_manager
        .get_batch(fusillade::BatchId(batch_id))
        .await
        .map_err(|_| Error::NotFound {
            resource: "Batch".to_string(),
            id: batch_id_str.clone(),
        })?;

    // Check ownership: users without ReadAll permission can only see their own batches
    let can_read_all = can_read_all_resources(&current_user, Resource::Batches);
    if !can_read_all {
        let user_id = current_user.id.to_string();
        if batch.created_by.as_deref() != Some(user_id.as_str()) {
            return Err(Error::NotFound {
                resource: "Batch".to_string(),
                id: batch_id_str.clone(),
            });
        }
    }

    // Fetch aggregated analytics metrics for this batch
    let analytics = fetch_batch_analytics(&state.db, &*state.request_manager, batch_id).await?;

    Ok(Json(analytics))
}

#[utoipa::path(
    post,
    path = "/batches/{batch_id}/cancel",
    tag = "batches",
    summary = "Cancel batch",
    description = "Cancels an in-progress batch",
    responses(
        (status = 200, description = "Batch cancellation initiated", body = BatchResponse),
        (status = 404, description = "Batch not found"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("batch_id" = String, Path, description = "The ID of the batch to cancel")
    )
)]
#[tracing::instrument(skip(state, current_user), fields(user_id = %current_user.id, batch_id = %batch_id_str))]
pub async fn cancel_batch(
    State(state): State<AppState>,
    Path(batch_id_str): Path<String>,
    current_user: RequiresPermission<resource::Batches, operation::UpdateOwn>,
) -> Result<Json<BatchResponse>> {
    let batch_id = Uuid::parse_str(&batch_id_str).map_err(|_| Error::BadRequest {
        message: "Invalid batch ID format".to_string(),
    })?;

    // Get batch first to verify it exists
    let batch = state
        .request_manager
        .get_batch(fusillade::BatchId(batch_id))
        .await
        .map_err(|_| Error::NotFound {
            resource: "Batch".to_string(),
            id: batch_id_str.clone(),
        })?;

    // Check ownership: users without UpdateAll permission can only cancel their own batches
    let can_update_all = has_permission(&current_user, Resource::Batches, Operation::UpdateAll);
    if !can_update_all {
        let user_id = current_user.id.to_string();
        if batch.created_by.as_deref() != Some(user_id.as_str()) {
            return Err(Error::NotFound {
                resource: "Batch".to_string(),
                id: batch_id_str.clone(),
            });
        }
    }

    // Cancel the batch
    state
        .request_manager
        .cancel_batch(fusillade::BatchId(batch_id))
        .await
        .map_err(|e| Error::Internal {
            operation: format!("cancel batch: {}", e),
        })?;

    // Fetch updated batch to get latest status
    let batch = state
        .request_manager
        .get_batch(fusillade::BatchId(batch_id))
        .await
        .map_err(|_| Error::NotFound {
            resource: "Batch".to_string(),
            id: batch_id_str.clone(),
        })?;

    tracing::info!("Batch {} cancelled", batch_id);

    Ok(Json(to_batch_response(batch)))
}

#[utoipa::path(
    get,
    path = "/batches",
    tag = "batches",
    summary = "List batches",
    description = "Returns a list of batches",
    responses(
        (status = 200, description = "List of batches", body = BatchListResponse),
        (status = 500, description = "Internal server error")
    ),
    params(
        ListBatchesQuery
    )
)]
#[tracing::instrument(skip(state, current_user), fields(user_id = %current_user.id, limit = ?query.pagination.limit, after = ?query.pagination.after))]
pub async fn list_batches(
    State(state): State<AppState>,
    Query(query): Query<ListBatchesQuery>,
    current_user: RequiresPermission<resource::Batches, operation::ReadOwn>,
) -> Result<Json<BatchListResponse>> {
    let limit = query.pagination.limit();

    // Parse the after cursor if provided
    let after = query
        .pagination
        .after
        .as_ref()
        .and_then(|after_str| Uuid::parse_str(after_str).ok().map(fusillade::BatchId));

    // Determine if user can read all batches or just their own
    let can_read_all = can_read_all_resources(&current_user, Resource::Batches);
    let created_by = if can_read_all { None } else { Some(current_user.id.to_string()) };

    // Fetch batches with ownership filtering and cursor-based pagination
    let batches = state
        .request_manager
        .list_batches(created_by, after, limit + 1) // Fetch one extra to determine has_more
        .await
        .map_err(|e| Error::Internal {
            operation: format!("list batches: {}", e),
        })?;

    // Check if there are more results
    let has_more = batches.len() > limit as usize;
    let batches: Vec<_> = batches.into_iter().take(limit as usize).collect();

    // Get first and last IDs
    let first_id = batches.first().map(|b| b.id.0.to_string());
    let last_id = batches.last().map(|b| b.id.0.to_string());

    // Convert batches to responses (status is embedded in batch)
    let data: Vec<_> = batches.into_iter().map(to_batch_response).collect();

    Ok(Json(BatchListResponse {
        object_type: ListObjectType::List,
        data,
        first_id,
        last_id,
        has_more,
    }))
}
