//! This file deals with the Files API.
//! This is designed to match (as far as possible) the OpenAI Files
//! [API](https://platform.openai.com/docs/api-reference/files/).
//!
//! Repository methods are delegated to the fusillade/ crate - which (as of 04/11/2025) stores
//! files disaggregated in postgres.

use crate::api::models::files::{FileDeleteResponse, FileListResponse, FileResponse, ListFilesQuery, ListObject, ObjectType, Purpose};
use crate::auth::permissions::{can_read_all_resources, has_permission, operation, resource, RequiresPermission};

use crate::db::handlers::{api_keys::ApiKeys, repository::Repository};
use crate::errors::{Error, Result};
use crate::types::{Operation, Resource};
use crate::AppState;
use axum::body::Body;
use axum::response::IntoResponse;
use axum::{
    extract::{Multipart, Path, Query, State},
    http::StatusCode,
    Json,
};
use fusillade::Storage;
use futures::stream::Stream;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

/// OpenAI Batch API request format
/// See: https://platform.openai.com/docs/api-reference/batch
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenAIBatchRequest {
    custom_id: String,
    method: String,
    url: String,
    body: serde_json::Value,
}

impl OpenAIBatchRequest {
    /// Transform OpenAI format to internal format
    ///
    /// # Arguments
    /// * `endpoint` - The target endpoint (e.g., "http://localhost:8080/ai")
    /// * `api_key` - The API key to inject for request execution
    #[tracing::instrument(skip(self, api_key), fields(custom_id = %self.custom_id, method = %self.method, url = %self.url))]
    fn to_internal(&self, endpoint: &str, api_key: String) -> Result<fusillade::RequestTemplateInput> {
        // Extract model from body if present
        let model = self
            .body
            .get("model")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::BadRequest {
                message: "Missing 'model' field in request body".to_string(),
            })?
            .to_string();

        // Serialize body back to string
        let body = serde_json::to_string(&self.body).map_err(|e| Error::BadRequest {
            message: format!("Invalid JSON body: {}", e),
        })?;

        Ok(fusillade::RequestTemplateInput {
            custom_id: Some(self.custom_id.clone()),
            endpoint: endpoint.to_string(),
            method: self.method.clone(),
            path: self.url.clone(),
            body,
            model,
            api_key,
        })
    }

    /// Transform internal format to OpenAI format
    #[tracing::instrument(skip(internal), fields(custom_id = ?internal.custom_id, method = %internal.method, path = %internal.path))]
    fn from_internal(internal: &fusillade::RequestTemplateInput) -> Result<Self> {
        // Parse body string to JSON
        let body: serde_json::Value = serde_json::from_str(&internal.body).map_err(|e| Error::Internal {
            operation: format!("Failed to parse stored body as JSON: {}", e),
        })?;

        Ok(OpenAIBatchRequest {
            custom_id: internal
                .custom_id
                .clone()
                .unwrap_or_else(|| format!("req-{}", uuid::Uuid::new_v4())),
            method: internal.method.clone(),
            url: internal.path.clone(),
            body,
        })
    }
}

/// Helper function to create a stream of FileStreamItem from multipart upload
/// This handles the entire multipart parsing inside the stream
///
/// # Arguments
/// * `endpoint` - Target endpoint for batch requests (e.g., "http://localhost:8080/ai")
/// * `api_key` - API key to inject for request execution
#[tracing::instrument(skip(multipart, api_key), fields(max_file_size, uploaded_by = ?uploaded_by, endpoint = %endpoint, buffer_size))]
fn create_file_stream(
    mut multipart: Multipart,
    max_file_size: u64,
    uploaded_by: Option<String>,
    endpoint: String,
    api_key: String,
    buffer_size: usize,
) -> Pin<Box<dyn Stream<Item = fusillade::FileStreamItem> + Send>> {
    let (tx, rx) = mpsc::channel(buffer_size);

    tokio::spawn(async move {
        let mut total_size = 0i64;
        let mut line_count = 0u64;
        let mut incomplete_line = String::new();
        let mut metadata = fusillade::FileMetadata {
            uploaded_by,
            ..Default::default()
        };
        let mut file_processed = false;

        // Parse multipart fields
        while let Ok(Some(field)) = multipart.next_field().await {
            let field_name = field.name().unwrap_or("").to_string();

            match field_name.as_str() {
                "purpose" => {
                    if let Ok(value) = field.text().await {
                        metadata.purpose = Some(value);
                    }
                }
                "expires_after[anchor]" => {
                    if let Ok(value) = field.text().await {
                        metadata.expires_after_anchor = Some(value);
                    }
                }
                "expires_after[seconds]" => {
                    if let Ok(value) = field.text().await {
                        if let Ok(seconds) = value.parse::<i64>() {
                            metadata.expires_after_seconds = Some(seconds);
                        }
                    }
                }
                "file" => {
                    // Extract filename from the field
                    metadata.filename = field.file_name().map(|s| s.to_string());

                    // Send metadata before processing file content
                    if tx.send(fusillade::FileStreamItem::Metadata(metadata.clone())).await.is_err() {
                        return;
                    }

                    // Now stream and parse the file content
                    let mut field = field;

                    while let Ok(Some(chunk)) = field.chunk().await {
                        let chunk_size = chunk.len() as i64;
                        total_size += chunk_size;

                        tracing::debug!(
                            "Processing chunk: {} bytes, total: {} bytes, lines so far: {}",
                            chunk_size,
                            total_size,
                            line_count
                        );

                        // Check size limit
                        if total_size > max_file_size as i64 {
                            let _ = tx
                                .send(fusillade::FileStreamItem::Error(format!(
                                    "File size exceeds maximum: {} > {}",
                                    total_size, max_file_size
                                )))
                                .await;
                            return;
                        }

                        // Convert chunk to UTF-8
                        let chunk_str = match std::str::from_utf8(&chunk) {
                            Ok(s) => s,
                            Err(_) => {
                                let _ = tx
                                    .send(fusillade::FileStreamItem::Error("File contains invalid UTF-8".to_string()))
                                    .await;
                                return;
                            }
                        };

                        // Combine with incomplete line from previous chunk
                        let text_to_process = if incomplete_line.is_empty() {
                            chunk_str.to_string()
                        } else {
                            format!("{}{}", incomplete_line, chunk_str)
                        };

                        let mut lines = text_to_process.lines().peekable();
                        let ends_with_newline = chunk_str.ends_with('\n');

                        // Process complete lines
                        while let Some(line) = lines.next() {
                            let is_last_line = lines.peek().is_none();

                            // If this is the last line and chunk doesn't end with newline,
                            // it might be incomplete - save it for next chunk
                            if is_last_line && !ends_with_newline {
                                incomplete_line = line.to_string();
                                break;
                            }

                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                continue;
                            }

                            // Parse JSON line as OpenAI Batch format, then transform to internal
                            match serde_json::from_str::<OpenAIBatchRequest>(trimmed) {
                                Ok(openai_req) => {
                                    // Transform to internal format
                                    match openai_req.to_internal(&endpoint, api_key.clone()) {
                                        Ok(template) => {
                                            line_count += 1;
                                            incomplete_line.clear();
                                            if tx.send(fusillade::FileStreamItem::Template(template)).await.is_err() {
                                                return;
                                            }
                                        }
                                        Err(e) => {
                                            let _ = tx
                                                .send(fusillade::FileStreamItem::Error(format!(
                                                    "Failed to transform request on line {}: {:?}",
                                                    line_count + 1,
                                                    e
                                                )))
                                                .await;
                                            return;
                                        }
                                    }
                                }
                                Err(e) => {
                                    let _ = tx
                                        .send(fusillade::FileStreamItem::Error(format!(
                                            "Invalid JSON on line {}: {}",
                                            line_count + 1,
                                            e
                                        )))
                                        .await;
                                    return;
                                }
                            }
                        }
                    }

                    // Process any remaining incomplete line at end of file
                    if !incomplete_line.is_empty() {
                        let trimmed = incomplete_line.trim();
                        if !trimmed.is_empty() {
                            match serde_json::from_str::<OpenAIBatchRequest>(trimmed) {
                                Ok(openai_req) => match openai_req.to_internal(&endpoint, api_key.clone()) {
                                    Ok(template) => {
                                        line_count += 1;
                                        if tx.send(fusillade::FileStreamItem::Template(template)).await.is_err() {
                                            return;
                                        }
                                    }
                                    Err(e) => {
                                        let _ = tx
                                            .send(fusillade::FileStreamItem::Error(format!("Failed to transform final line: {:?}", e)))
                                            .await;
                                        return;
                                    }
                                },
                                Err(e) => {
                                    let _ = tx
                                        .send(fusillade::FileStreamItem::Error(format!("Invalid JSON on final line: {}", e)))
                                        .await;
                                    return;
                                }
                            }
                        }
                    }

                    // Check if file is empty (no templates parsed)
                    if line_count == 0 {
                        let _ = tx
                            .send(fusillade::FileStreamItem::Error(
                                "File contains no valid request templates".to_string(),
                            ))
                            .await;
                        return;
                    }

                    // Set the size and mark file as processed
                    metadata.size_bytes = Some(total_size);
                    file_processed = true;

                    // Continue processing remaining fields (metadata after file)
                }
                _ => {
                    // Unknown field, skip it
                }
            }
        }

        // After all fields are processed, check if we got a file
        if !file_processed {
            let _ = tx
                .send(fusillade::FileStreamItem::Error(
                    "No file field found in multipart upload".to_string(),
                ))
                .await;
            return;
        }

        // Send final metadata with all fields (including any that came after the file)
        let _ = tx.send(fusillade::FileStreamItem::Metadata(metadata.clone())).await;
    });

    Box::pin(ReceiverStream::new(rx))
}

#[utoipa::path(
    post,
    path = "/files",
    tag = "files",
    summary = "Upload file",
    description = "Upload a file that can be used with the Batch API. Files must be JSONL format.",
    request_body(
        content_type = "multipart/form-data",
        description = "File upload with purpose and optional expiration policy"
    ),
    responses(
        (status = 201, description = "File uploaded successfully", body = FileResponse),
        (status = 400, description = "Invalid request"),
        (status = 413, description = "Payload too large"),
        (status = 500, description = "Internal server error")
    )
)]
#[tracing::instrument(skip(state, current_user, multipart), fields(user_id = %current_user.id))]
pub async fn upload_file(
    State(state): State<AppState>,
    current_user: RequiresPermission<resource::Files, operation::CreateOwn>,
    multipart: Multipart,
) -> Result<(StatusCode, Json<FileResponse>)> {
    let max_file_size = state.config.batches.files.max_file_size;
    let uploaded_by = Some(current_user.id.to_string());

    // Fetch system API key for batch request execution
    let mut conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut api_keys_repo = ApiKeys::new(&mut conn);
    let system_key = api_keys_repo
        .get_by_id(Uuid::nil())
        .await
        .map_err(Error::Database)?
        .ok_or_else(|| Error::Internal {
            operation: "System API key not found".to_string(),
        })?;

    // Construct batch execution endpoint (where fusillade will send requests)
    let endpoint = format!("http://{}:{}/ai", state.config.host, state.config.port);

    // Create a stream that parses the multipart upload and yields FileStreamItems
    let file_stream = create_file_stream(
        multipart,
        max_file_size,
        uploaded_by,
        endpoint,
        system_key.secret,
        state.config.batches.files.upload_buffer_size,
    );

    // Create file via request manager with streaming
    let created_file_id = state.request_manager.create_file_stream(file_stream).await.map_err(|e| match e {
        fusillade::FusilladeError::ValidationError(msg) => Error::BadRequest { message: msg },
        _ => Error::Internal {
            operation: format!("create file: {}", e),
        },
    })?;

    tracing::info!("File {} uploaded successfully", created_file_id);

    // Build response using the fusillade file
    let file = state.request_manager.get_file(created_file_id).await.map_err(|e| Error::Internal {
        operation: format!("retrieve created file: {}", e),
    })?;

    // Validate purpose (only batch is supported)
    if let Some(purpose) = file.purpose {
        if purpose != fusillade::Purpose::Batch {
            return Err(Error::BadRequest {
                message: format!("Invalid purpose '{}'. Only 'batch' is supported.", purpose),
            });
        }
    }

    Ok((
        StatusCode::CREATED,
        Json(FileResponse {
            id: file.id.0.to_string(), // Use full UUID, not Display truncation
            object_type: ObjectType::File,
            bytes: file.size_bytes,
            created_at: file.created_at.timestamp(),
            filename: file.name,
            purpose: Purpose::Batch,
        }),
    ))
}

#[utoipa::path(
    get,
    path = "/files",
    tag = "files",
    summary = "List files",
    description = "Returns a list of files.",
    responses(
        (status = 200, description = "List of files", body = FileListResponse),
        (status = 500, description = "Internal server error")
    ),
    params(
        ListFilesQuery
    )
)]
#[tracing::instrument(skip(state, current_user), fields(user_id = %current_user.id, limit = ?query.limit, order = %query.order))]
pub async fn list_files(
    State(state): State<AppState>,
    Query(query): Query<ListFilesQuery>,
    current_user: RequiresPermission<resource::Files, operation::ReadOwn>,
) -> Result<Json<FileListResponse>> {
    let has_system_access = has_permission(&current_user, Resource::Files, Operation::SystemAccess);
    let can_read_all_files = can_read_all_resources(&current_user, Resource::Files);

    if query.order != "asc" && query.order != "desc" {
        return Err(Error::BadRequest {
            message: "Order must be 'asc' or 'desc'".to_string(),
        });
    }

    let limit = query.limit.unwrap_or(20).clamp(1, 10000);

    // Build filter based on permissions
    let filter = fusillade::FileFilter {
        // Filter by ownership if user can't read all files
        uploaded_by: if !can_read_all_files {
            Some(current_user.id.to_string())
        } else {
            None
        },
        // Filter by status if user doesn't have system access
        // TODO: What is the point of this 'status' field?
        status: if !has_system_access { Some("processed".to_string()) } else { None },
        purpose: None,
    };

    use fusillade::Storage;
    let mut files = state.request_manager.list_files(filter).await.map_err(|e| Error::Internal {
        operation: format!("list files: {}", e),
    })?;

    // Apply limit and pagination (simple version)
    let has_more = files.len() > limit as usize;
    if has_more {
        files.truncate(limit as usize);
    }

    let first_id = files.first().map(|f| f.id.0.to_string());
    let last_id = files.last().map(|f| f.id.0.to_string());

    let data: Vec<FileResponse> = files
        .iter()
        .map(|f| FileResponse {
            id: f.id.0.to_string(), // Use full UUID, not Display truncation
            object_type: ObjectType::File,
            bytes: f.size_bytes,
            created_at: f.created_at.timestamp(),
            filename: f.name.clone(),
            purpose: Purpose::Batch,
        })
        .collect();

    Ok(Json(FileListResponse {
        object_type: ListObject::List,
        data,
        first_id,
        last_id,
        has_more,
    }))
}

#[utoipa::path(
    get,
    path = "/files/{file_id}",
    tag = "files",
    summary = "Retrieve file",
    description = "Returns information about a specific file.",
    responses(
        (status = 200, description = "File metadata", body = FileResponse),
        (status = 404, description = "File not found"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("file_id" = String, Path, description = "The ID of the file to retrieve")
    )
)]
#[tracing::instrument(skip(state, current_user), fields(user_id = %current_user.id, file_id = %file_id_str))]
pub async fn get_file(
    State(state): State<AppState>,
    Path(file_id_str): Path<String>,
    current_user: RequiresPermission<resource::Files, operation::ReadOwn>,
) -> Result<Json<FileResponse>> {
    let has_system_access = has_permission(&current_user, Resource::Files, Operation::SystemAccess);
    let can_read_all_files = can_read_all_resources(&current_user, Resource::Files);

    let file_id = Uuid::parse_str(&file_id_str).map_err(|_| Error::BadRequest {
        message: "Invalid file ID format".to_string(),
    })?;

    let file = state
        .request_manager
        .get_file(fusillade::FileId(file_id))
        .await
        .map_err(|_e| Error::NotFound {
            resource: "File".to_string(),
            id: file_id_str.clone(),
        })?;

    // Check ownership: users without ReadAll permission can only see their own files
    if !can_read_all_files {
        let user_id = current_user.id.to_string();
        if file.uploaded_by.as_deref() != Some(user_id.as_str()) {
            return Err(Error::NotFound {
                resource: "File".to_string(),
                id: file_id_str,
            });
        }
    }

    // Check status: users without SystemAccess can only see processed files
    if !has_system_access && file.status != fusillade::FileStatus::Processed {
        return Err(Error::NotFound {
            resource: "File".to_string(),
            id: file_id_str,
        });
    }

    Ok(Json(FileResponse {
        id: file.id.0.to_string(), // Use full UUID, not Display truncation
        object_type: ObjectType::File,
        bytes: file.size_bytes,
        created_at: file.created_at.timestamp(),
        filename: file.name,
        purpose: Purpose::Batch,
    }))
}

#[utoipa::path(
    get,
    path = "/files/{file_id}/content",
    tag = "files",
    summary = "Retrieve file content",
    description = "Download the content of a file as JSONL. Returns the file metadata and request templates.",
    responses(
        (status = 200, description = "File content", content_type = "application/x-ndjson"),
        (status = 404, description = "File not found"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("file_id" = String, Path, description = "The ID of the file to retrieve content from")
    )
)]
#[tracing::instrument(skip(state, current_user), fields(user_id = %current_user.id, file_id = %file_id_str))]
pub async fn get_file_content(
    State(state): State<AppState>,
    Path(file_id_str): Path<String>,
    current_user: RequiresPermission<resource::Files, operation::ReadOwn>,
) -> Result<axum::response::Response> {
    let has_system_access = has_permission(&current_user, Resource::Files, Operation::SystemAccess);
    let can_read_all_files = can_read_all_resources(&current_user, Resource::Files);

    let file_id = Uuid::parse_str(&file_id_str).map_err(|_| Error::BadRequest {
        message: "Invalid file ID format".to_string(),
    })?;

    use fusillade::Storage;

    // First, get the file to check ownership and status
    let file = state
        .request_manager
        .get_file(fusillade::FileId(file_id))
        .await
        .map_err(|_e| Error::NotFound {
            resource: "File".to_string(),
            id: file_id_str.clone(),
        })?;

    // Check ownership: users without ReadAll permission can only see their own files
    if !can_read_all_files {
        let user_id = current_user.id.to_string();
        if file.uploaded_by.as_deref() != Some(user_id.as_str()) {
            return Err(Error::NotFound {
                resource: "File".to_string(),
                id: file_id_str,
            });
        }
    }

    // Check status: users without SystemAccess can only see processed files
    if !has_system_access && file.status != fusillade::FileStatus::Processed {
        return Err(Error::NotFound {
            resource: "File".to_string(),
            id: file_id_str,
        });
    }

    // For batch output/error files, verify the batch is complete before allowing download
    if matches!(
        file.purpose,
        Some(fusillade::Purpose::BatchOutput) | Some(fusillade::Purpose::BatchError)
    ) {
        // Find the batch that owns this file
        let batches = state
            .request_manager
            .list_batches(None, None, 1000) // Search through batches
            .await
            .map_err(|e| Error::Internal {
                operation: format!("list batches: {}", e),
            })?;

        let batch = batches.into_iter().find(|b| {
            if file.purpose == Some(fusillade::Purpose::BatchOutput) {
                b.output_file_id == Some(fusillade::FileId(file_id))
            } else {
                b.error_file_id == Some(fusillade::FileId(file_id))
            }
        });

        if let Some(batch) = batch {
            // Get batch status to check if it's complete
            let status = state
                .request_manager
                .get_batch_status(batch.id)
                .await
                .map_err(|e| Error::Internal {
                    operation: format!("get batch status: {}", e),
                })?;

            let openai_status = status.openai_status();

            // Only allow download if batch is in a terminal state
            if openai_status != "completed" && openai_status != "failed" && openai_status != "cancelled" && openai_status != "expired" {
                return Err(Error::BadRequest {
                    message: format!(
                        "Batch is still processing (status: {}). Wait for completion before downloading results.",
                        openai_status
                    ),
                });
            }
        }
    }

    // Stream the file content as JSONL
    let content_stream = state.request_manager.get_file_content_stream(fusillade::FileId(file_id));

    // Convert FileContentItem to JSONL (one per line)
    let jsonl_stream = content_stream.map(|content_result| {
        content_result
            .and_then(|content_item| {
                // Handle different content types
                match content_item {
                    fusillade::FileContentItem::Template(template) => {
                        // Transform to OpenAI format (drops api_key, endpoint)
                        OpenAIBatchRequest::from_internal(&template)
                            .map_err(|e| fusillade::FusilladeError::Other(anyhow::anyhow!("Failed to transform to OpenAI format: {:?}", e)))
                            .and_then(|openai_req| {
                                serde_json::to_string(&openai_req)
                                    .map(|json| format!("{}\n", json))
                                    .map_err(|e| fusillade::FusilladeError::Other(anyhow::anyhow!("JSON serialization failed: {}", e)))
                            })
                    }
                    fusillade::FileContentItem::Output(output) => {
                        // Already in OpenAI format, just serialize
                        serde_json::to_string(&output)
                            .map(|json| format!("{}\n", json))
                            .map_err(|e| fusillade::FusilladeError::Other(anyhow::anyhow!("JSON serialization failed: {}", e)))
                    }
                    fusillade::FileContentItem::Error(error) => {
                        // Already in OpenAI format, just serialize
                        serde_json::to_string(&error)
                            .map(|json| format!("{}\n", json))
                            .map_err(|e| fusillade::FusilladeError::Other(anyhow::anyhow!("JSON serialization failed: {}", e)))
                    }
                }
            })
            .map_err(|e| std::io::Error::other(e.to_string()))
    });

    let body = Body::from_stream(jsonl_stream);

    Ok((StatusCode::OK, [("content-type", "application/x-ndjson")], body).into_response())
}

#[utoipa::path(
    delete,
    path = "/files/{file_id}",
    tag = "files",
    summary = "Delete file",
    description = "Delete a file by ID",
    responses(
        (status = 200, description = "File deleted successfully", body = FileDeleteResponse),
        (status = 404, description = "File not found"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("file_id" = String, Path, description = "The ID of the file to delete")
    )
)]
#[tracing::instrument(skip(state, current_user), fields(user_id = %current_user.id, file_id = %file_id_str))]
pub async fn delete_file(
    State(state): State<AppState>,
    Path(file_id_str): Path<String>,
    current_user: RequiresPermission<resource::Files, operation::DeleteOwn>,
) -> Result<Json<FileDeleteResponse>> {
    let can_delete_all_files = can_read_all_resources(&current_user, Resource::Files);

    let file_id = Uuid::parse_str(&file_id_str).map_err(|_| Error::BadRequest {
        message: "Invalid file ID format".to_string(),
    })?;

    // First, get the file to check ownership
    let file = state
        .request_manager
        .get_file(fusillade::FileId(file_id))
        .await
        .map_err(|_e| Error::NotFound {
            resource: "File".to_string(),
            id: file_id_str.clone(),
        })?;

    // Check ownership: users without DeleteAll permission can only delete their own files
    if !can_delete_all_files {
        let user_id = current_user.id.to_string();
        if file.uploaded_by.as_deref() != Some(user_id.as_str()) {
            return Err(Error::NotFound {
                resource: "File".to_string(),
                id: file_id_str.clone(),
            });
        }
    }

    // Perform the deletion (hard delete - cascades to batches and requests)
    state
        .request_manager
        .delete_file(fusillade::FileId(file_id))
        .await
        .map_err(|e| Error::Internal {
            operation: format!("delete file: {}", e),
        })?;

    Ok(Json(FileDeleteResponse {
        id: file_id.to_string(),
        object_type: ObjectType::File,
        deleted: true,
    }))
}

#[cfg(test)]
mod tests {
    use crate::api::models::files::FileResponse;
    use crate::api::models::users::Role;
    use crate::test_utils::*;
    use sqlx::PgPool;

    #[sqlx::test]
    #[test_log::test]
    async fn test_upload_and_download_file_content(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        // User needs BatchAPIUser role to create/read files
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create test JSONL content with 3 request templates in OpenAI Batch API format
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello 1"}]}}
{"custom_id":"request-2","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello 2"}]}}
{"custom_id":"request-3","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello 3"}]}}
"#;

        // Upload the file
        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");

        let upload_response = app
            .post("/admin/api/v1/files")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_text("purpose", "batch")
                    .add_part("file", file_part),
            )
            .await;

        upload_response.assert_status(axum::http::StatusCode::CREATED);
        let file: FileResponse = upload_response.json();
        let file_id = file.id;

        // Download the file content
        let download_response = app
            .get(&format!("/admin/api/v1/files/{}/content", file_id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        download_response.assert_status(axum::http::StatusCode::OK);
        download_response.assert_header("content-type", "application/x-ndjson");

        let downloaded_content = download_response.text();

        // Verify the downloaded content matches the uploaded content
        // Note: lines might have different whitespace, so compare each line as JSON
        let original_lines: Vec<&str> = jsonl_content.trim().lines().collect();
        let downloaded_lines: Vec<&str> = downloaded_content.trim().lines().collect();

        assert_eq!(original_lines.len(), downloaded_lines.len(), "Number of lines should match");

        for (i, (orig, down)) in original_lines.iter().zip(downloaded_lines.iter()).enumerate() {
            let orig_json: serde_json::Value = serde_json::from_str(orig).unwrap_or_else(|_| panic!("Failed to parse original line {}", i));
            let down_json: serde_json::Value =
                serde_json::from_str(down).unwrap_or_else(|_| panic!("Failed to parse downloaded line {}", i));
            assert_eq!(orig_json, down_json, "Line {} should match (orig: {}, down: {})", i, orig, down);
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_upload_missing_model_field(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        // Missing model field in body
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"messages":[{"role":"user","content":"Hello"}]}}"#;

        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");

        let upload_response = app
            .post("/admin/api/v1/files")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_text("purpose", "batch")
                    .add_part("file", file_part),
            )
            .await;

        // Should reject with 400 Bad Request
        upload_response.assert_status(axum::http::StatusCode::BAD_REQUEST);
        let error_body = upload_response.text();
        assert!(error_body.contains("model"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_upload_missing_custom_id(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        // Missing custom_id field
        let jsonl_content =
            r#"{"method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;

        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");

        let upload_response = app
            .post("/admin/api/v1/files")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_text("purpose", "batch")
                    .add_part("file", file_part),
            )
            .await;

        // Should reject with 400 Bad Request since custom_id is required
        upload_response.assert_status(axum::http::StatusCode::BAD_REQUEST);
        let error_body = upload_response.text();
        assert!(error_body.contains("custom_id"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_upload_invalid_json_body(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        // Invalid JSON in body field (not an object)
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":"not a json object"}"#;

        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");

        let upload_response = app
            .post("/admin/api/v1/files")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_text("purpose", "batch")
                    .add_part("file", file_part),
            )
            .await;

        // Should reject with 400 Bad Request
        upload_response.assert_status(axum::http::StatusCode::BAD_REQUEST);
        let error_body = upload_response.text();
        assert!(error_body.contains("model"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_upload_malformed_jsonl(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        // Malformed JSONL - not valid JSON
        let jsonl_content = "this is not json at all\n{also not json}";

        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");

        let upload_response = app
            .post("/admin/api/v1/files")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_text("purpose", "batch")
                    .add_part("file", file_part),
            )
            .await;

        // Should reject with 400 Bad Request
        upload_response.assert_status(axum::http::StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_upload_empty_file(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        // Empty file
        let jsonl_content = "";

        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");

        let upload_response = app
            .post("/admin/api/v1/files")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_text("purpose", "batch")
                    .add_part("file", file_part),
            )
            .await;

        // Should reject with 400 Bad Request
        upload_response.assert_status(axum::http::StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_upload_with_metadata_after_file_field(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        // Create test JSONL content
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;

        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");

        // NOTE: The file field is added BEFORE the metadata fields (purpose, expires_after)
        // This tests whether the handler correctly processes metadata regardless of field order
        let upload_response = app
            .post("/admin/api/v1/files")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_part("file", file_part)
                    .add_text("purpose", "batch")
                    .add_text("expires_after[anchor]", "processing_complete")
                    .add_text("expires_after[seconds]", "86400"),
            )
            .await;

        // Should succeed
        upload_response.assert_status(axum::http::StatusCode::CREATED);
        let file: FileResponse = upload_response.json();

        // Verify the file was created - now let's check if metadata was captured
        // We need to query the database or fusillade to verify the metadata was stored
        // For now, we verify the upload succeeded and the file exists
        let get_response = app
            .get(&format!("/admin/api/v1/files/{}", file.id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        get_response.assert_status(axum::http::StatusCode::OK);
        let retrieved_file: FileResponse = get_response.json();
        assert_eq!(retrieved_file.purpose, crate::api::models::files::Purpose::Batch);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_upload_duplicate_filename(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        // Create test JSONL content
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;

        // Upload first file with a specific filename
        let file_part1 = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("duplicate-test.jsonl");

        let upload_response1 = app
            .post("/admin/api/v1/files")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_text("purpose", "batch")
                    .add_part("file", file_part1),
            )
            .await;

        // First upload should succeed
        upload_response1.assert_status(axum::http::StatusCode::CREATED);

        // Try to upload another file with the same filename
        let file_part2 = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("duplicate-test.jsonl");

        let upload_response2 = app
            .post("/admin/api/v1/files")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_text("purpose", "batch")
                    .add_part("file", file_part2),
            )
            .await;

        // Second upload should fail with 400 Bad Request
        upload_response2.assert_status(axum::http::StatusCode::BAD_REQUEST);
        let error_body = upload_response2.text();
        assert!(
            error_body.contains("already exists"),
            "Error message should mention file already exists: {}",
            error_body
        );
    }
}
