// This file deals with the Batches API.
//! This is designed to match (as far as possible) the OpenAI Batches
//! [API](https://platform.openai.com/docs/api-reference/batch/).
//!
//! Repository methods are delegated to the fusillade/ crate.

use crate::api::models::batches::{
    BatchErrors, BatchListResponse, BatchObjectType, BatchResponse, CreateBatchRequest, ListBatchesQuery, ListObjectType, RequestCounts,
};
use crate::auth::permissions::{can_read_all_resources, has_permission, operation, resource, RequiresPermission};
use crate::errors::{Error, Result};
use crate::types::{Operation, Resource};
use crate::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use fusillade::Storage;
use std::collections::HashMap;
use uuid::Uuid;

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
    } else if batch.in_progress_requests > 0 || batch.completed_requests > 0 {
        "in_progress"
    } else {
        "validating"
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::files::FileResponse;
    use crate::api::models::users::Role;
    use crate::test_utils::{
        add_auth_headers, add_deployment_to_group, add_user_to_group, create_test_app, create_test_deployment, create_test_group,
        create_test_user_with_roles,
    };
    use axum::http::StatusCode;
    use sqlx::PgPool;

    #[sqlx::test]
    async fn test_create_batch_success(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create a deployment and add to group so user has access to the model
        let deployment = create_test_deployment(&pool, user.id, "gpt-3.5-turbo", "gpt-3.5-turbo").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // First, upload a file for the batch
        let file_content = r#"{"custom_id": "request-1", "method": "POST", "url": "/v1/chat/completions", "body": {"model": "gpt-3.5-turbo", "messages": [{"role": "user", "content": "Hello"}]}}"#;
        let file_part = axum_test::multipart::Part::bytes(file_content.as_bytes()).file_name("test.jsonl");

        let headers = add_auth_headers(&user);
        let upload_response = app
            .post("/ai/v1/files")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_text("purpose", "batch")
                    .add_part("file", file_part),
            )
            .await;

        upload_response.assert_status(StatusCode::CREATED);
        let file: FileResponse = upload_response.json();

        // Create batch
        let create_req = CreateBatchRequest {
            input_file_id: file.id.clone(),
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
        };

        let response = {
            let headers = add_auth_headers(&user);
            app.post("/ai/v1/batches")
                .json(&create_req)
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
        }
        .await;

        response.assert_status(StatusCode::CREATED);
        let batch: BatchResponse = response.json();
        assert_eq!(batch.endpoint, "/v1/chat/completions");
        assert_eq!(batch.completion_window, "24h");
        assert_eq!(batch.input_file_id, file.id);
    }

    #[sqlx::test]
    async fn test_create_batch_invalid_completion_window(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        let create_req = CreateBatchRequest {
            input_file_id: "file-123".to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "48h".to_string(), // Invalid
            metadata: None,
        };

        let response = {
            let headers = add_auth_headers(&user);
            app.post("/ai/v1/batches")
                .json(&create_req)
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
        }
        .await;

        response.assert_status(StatusCode::BAD_REQUEST);
        let body = response.text();
        assert!(body.contains("24h"));
    }

    #[sqlx::test]
    async fn test_create_batch_unsupported_endpoint(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        let create_req = CreateBatchRequest {
            input_file_id: "file-123".to_string(),
            endpoint: "/v1/images/generations".to_string(), // Unsupported
            completion_window: "24h".to_string(),
            metadata: None,
        };

        let response = {
            let headers = add_auth_headers(&user);
            app.post("/ai/v1/batches")
                .json(&create_req)
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
        }
        .await;

        response.assert_status(StatusCode::BAD_REQUEST);
        let body = response.text();
        assert!(body.contains("Unsupported endpoint"));
    }

    #[sqlx::test]
    async fn test_create_batch_invalid_file_id_format(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        let create_req = CreateBatchRequest {
            input_file_id: "not-a-uuid".to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
        };

        let response = {
            let headers = add_auth_headers(&user);
            app.post("/ai/v1/batches")
                .json(&create_req)
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
        }
        .await;

        response.assert_status(StatusCode::BAD_REQUEST);
        let body = response.text();
        assert!(body.contains("Invalid input_file_id format"));
    }

    #[sqlx::test]
    async fn test_create_batch_file_not_found(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        let create_req = CreateBatchRequest {
            input_file_id: uuid::Uuid::new_v4().to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
        };

        let response = {
            let headers = add_auth_headers(&user);
            app.post("/ai/v1/batches")
                .json(&create_req)
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
        }
        .await;

        response.assert_status(StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    async fn test_get_batch_success(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let deployment = create_test_deployment(&pool, user.id, "gpt-3.5-turbo", "gpt-3.5-turbo").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Upload file and create batch
        let file_content = r#"{"custom_id": "request-1", "method": "POST", "url": "/v1/chat/completions", "body": {"model": "gpt-3.5-turbo", "messages": [{"role": "user", "content": "Hello"}]}}"#;
        let file_part = axum_test::multipart::Part::bytes(file_content.as_bytes()).file_name("test.jsonl");

        let headers = add_auth_headers(&user);
        let upload_response = app
            .post("/ai/v1/files")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_text("purpose", "batch")
                    .add_part("file", file_part),
            )
            .await;

        let file: FileResponse = upload_response.json();

        let create_req = CreateBatchRequest {
            input_file_id: file.id,
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
        };

        let create_response = {
            let headers = add_auth_headers(&user);
            app.post("/ai/v1/batches")
                .json(&create_req)
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
        }
        .await;

        let batch: BatchResponse = create_response.json();

        // Get batch
        let response = {
            let headers = add_auth_headers(&user);
            app.get(&format!("/ai/v1/batches/{}", batch.id))
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
        }
        .await;

        response.assert_status(StatusCode::OK);
        let retrieved_batch: BatchResponse = response.json();
        assert_eq!(retrieved_batch.id, batch.id);
        assert_eq!(retrieved_batch.endpoint, "/v1/chat/completions");
    }

    #[sqlx::test]
    async fn test_get_batch_not_found(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        let batch_id = uuid::Uuid::new_v4();
        let response = {
            let headers = add_auth_headers(&user);
            app.get(&format!("/ai/v1/batches/{}", batch_id))
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
        }
        .await;

        response.assert_status(StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    async fn test_get_batch_invalid_id_format(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        let response = {
            let headers = add_auth_headers(&user);
            app.get("/ai/v1/batches/not-a-uuid")
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
        }
        .await;

        response.assert_status(StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    async fn test_get_batch_ownership_check(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let user2 = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user1.id, group.id).await;

        let deployment = create_test_deployment(&pool, user1.id, "gpt-3.5-turbo", "gpt-3.5-turbo").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user1.id).await;

        // User1 creates a batch
        let file_content = r#"{"custom_id": "request-1", "method": "POST", "url": "/v1/chat/completions", "body": {"model": "gpt-3.5-turbo", "messages": [{"role": "user", "content": "Hello"}]}}"#;
        let file_part = axum_test::multipart::Part::bytes(file_content.as_bytes()).file_name("test.jsonl");

        let headers = add_auth_headers(&user1);
        let upload_response = app
            .post("/ai/v1/files")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_text("purpose", "batch")
                    .add_part("file", file_part),
            )
            .await;

        let file: FileResponse = upload_response.json();

        let create_req = CreateBatchRequest {
            input_file_id: file.id,
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
        };

        let create_response = {
            let headers = add_auth_headers(&user1);
            app.post("/ai/v1/batches")
                .json(&create_req)
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
        }
        .await;

        let batch: BatchResponse = create_response.json();

        // User2 tries to get user1's batch (should fail)
        let response = {
            let headers = add_auth_headers(&user2);
            app.get(&format!("/ai/v1/batches/{}", batch.id))
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
        }
        .await;

        response.assert_status(StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    async fn test_list_batches_empty(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        let response = {
            let headers = add_auth_headers(&user);
            app.get("/ai/v1/batches")
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
        }
        .await;

        response.assert_status(StatusCode::OK);
        let list: BatchListResponse = response.json();
        assert_eq!(list.data.len(), 0);
        assert!(!list.has_more);
    }

    #[sqlx::test]
    async fn test_list_batches_with_results(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let deployment = create_test_deployment(&pool, user.id, "gpt-3.5-turbo", "gpt-3.5-turbo").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Create a batch
        let file_content = r#"{"custom_id": "request-1", "method": "POST", "url": "/v1/chat/completions", "body": {"model": "gpt-3.5-turbo", "messages": [{"role": "user", "content": "Hello"}]}}"#;
        let file_part = axum_test::multipart::Part::bytes(file_content.as_bytes()).file_name("test.jsonl");

        let headers = add_auth_headers(&user);
        let upload_response = app
            .post("/ai/v1/files")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_text("purpose", "batch")
                    .add_part("file", file_part),
            )
            .await;

        let file: FileResponse = upload_response.json();

        let create_req = CreateBatchRequest {
            input_file_id: file.id,
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
        };

        {
            let headers = add_auth_headers(&user);
            app.post("/ai/v1/batches")
                .json(&create_req)
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
        }
        .await;

        // List batches
        let response = {
            let headers = add_auth_headers(&user);
            app.get("/ai/v1/batches")
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
        }
        .await;

        response.assert_status(StatusCode::OK);
        let list: BatchListResponse = response.json();
        assert_eq!(list.data.len(), 1);
        assert!(!list.has_more);
    }

    #[sqlx::test]
    async fn test_list_batches_ownership_filtering(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let user2 = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user1.id, group.id).await;

        let deployment = create_test_deployment(&pool, user1.id, "gpt-3.5-turbo", "gpt-3.5-turbo").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user1.id).await;

        // User1 creates a batch
        let file_content = r#"{"custom_id": "request-1", "method": "POST", "url": "/v1/chat/completions", "body": {"model": "gpt-3.5-turbo", "messages": [{"role": "user", "content": "Hello"}]}}"#;
        let file_part = axum_test::multipart::Part::bytes(file_content.as_bytes()).file_name("test.jsonl");

        let headers = add_auth_headers(&user1);
        let upload_response = app
            .post("/ai/v1/files")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_text("purpose", "batch")
                    .add_part("file", file_part),
            )
            .await;

        let file: FileResponse = upload_response.json();

        let create_req = CreateBatchRequest {
            input_file_id: file.id,
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
        };

        {
            let headers = add_auth_headers(&user1);
            app.post("/ai/v1/batches")
                .json(&create_req)
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
        }
        .await;

        // User2 lists batches (should see none)
        let response = {
            let headers = add_auth_headers(&user2);
            app.get("/ai/v1/batches")
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
        }
        .await;

        response.assert_status(StatusCode::OK);
        let list: BatchListResponse = response.json();
        assert_eq!(list.data.len(), 0);
    }

    #[sqlx::test]
    async fn test_cancel_batch_success(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let deployment = create_test_deployment(&pool, user.id, "gpt-3.5-turbo", "gpt-3.5-turbo").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Create a batch
        let file_content = r#"{"custom_id": "request-1", "method": "POST", "url": "/v1/chat/completions", "body": {"model": "gpt-3.5-turbo", "messages": [{"role": "user", "content": "Hello"}]}}"#;
        let file_part = axum_test::multipart::Part::bytes(file_content.as_bytes()).file_name("test.jsonl");

        let headers = add_auth_headers(&user);
        let upload_response = app
            .post("/ai/v1/files")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_text("purpose", "batch")
                    .add_part("file", file_part),
            )
            .await;

        let file: FileResponse = upload_response.json();

        let create_req = CreateBatchRequest {
            input_file_id: file.id,
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
        };

        let create_response = {
            let headers = add_auth_headers(&user);
            app.post("/ai/v1/batches")
                .json(&create_req)
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
        }
        .await;

        let batch: BatchResponse = create_response.json();

        // Cancel batch
        let response = {
            let headers = add_auth_headers(&user);
            app.post(&format!("/ai/v1/batches/{}/cancel", batch.id))
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
        }
        .await;

        response.assert_status(StatusCode::OK);
        let cancelled_batch: BatchResponse = response.json();
        assert!(cancelled_batch.status == "cancelling" || cancelled_batch.status == "cancelled");
    }

    #[sqlx::test]
    async fn test_cancel_batch_not_found(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        let batch_id = uuid::Uuid::new_v4();
        let response = {
            let headers = add_auth_headers(&user);
            app.post(&format!("/ai/v1/batches/{}/cancel", batch_id))
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
        }
        .await;

        response.assert_status(StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    async fn test_cancel_batch_ownership_check(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let user2 = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user1.id, group.id).await;

        let deployment = create_test_deployment(&pool, user1.id, "gpt-3.5-turbo", "gpt-3.5-turbo").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user1.id).await;

        // User1 creates a batch
        let file_content = r#"{"custom_id": "request-1", "method": "POST", "url": "/v1/chat/completions", "body": {"model": "gpt-3.5-turbo", "messages": [{"role": "user", "content": "Hello"}]}}"#;
        let file_part = axum_test::multipart::Part::bytes(file_content.as_bytes()).file_name("test.jsonl");

        let headers = add_auth_headers(&user1);
        let upload_response = app
            .post("/ai/v1/files")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_text("purpose", "batch")
                    .add_part("file", file_part),
            )
            .await;

        let file: FileResponse = upload_response.json();

        let create_req = CreateBatchRequest {
            input_file_id: file.id,
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
        };

        let create_response = {
            let headers = add_auth_headers(&user1);
            app.post("/ai/v1/batches")
                .json(&create_req)
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
        }
        .await;

        let batch: BatchResponse = create_response.json();

        // User2 tries to cancel user1's batch (should fail)
        let response = {
            let headers = add_auth_headers(&user2);
            app.post(&format!("/ai/v1/batches/{}/cancel", batch.id))
                .add_header(&headers[0].0, &headers[0].1)
                .add_header(&headers[1].0, &headers[1].1)
        }
        .await;

        response.assert_status(StatusCode::NOT_FOUND);
    }
}
