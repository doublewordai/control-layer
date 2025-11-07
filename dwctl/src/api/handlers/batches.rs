// This file deals with the Batches API.
//! This is designed to match (as far as possible) the OpenAI Batches
//! [API](https://platform.openai.com/docs/api-reference/batch/).
//!
//! Repository methods are delegated to the fusillade/ crate.

use crate::api::models::batches::{
    BatchListResponse, BatchObjectType, BatchResponse, CreateBatchRequest, ListBatchesQuery,
    ListObjectType, RequestCounts,
};
use crate::auth::permissions::{operation, resource, RequiresPermission};
use crate::errors::{Error, Result};
use crate::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use fusillade::Storage;
use std::collections::HashMap;
use uuid::Uuid;

/// Helper function to convert fusillade Batch + BatchStatus to OpenAI BatchResponse
fn to_batch_response(
    batch: fusillade::Batch,
    status: fusillade::BatchStatus,
) -> BatchResponse {
    // Convert metadata from serde_json::Value to HashMap<String, String>
    let metadata: Option<HashMap<String, String>> = batch.metadata.and_then(|m| {
        m.as_object().map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
    });

    // Compute timestamps based on status
    let openai_status = status.openai_status();
    let in_progress_at = if openai_status != "validating" {
        status.started_at.map(|dt| dt.timestamp())
    } else {
        None
    };

    let finalizing_at = if openai_status == "finalizing" || openai_status == "completed" {
        status.last_updated_at.map(|dt| dt.timestamp())
    } else {
        None
    };

    let completed_at = if openai_status == "completed" {
        status.last_updated_at.map(|dt| dt.timestamp())
    } else {
        None
    };

    let failed_at = if openai_status == "failed" {
        status.last_updated_at.map(|dt| dt.timestamp())
    } else {
        None
    };

    let cancelled_at = if openai_status == "cancelled" {
        status.last_updated_at.map(|dt| dt.timestamp())
    } else {
        None
    };

    BatchResponse {
        id: batch.id.0.to_string(),
        object_type: BatchObjectType::Batch,
        endpoint: batch.endpoint.clone(),
        errors: None, // TODO: Implement error tracking
        input_file_id: batch.file_id.0.to_string(),
        completion_window: batch.completion_window.clone(),
        status: openai_status.to_string(),
        output_file_id: batch.output_file_id.map(|id| id.0.to_string()),
        error_file_id: batch.error_file_id.map(|id| id.0.to_string()),
        created_at: batch.created_at.timestamp(),
        in_progress_at,
        expires_at: None, // TODO: Calculate expiration based on completion_window
        finalizing_at,
        completed_at,
        failed_at,
        expired_at: None,
        cancelling_at: None, // TODO: Track cancelling state
        cancelled_at,
        request_counts: RequestCounts {
            total: status.total_requests,
            completed: status.completed_requests,
            failed: status.failed_requests,
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
    let supported_endpoints = vec![
        "/v1/chat/completions",
        "/v1/completions",
        "/v1/embeddings",
        "/v1/moderations",
    ];
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
    let _file = state
        .request_manager
        .get_file(fusillade::FileId(file_id))
        .await
        .map_err(|_| Error::NotFound {
            resource: "File".to_string(),
            id: req.input_file_id.clone(),
        })?;

    // TODO: Check file ownership if user doesn't have ReadAll permission

    // Convert metadata to serde_json::Value
    let metadata = req
        .metadata
        .map(|m| serde_json::to_value(m).ok())
        .flatten();

    // Create batch input
    let batch_input = fusillade::BatchInput {
        file_id: fusillade::FileId(file_id),
        endpoint: req.endpoint.clone(),
        completion_window: req.completion_window.clone(),
        metadata,
    };

    // Create the batch
    let batch = state
        .request_manager
        .create_batch(batch_input)
        .await
        .map_err(|e| Error::Internal {
            operation: format!("create batch: {}", e),
        })?;

    // Get batch status
    let status = state
        .request_manager
        .get_batch_status(batch.id)
        .await
        .map_err(|e| Error::Internal {
            operation: format!("get batch status: {}", e),
        })?;

    tracing::info!("Batch {} created successfully", batch.id);

    Ok((StatusCode::CREATED, Json(to_batch_response(batch, status))))
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

    // TODO: Check batch ownership if user doesn't have ReadAll permission

    // Get batch status
    let status = state
        .request_manager
        .get_batch_status(fusillade::BatchId(batch_id))
        .await
        .map_err(|e| Error::Internal {
            operation: format!("get batch status: {}", e),
        })?;

    Ok(Json(to_batch_response(batch, status)))
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

    // TODO: Check batch ownership if user doesn't have UpdateAll permission

    // Cancel the batch
    state
        .request_manager
        .cancel_batch(fusillade::BatchId(batch_id))
        .await
        .map_err(|e| Error::Internal {
            operation: format!("cancel batch: {}", e),
        })?;

    // Get updated status
    let status = state
        .request_manager
        .get_batch_status(fusillade::BatchId(batch_id))
        .await
        .map_err(|e| Error::Internal {
            operation: format!("get batch status: {}", e),
        })?;

    tracing::info!("Batch {} cancelled", batch_id);

    Ok(Json(to_batch_response(batch, status)))
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
#[tracing::instrument(skip(_state, current_user), fields(user_id = %current_user.id, limit = ?query.limit, after = ?query.after))]
pub async fn list_batches(
    State(_state): State<AppState>,
    Query(query): Query<ListBatchesQuery>,
    current_user: RequiresPermission<resource::Batches, operation::ReadOwn>,
) -> Result<Json<BatchListResponse>> {
    let _limit = query.limit.unwrap_or(20).clamp(1, 100);

    // TODO: Implement proper pagination with cursor
    // TODO: Filter by ownership if user doesn't have ReadAll permission

    // For now, get all batches (this is inefficient for large datasets)
    // In production, you'd want a proper list_batches method with filtering
    // We'll need to add this to the Storage trait

    // Temporary workaround: Since we don't have a list_batches method yet,
    // return an empty list. This needs to be implemented properly.
    let data = vec![];
    let has_more = false;

    Ok(Json(BatchListResponse {
        object_type: ListObjectType::List,
        data,
        first_id: None,
        last_id: None,
        has_more,
    }))
}
