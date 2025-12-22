//! This file deals with the Files API.
//! This is designed to match (as far as possible) the OpenAI Files
//! [API](https://platform.openai.com/docs/api-reference/files/).
//!
//! Repository methods are delegated to the fusillade/ crate - which (as of 04/11/2025) stores
//! files disaggregated in postgres.

use crate::api::models::files::{
    FileContentQuery, FileCostEstimate, FileDeleteResponse, FileListResponse, FileResponse, ListFilesQuery, ListObject, ObjectType, Purpose,
};
use crate::auth::permissions::{RequiresPermission, can_read_all_resources, operation, resource};

use crate::AppState;
use crate::db::{
    handlers::api_keys::ApiKeys,
    handlers::deployments::{DeploymentFilter, Deployments},
    handlers::repository::Repository,
    handlers::tariffs::Tariffs,
    models::api_keys::ApiKeyPurpose,
    models::deployments::ModelStatus,
};
use crate::errors::{Error, Result};
use crate::types::Resource;
use axum::{
    Json,
    extract::{Multipart, Path, Query, State},
    http::StatusCode,
};
use chrono::Utc;
use fusillade::Storage;
use futures::StreamExt;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
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

/// Allowed HTTP methods for batch requests
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AllowedHttpMethod {
    Post,
}

impl std::str::FromStr for AllowedHttpMethod {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_uppercase().as_str() {
            "POST" => Ok(Self::Post),
            _ => Err(Error::BadRequest {
                message: format!("Unsupported HTTP method '{}'. Only POST is currently supported.", s),
            }),
        }
    }
}

/// Allowed URL paths for batch requests
const ALLOWED_URL_PATHS: &[&str] = &["/v1/chat/completions", "/v1/completions", "/v1/embeddings"];

fn validate_url_path(url: &str) -> Result<()> {
    if !ALLOWED_URL_PATHS.contains(&url) {
        return Err(Error::BadRequest {
            message: format!(
                "Unsupported URL path '{}'. Allowed paths are: {}",
                url,
                ALLOWED_URL_PATHS.join(", ")
            ),
        });
    }
    Ok(())
}

impl OpenAIBatchRequest {
    /// Transform OpenAI format to internal format
    ///
    /// # Arguments
    /// * `endpoint` - The target endpoint (e.g., "http://localhost:8080/ai")
    /// * `api_key` - The API key to inject for request execution
    /// * `accessible_models` - Set of model aliases the user can access
    #[tracing::instrument(skip(self, api_key, accessible_models), fields(custom_id = %self.custom_id, method = %self.method, url = %self.url))]
    fn to_internal(&self, endpoint: &str, api_key: String, accessible_models: &HashSet<String>) -> Result<fusillade::RequestTemplateInput> {
        // Validate HTTP method
        let _validated_method = self.method.parse::<AllowedHttpMethod>()?;

        // Validate URL path
        validate_url_path(&self.url)?;

        // Extract model from body if present
        let model = self
            .body
            .get("model")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::BadRequest {
                message: "Missing 'model' field in request body".to_string(),
            })?
            .to_string();

        // Validate model access
        if !accessible_models.contains(&model) {
            return Err(Error::BadRequest {
                message: format!("Model '{}' has not been configured or is not available to user.", model),
            });
        }

        // Strip 'priority' key from body if present (users shouldn't control priority)
        let mut sanitized_body = self.body.clone();
        if sanitized_body.is_object()
            && let Some(obj) = sanitized_body.as_object_mut()
            && obj.remove("priority").is_some()
        {
            tracing::debug!(
                custom_id = %self.custom_id,
                "Stripped 'priority' field from request body"
            );
        }

        // Serialize sanitized body back to string
        let body = serde_json::to_string(&sanitized_body).map_err(|e| Error::BadRequest {
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
#[tracing::instrument(skip(multipart, api_key, accessible_models), fields(max_file_size, uploaded_by = ?uploaded_by, endpoint = %endpoint, buffer_size))]
fn create_file_stream(
    mut multipart: Multipart,
    max_file_size: u64,
    uploaded_by: Option<String>,
    endpoint: String,
    api_key: String,
    buffer_size: usize,
    accessible_models: HashSet<String>,
) -> Pin<Box<dyn Stream<Item = fusillade::FileStreamItem> + Send>> {
    let (tx, rx) = mpsc::channel(buffer_size);

    tokio::spawn(async move {
        let mut total_size = 0i64;
        let mut line_count = 0u64;
        let mut incomplete_line = String::new();
        let mut incomplete_utf8_bytes = Vec::new(); // Buffer for incomplete UTF-8 sequences at chunk boundaries
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
                    if let Ok(value) = field.text().await
                        && let Ok(seconds) = value.parse::<i64>()
                    {
                        metadata.expires_after_seconds = Some(seconds);
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

                        // Combine incomplete UTF-8 bytes from previous chunk with current chunk
                        let combined_bytes = if incomplete_utf8_bytes.is_empty() {
                            chunk.to_vec()
                        } else {
                            let mut combined = incomplete_utf8_bytes.clone();
                            combined.extend_from_slice(&chunk);
                            combined
                        };

                        // Try to convert to UTF-8, handling incomplete sequences at the end
                        let (chunk_str, remaining_bytes) = match std::str::from_utf8(&combined_bytes) {
                            Ok(s) => {
                                // All bytes are valid UTF-8
                                incomplete_utf8_bytes.clear();
                                (s.to_string(), Vec::new())
                            }
                            Err(e) => {
                                // Check if the error is due to an incomplete sequence at the end
                                let valid_up_to = e.valid_up_to();

                                // If there's an error length, it means we have invalid UTF-8, not just incomplete
                                if let Some(error_len) = e.error_len() {
                                    // This is actual invalid UTF-8, not just an incomplete sequence
                                    tracing::error!(
                                        "UTF-8 parsing error on/near line {}, byte offset {} in combined buffer, total file offset ~{}, combined buffer size: {} bytes, error: {:?}",
                                        line_count + 1,
                                        valid_up_to,
                                        total_size - chunk_size + valid_up_to as i64,
                                        combined_bytes.len(),
                                        e
                                    );

                                    // Show a hex dump of the problematic area
                                    let error_start = valid_up_to.saturating_sub(20);
                                    let error_end = (valid_up_to + error_len + 20).min(combined_bytes.len());
                                    let problem_bytes = &combined_bytes[error_start..error_end];
                                    tracing::error!("Bytes around error (offset {}-{}): {:02x?}", error_start, error_end, problem_bytes);

                                    // Try to show ASCII representation
                                    let ascii_repr: String = problem_bytes
                                        .iter()
                                        .map(|&b| if b.is_ascii_graphic() || b == b' ' { b as char } else { '.' })
                                        .collect();
                                    tracing::error!("ASCII representation: '{}'", ascii_repr);

                                    // Also show the incomplete_line if any
                                    if !incomplete_line.is_empty() {
                                        tracing::error!(
                                            "Incomplete line from previous chunk (may be part of the problem): '{}'",
                                            incomplete_line.chars().take(200).collect::<String>()
                                        );
                                    }

                                    let error_msg = format!(
                                        "File contains invalid UTF-8 on/near line {} at byte offset {}. Error: {}",
                                        line_count + 1,
                                        total_size - chunk_size + valid_up_to as i64,
                                        e
                                    );
                                    let _ = tx.send(fusillade::FileStreamItem::Error(error_msg)).await;
                                    return;
                                }

                                // Otherwise, this is an incomplete UTF-8 sequence at the end of the chunk
                                // Save the incomplete bytes for the next chunk
                                let valid_str =
                                    std::str::from_utf8(&combined_bytes[..valid_up_to]).expect("valid_up_to should point to valid UTF-8");
                                let remaining = combined_bytes[valid_up_to..].to_vec();

                                tracing::debug!(
                                    "Incomplete UTF-8 sequence at chunk boundary, buffering {} bytes for next chunk",
                                    remaining.len()
                                );

                                (valid_str.to_string(), remaining)
                            }
                        };

                        // Update the incomplete UTF-8 buffer for next iteration
                        incomplete_utf8_bytes = remaining_bytes;

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
                                    // Transform to internal format (includes model access validation)
                                    match openai_req.to_internal(&endpoint, api_key.clone(), &accessible_models) {
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
                                                    "Failed to transform request on line {}: {}",
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
                                Ok(openai_req) => match openai_req.to_internal(&endpoint, api_key.clone(), &accessible_models) {
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

    // Get or create user-specific hidden batch API key for batch request execution
    let mut conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut api_keys_repo = ApiKeys::new(&mut conn);
    let user_api_key = api_keys_repo
        .get_or_create_hidden_key(current_user.id, ApiKeyPurpose::Batch)
        .await
        .map_err(Error::Database)?;

    // Construct batch execution endpoint (where fusillade will send requests)
    let endpoint = format!("http://{}:{}/ai", state.config.host, state.config.port);

    // Query models accessible to the user for validation during file parsing
    let mut deployments_repo = Deployments::new(&mut conn);
    let filter = DeploymentFilter::new(0, i64::MAX)
        .with_accessible_to(current_user.id)
        .with_statuses(vec![ModelStatus::Active])
        .with_deleted(false);
    let accessible_deployments = deployments_repo.list(&filter).await.map_err(Error::Database)?;
    let accessible_models: HashSet<String> = accessible_deployments.into_iter().map(|d| d.alias).collect();

    // drop conn so it isn't persisted for entire upload process
    drop(conn);

    // Create a stream that parses the multipart upload and yields FileStreamItems
    let file_stream = create_file_stream(
        multipart,
        max_file_size,
        uploaded_by,
        endpoint,
        user_api_key,
        state.config.batches.files.upload_buffer_size,
        accessible_models,
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
    if let Some(purpose) = file.purpose
        && purpose != fusillade::Purpose::Batch
    {
        return Err(Error::BadRequest {
            message: format!("Invalid purpose '{}'. Only 'batch' is supported.", purpose),
        });
    }

    // Convert fusillade Purpose to API Purpose
    let api_purpose = match file.purpose {
        Some(fusillade::batch::Purpose::Batch) => Purpose::Batch,
        Some(fusillade::batch::Purpose::BatchOutput) => Purpose::BatchOutput,
        Some(fusillade::batch::Purpose::BatchError) => Purpose::BatchError,
        None => Purpose::Batch, // Default to Batch for backwards compatibility
    };

    Ok((
        StatusCode::CREATED,
        Json(FileResponse {
            id: file.id.0.to_string(), // Use full UUID, not Display truncation
            object_type: ObjectType::File,
            bytes: file.size_bytes,
            created_at: file.created_at.timestamp(),
            filename: file.name,
            purpose: api_purpose,
            expires_at: file.expires_at.map(|dt| dt.timestamp()),
        }),
    ))
}

#[utoipa::path(
    get,
    path = "/files",
    tag = "files",
    summary = "List files",
    description = "Returns a list of files with cursor-based pagination (OpenAI-compatible). Use the `last_id` from the response as the `after` parameter to get the next page.",
    responses(
        (status = 200, description = "List of files with pagination metadata (first_id, last_id, has_more)", body = FileListResponse),
        (status = 500, description = "Internal server error")
    ),
    params(
        ListFilesQuery
    )
)]
#[tracing::instrument(skip(state, current_user), fields(user_id = %current_user.id, limit = ?query.pagination.limit, order = %query.order))]
pub async fn list_files(
    State(state): State<AppState>,
    Query(query): Query<ListFilesQuery>,
    current_user: RequiresPermission<resource::Files, operation::ReadOwn>,
) -> Result<Json<FileListResponse>> {
    let can_read_all_files = can_read_all_resources(&current_user, Resource::Files);

    if query.order != "asc" && query.order != "desc" {
        return Err(Error::BadRequest {
            message: "Order must be 'asc' or 'desc'".to_string(),
        });
    }

    let limit = query.pagination.limit();

    // Parse the 'after' cursor if provided
    let after = query
        .pagination
        .after
        .as_ref()
        .and_then(|id_str| uuid::Uuid::parse_str(id_str).ok().map(fusillade::FileId::from));

    // Build filter based on permissions
    let filter = fusillade::FileFilter {
        // Filter by ownership if user can't read all files
        uploaded_by: if !can_read_all_files {
            Some(current_user.id.to_string())
        } else {
            None
        },
        // No status filtering
        status: None,
        purpose: query.purpose.clone(),
        after,
        limit: Some((limit + 1) as usize), // Fetch one extra to check has_more
        ascending: query.order == "asc",
    };

    use fusillade::Storage;
    let mut files = state.request_manager.list_files(filter).await.map_err(|e| Error::Internal {
        operation: format!("list files: {}", e),
    })?;

    // Check if there are more results
    let has_more = files.len() > limit as usize;
    if has_more {
        files.truncate(limit as usize);
    }

    let first_id = files.first().map(|f| f.id.0.to_string());
    let last_id = files.last().map(|f| f.id.0.to_string());

    let data: Vec<FileResponse> = files
        .iter()
        .map(|f| {
            // Convert fusillade Purpose to API Purpose
            let api_purpose = match f.purpose {
                Some(fusillade::batch::Purpose::Batch) => Purpose::Batch,
                Some(fusillade::batch::Purpose::BatchOutput) => Purpose::BatchOutput,
                Some(fusillade::batch::Purpose::BatchError) => Purpose::BatchError,
                None => Purpose::Batch, // Default to Batch for backwards compatibility
            };

            FileResponse {
                id: f.id.0.to_string(), // Use full UUID, not Display truncation
                object_type: ObjectType::File,
                bytes: f.size_bytes,
                created_at: f.created_at.timestamp(),
                filename: f.name.clone(),
                purpose: api_purpose,
                expires_at: f.expires_at.map(|dt| dt.timestamp()),
            }
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

    // Convert fusillade Purpose to API Purpose
    let api_purpose = match file.purpose {
        Some(fusillade::batch::Purpose::Batch) => Purpose::Batch,
        Some(fusillade::batch::Purpose::BatchOutput) => Purpose::BatchOutput,
        Some(fusillade::batch::Purpose::BatchError) => Purpose::BatchError,
        None => Purpose::Batch, // Default to Batch for backwards compatibility
    };

    Ok(Json(FileResponse {
        id: file.id.0.to_string(), // Use full UUID, not Display truncation
        object_type: ObjectType::File,
        bytes: file.size_bytes,
        created_at: file.created_at.timestamp(),
        filename: file.name,
        purpose: api_purpose,
        expires_at: file.expires_at.map(|dt| dt.timestamp()),
    }))
}

#[utoipa::path(
    get,
    path = "/files/{file_id}/content",
    tag = "files",
    summary = "Retrieve file content",
    description = "Download the content of a file as JSONL. Returns the file metadata and request templates. Supports pagination via limit and offset parameters.",
    responses(
        (status = 200, description = "File content", content_type = "application/x-ndjson"),
        (status = 404, description = "File not found"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("file_id" = String, Path, description = "The ID of the file to retrieve content from"),
        FileContentQuery
    )
)]
#[tracing::instrument(skip(state, current_user), fields(user_id = %current_user.id, file_id = %file_id_str, limit = ?query.pagination.limit, offset = ?query.pagination.skip))]
pub async fn get_file_content(
    State(state): State<AppState>,
    Path(file_id_str): Path<String>,
    Query(query): Query<FileContentQuery>,
    current_user: RequiresPermission<resource::Files, operation::ReadOwn>,
) -> Result<axum::response::Response> {
    let can_read_all_files = can_read_all_resources(&current_user, Resource::Files);

    let file_id = Uuid::parse_str(&file_id_str).map_err(|_| Error::BadRequest {
        message: "Invalid file ID format".to_string(),
    })?;

    use fusillade::Storage;

    // First, get the file to check ownership
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

    // Stream the file content as JSONL, starting from offset
    let offset = query.pagination.skip.unwrap_or(0) as usize;
    let content_stream = state.request_manager.get_file_content_stream(fusillade::FileId(file_id), offset);

    // Apply limit if specified, fetching one extra to detect if there are more results
    let requested_limit = query.pagination.limit.map(|l| l as usize);
    let fetch_limit = requested_limit.map(|l| l + 1); // Fetch one extra to check for more results

    let content_stream: Pin<Box<dyn Stream<Item = fusillade::Result<fusillade::FileContentItem>> + Send>> = if let Some(limit) = fetch_limit
    {
        Box::pin(content_stream.take(limit))
    } else {
        Box::pin(content_stream)
    };

    // Collect items to determine if we need to truncate and set headers
    let items: Vec<_> = content_stream.collect().await;

    // Check if there's more paginated data currently available
    let has_more_paginated = if let Some(req_limit) = requested_limit {
        items.len() > req_limit
    } else {
        false // No limit means we fetched everything currently available
    };

    // For BatchOutput and BatchError files, check if the batch is still running
    // (which means more data may be written to this file in the future)
    let file_may_receive_more_data = match file.purpose {
        Some(fusillade::batch::Purpose::Batch) => false, // Input files are static
        Some(fusillade::batch::Purpose::BatchOutput) => {
            let batch = state
                .request_manager
                .get_batch_by_output_file_id(fusillade::FileId(file_id), fusillade::batch::OutputFileType::Output)
                .await
                .map_err(|e| Error::Internal {
                    operation: format!("get batch by output file: {}", e),
                })?;
            if let Some(batch) = batch {
                let status = state
                    .request_manager
                    .get_batch_status(batch.id)
                    .await
                    .map_err(|e| Error::Internal {
                        operation: format!("get batch status: {}", e),
                    })?;
                status.pending_requests > 0 || status.in_progress_requests > 0
            } else {
                false
            }
        }
        Some(fusillade::batch::Purpose::BatchError) => {
            let batch = state
                .request_manager
                .get_batch_by_output_file_id(fusillade::FileId(file_id), fusillade::batch::OutputFileType::Error)
                .await
                .map_err(|e| Error::Internal {
                    operation: format!("get batch by error file: {}", e),
                })?;
            if let Some(batch) = batch {
                let status = state
                    .request_manager
                    .get_batch_status(batch.id)
                    .await
                    .map_err(|e| Error::Internal {
                        operation: format!("get batch status: {}", e),
                    })?;
                status.pending_requests > 0 || status.in_progress_requests > 0
            } else {
                false
            }
        }
        None => false, // Shouldn't happen, but assume complete
    };

    let has_more = has_more_paginated || file_may_receive_more_data;

    // Truncate to requested limit if we fetched extra
    let items_to_return = if let Some(req_limit) = requested_limit {
        items.into_iter().take(req_limit).collect::<Vec<_>>()
    } else {
        items
    };

    let line_count = items_to_return.len();
    let last_line = offset + line_count;

    // Convert FileContentItem to JSONL (one per line)
    let mut jsonl_lines = Vec::new();
    for content_result in items_to_return {
        let json_line = content_result
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
            .map_err(|e| Error::Internal {
                operation: format!("serialize content: {}", e),
            })?;
        jsonl_lines.push(json_line);
    }

    let jsonl_content = jsonl_lines.join("");

    let mut response = axum::response::Response::new(axum::body::Body::from(jsonl_content));
    response
        .headers_mut()
        .insert("content-type", "application/x-ndjson".parse().unwrap());
    response.headers_mut().insert("X-Incomplete", has_more.to_string().parse().unwrap());
    response.headers_mut().insert("X-Last-Line", last_line.to_string().parse().unwrap());
    *response.status_mut() = StatusCode::OK;

    Ok(response)
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

#[utoipa::path(
    get,
    path = "/files/{file_id}/cost-estimate",
    tag = "files",
    summary = "Get file cost estimate",
    description = "Estimate the cost of processing a batch file based on file size and model pricing. Returns per-model breakdown and total cost.",
    responses(
        (status = 200, description = "Cost estimate", body = FileCostEstimate),
        (status = 404, description = "File not found"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("file_id" = String, Path, description = "The ID of the file to estimate cost for")
    )
)]
#[tracing::instrument(skip(state, current_user), fields(user_id = %current_user.id, file_id = %file_id_str))]
pub async fn get_file_cost_estimate(
    State(state): State<AppState>,
    Path(file_id_str): Path<String>,
    current_user: RequiresPermission<resource::Files, operation::ReadOwn>,
) -> Result<Json<crate::api::models::files::FileCostEstimate>> {
    use rust_decimal::Decimal;
    use std::collections::HashMap;

    let can_read_all_files = can_read_all_resources(&current_user, Resource::Files);

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

    // Fetch all deployments and their pricing information upfront
    let mut conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut deployments_repo = Deployments::new(&mut conn);

    let filter = DeploymentFilter::new(0, 1000)
        .with_statuses(vec![ModelStatus::Active])
        .with_deleted(false);
    let all_deployments = deployments_repo.list(&filter).await.map_err(Error::Database)?;

    // Build a lookup map of model alias -> (deployment, avg_output_tokens, model_type)
    let mut model_info: HashMap<
        String,
        (
            crate::db::models::deployments::DeploymentDBResponse,
            Option<i64>,
            Option<crate::db::models::deployments::ModelType>,
        ),
    > = HashMap::new();

    for deployment in all_deployments {
        // Query http_analytics for last 100 responses to get average output token count
        let avg_output_tokens: Option<i64> = sqlx::query_scalar(
            r#"
            SELECT AVG(completion_tokens)::BIGINT
            FROM (
                SELECT completion_tokens
                FROM http_analytics
                WHERE model = $1
                  AND completion_tokens IS NOT NULL
                  AND status_code = 200
                ORDER BY timestamp DESC
                LIMIT 100
            ) recent_responses
            "#,
        )
        .bind(&deployment.alias)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| Error::Database(e.into()))?
        .flatten();

        model_info.insert(
            deployment.alias.clone(),
            (deployment.clone(), avg_output_tokens, deployment.model_type.clone()),
        );
    }

    // Get aggregated template statistics (optimized single query)
    let template_stats = state
        .request_manager
        .get_file_template_stats(fusillade::FileId(file_id))
        .await
        .map_err(|e| Error::Internal {
            operation: format!("get file template stats: {}", e),
        })?;

    // Convert to the format needed for cost calculation
    let mut model_stats: HashMap<String, (i64, i64)> = HashMap::new(); // (request_count, input_tokens)

    for stat in template_stats {
        // Estimate input tokens: body size in bytes / 4
        let estimated_input_tokens = stat.total_body_bytes / 4;
        model_stats.insert(stat.model, (stat.request_count, estimated_input_tokens));
    }

    let mut total_cost = Decimal::ZERO;
    let mut model_breakdowns = Vec::new();

    // Create tariffs repository once for all pricing lookups
    let mut tariffs_repo = Tariffs::new(&mut conn);
    let current_time = Utc::now();

    for (model_alias, (request_count, input_tokens)) in model_stats {
        // Look up the deployment and historical average
        let (deployment_opt, avg_output_tokens, model_type) = model_info
            .get(&model_alias)
            .map(|(d, avg, mt)| (Some(d.clone()), *avg, mt.clone()))
            .unwrap_or((None, None, None));

        // Calculate estimated output tokens using historical average or fallback heuristics
        let estimated_output_tokens = if matches!(model_type, Some(crate::db::models::deployments::ModelType::Embeddings)) {
            // Embedding models have minimal output
            request_count
        } else if let Some(avg) = avg_output_tokens {
            // Use historical average multiplied by request count
            avg * request_count
        } else {
            // Fallback: estimate 10% larger than input
            ((input_tokens as f64) * 1.1) as i64
        };

        let cost = if let Some(deployment) = deployment_opt {
            // Look up tariff pricing for Batch API key purpose, with fallback to realtime
            let pricing_result = tariffs_repo
                .get_pricing_at_timestamp_with_fallback(deployment.id, Some(&ApiKeyPurpose::Batch), &ApiKeyPurpose::Realtime, current_time)
                .await
                .map_err(Error::Database)?;

            if let Some((input_price, output_price)) = pricing_result {
                let input_cost = Decimal::from(input_tokens) * input_price;
                let output_cost = Decimal::from(estimated_output_tokens) * output_price;
                input_cost + output_cost
            } else {
                Decimal::ZERO
            }
        } else {
            // Model not found, cost is 0
            Decimal::ZERO
        };

        total_cost += cost;

        model_breakdowns.push(crate::api::models::files::ModelCostBreakdown {
            model: model_alias,
            request_count,
            estimated_input_tokens: input_tokens,
            estimated_output_tokens,
            estimated_cost: cost.to_string(),
        });
    }

    // Calculate totals
    let total_requests: i64 = model_breakdowns.iter().map(|m| m.request_count).sum();
    let total_input_tokens: i64 = model_breakdowns.iter().map(|m| m.estimated_input_tokens).sum();
    let total_output_tokens: i64 = model_breakdowns.iter().map(|m| m.estimated_output_tokens).sum();

    Ok(Json(crate::api::models::files::FileCostEstimate {
        file_id: file_id_str,
        total_requests,
        total_estimated_input_tokens: total_input_tokens,
        total_estimated_output_tokens: total_output_tokens,
        total_estimated_cost: total_cost.to_string(),
        models: model_breakdowns,
    }))
}

#[cfg(test)]
mod tests {
    use crate::api::models::files::FileResponse;
    use crate::api::models::users::Role;
    use crate::db::models::api_keys::ApiKeyPurpose;
    use crate::test_utils::*;
    use sqlx::PgPool;
    use uuid::Uuid;

    #[sqlx::test]
    #[test_log::test]
    async fn test_upload_and_download_file_content(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        // User needs BatchAPIUser role to create/read files
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create a deployment and add to group so user has access to the model
        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Create test JSONL content with 3 request templates in OpenAI Batch API format
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello 1"}]}}
{"custom_id":"request-2","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello 2"}]}}
{"custom_id":"request-3","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello 3"}]}}
"#;

        // Upload the file
        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");

        let upload_response = app
            .post("/ai/v1/files")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
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
            .get(&format!("/ai/v1/files/{}/content", file_id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
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
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        // Missing model field in body
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"messages":[{"role":"user","content":"Hello"}]}}"#;

        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");

        let upload_response = app
            .post("/ai/v1/files")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
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
    async fn test_upload_model_access_denied(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create a deployment for a different model than what's in the batch file
        let deployment = create_test_deployment(&pool, user.id, "allowed-model", "allowed-model").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Batch file requests a model the user doesn't have access to
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"unauthorized-model","messages":[{"role":"user","content":"Hello"}]}}"#;

        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");

        let upload_response = app
            .post("/ai/v1/files")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_text("purpose", "batch")
                    .add_part("file", file_part),
            )
            .await;

        // Should reject with 400 Bad Request due to model access denied
        upload_response.assert_status(axum::http::StatusCode::BAD_REQUEST);
        let error_body = upload_response.text();
        assert!(error_body.contains("Model"));
        assert!(error_body.contains("has not been configured or is not available to user."));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_upload_missing_custom_id(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        // Missing custom_id field
        let jsonl_content =
            r#"{"method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;

        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");

        let upload_response = app
            .post("/ai/v1/files")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
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
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        // Invalid JSON in body field (not an object)
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":"not a json object"}"#;

        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");

        let upload_response = app
            .post("/ai/v1/files")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
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
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        // Malformed JSONL - not valid JSON
        let jsonl_content = "this is not json at all\n{also not json}";

        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");

        let upload_response = app
            .post("/ai/v1/files")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
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
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        // Empty file
        let jsonl_content = "";

        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");

        let upload_response = app
            .post("/ai/v1/files")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
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
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create a deployment and add to group so user has access to the model
        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Create test JSONL content
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;

        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");

        // NOTE: The file field is added BEFORE the metadata fields (purpose, expires_after)
        // This tests whether the handler correctly processes metadata regardless of field order
        let upload_response = app
            .post("/ai/v1/files")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
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
            .get(&format!("/ai/v1/files/{}", file.id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        get_response.assert_status(axum::http::StatusCode::OK);
        let retrieved_file: FileResponse = get_response.json();
        assert_eq!(retrieved_file.purpose, crate::api::models::files::Purpose::Batch);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_upload_duplicate_filename(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create a deployment and add to group so user has access to the model
        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Create test JSONL content
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;

        // Upload first file with a specific filename
        let file_part1 = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("duplicate-test.jsonl");

        let upload_response1 = app
            .post("/ai/v1/files")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
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
            .post("/ai/v1/files")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
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

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_file_cost_estimate(pool: PgPool) {
        use rust_decimal::Decimal;
        use std::str::FromStr;

        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create two deployments with different pricing
        let deployment1 = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment1.id, group.id, user.id).await;

        let deployment2 = create_test_deployment(&pool, user.id, "gpt-3.5-model", "gpt-3.5").await;
        add_deployment_to_group(&pool, deployment2.id, group.id, user.id).await;

        // Set pricing for the models using tariffs
        use crate::db::handlers::Tariffs;
        use crate::db::models::tariffs::TariffCreateDBRequest;

        let mut conn = pool.acquire().await.unwrap();
        let mut tariffs_repo = Tariffs::new(&mut conn);

        // Create batch tariff for gpt-4
        tariffs_repo
            .create(&TariffCreateDBRequest {
                deployed_model_id: deployment1.id,
                name: "batch".to_string(),
                input_price_per_token: Decimal::from_str("0.00003").unwrap(), // $0.03 per 1K tokens
                output_price_per_token: Decimal::from_str("0.00006").unwrap(), // $0.06 per 1K tokens
                api_key_purpose: Some(ApiKeyPurpose::Batch),
                valid_from: None,
            })
            .await
            .unwrap();

        // Create batch tariff for gpt-3.5
        tariffs_repo
            .create(&TariffCreateDBRequest {
                deployed_model_id: deployment2.id,
                name: "batch".to_string(),
                input_price_per_token: Decimal::from_str("0.000001").unwrap(), // $0.001 per 1K tokens
                output_price_per_token: Decimal::from_str("0.000002").unwrap(), // $0.002 per 1K tokens
                api_key_purpose: Some(ApiKeyPurpose::Batch),
                valid_from: None,
            })
            .await
            .unwrap();

        drop(conn);

        // Create test JSONL content with mixed models
        // 2 requests for gpt-4, 1 request for gpt-3.5
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello 1"}]}}
{"custom_id":"request-2","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-3.5","messages":[{"role":"user","content":"Hello 2"}]}}
{"custom_id":"request-3","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello 3"}]}}
"#;

        // Upload the file
        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");

        let upload_response = app
            .post("/ai/v1/files")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_text("purpose", "batch")
                    .add_part("file", file_part),
            )
            .await;

        upload_response.assert_status(axum::http::StatusCode::CREATED);
        let file: FileResponse = upload_response.json();
        let file_id = file.id;

        // Get cost estimate
        let estimate_response = app
            .get(&format!("/ai/v1/files/{}/cost-estimate", file_id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        estimate_response.assert_status(axum::http::StatusCode::OK);
        let estimate: crate::api::models::files::FileCostEstimate = estimate_response.json();

        // Verify basic structure
        assert_eq!(estimate.file_id, file_id);
        assert_eq!(estimate.total_requests, 3);
        assert_eq!(estimate.models.len(), 2); // Two different models

        // Verify we have breakdowns for both models
        let gpt4_breakdown = estimate
            .models
            .iter()
            .find(|m| m.model == "gpt-4")
            .expect("Should have gpt-4 breakdown");
        let gpt35_breakdown = estimate
            .models
            .iter()
            .find(|m| m.model == "gpt-3.5")
            .expect("Should have gpt-3.5 breakdown");

        assert_eq!(gpt4_breakdown.request_count, 2);
        assert_eq!(gpt35_breakdown.request_count, 1);

        // Verify token estimates are calculated (input = body_bytes / 4, output = input * 1.2)
        assert!(gpt4_breakdown.estimated_input_tokens > 0);
        assert!(gpt4_breakdown.estimated_output_tokens > 0);
        assert!(gpt35_breakdown.estimated_input_tokens > 0);
        assert!(gpt35_breakdown.estimated_output_tokens > 0);

        // Verify costs are calculated (should be > 0 since we set pricing)
        let gpt4_cost = Decimal::from_str(&gpt4_breakdown.estimated_cost).unwrap();
        let gpt35_cost = Decimal::from_str(&gpt35_breakdown.estimated_cost).unwrap();
        assert!(gpt4_cost > Decimal::ZERO, "GPT-4 cost should be greater than zero");
        assert!(gpt35_cost > Decimal::ZERO, "GPT-3.5 cost should be greater than zero");

        // Verify total cost is sum of model costs
        let total_cost = Decimal::from_str(&estimate.total_estimated_cost).unwrap();
        assert_eq!(total_cost, gpt4_cost + gpt35_cost);

        // Verify totals match sum of breakdowns
        let total_input: i64 = estimate.models.iter().map(|m| m.estimated_input_tokens).sum();
        let total_output: i64 = estimate.models.iter().map(|m| m.estimated_output_tokens).sum();
        assert_eq!(estimate.total_estimated_input_tokens, total_input);
        assert_eq!(estimate.total_estimated_output_tokens, total_output);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_upload_invalid_http_method(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Use GET method instead of POST
        let jsonl_content = r#"{"custom_id":"request-1","method":"GET","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;

        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");

        let upload_response = app
            .post("/ai/v1/files")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_text("purpose", "batch")
                    .add_part("file", file_part),
            )
            .await;

        upload_response.assert_status(axum::http::StatusCode::BAD_REQUEST);
        let error_body = upload_response.text();
        assert!(error_body.contains("Unsupported HTTP method"));
        assert!(error_body.contains("GET"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_upload_invalid_url_path(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Use invalid URL path
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/api/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;

        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");

        let upload_response = app
            .post("/ai/v1/files")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_text("purpose", "batch")
                    .add_part("file", file_part),
            )
            .await;

        upload_response.assert_status(axum::http::StatusCode::BAD_REQUEST);
        let error_body = upload_response.text();
        assert!(error_body.contains("Unsupported URL path"));
        assert!(error_body.contains("/api/completions"));
        assert!(error_body.contains("/v1/chat/completions"));
        assert!(error_body.contains("/v1/embeddings"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_upload_strips_priority_field(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let deployment = create_test_deployment(&pool, user.id, "qwen-model", "Qwen/Qwen3-VL-30B-A3B-Instruct-FP8").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Upload file with priority field that user is trying to manipulate
        let jsonl_content = r#"{"custom_id": "priority-hijack", "method": "POST", "url": "/v1/chat/completions", "body": {"model": "Qwen/Qwen3-VL-30B-A3B-Instruct-FP8", "messages": [{"role": "user", "content": "urgent"}], "priority": -999999}}"#;

        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-priority.jsonl");

        let upload_response = app
            .post("/ai/v1/files")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_text("purpose", "batch")
                    .add_part("file", file_part),
            )
            .await;

        // Upload should succeed
        upload_response.assert_status(axum::http::StatusCode::CREATED);
        let file: FileResponse = upload_response.json();
        let file_id = file.id;

        // Download the file content and verify priority was stripped
        let download_response = app
            .get(&format!("/ai/v1/files/{}/content", file_id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        download_response.assert_status(axum::http::StatusCode::OK);
        let downloaded_content = download_response.text();

        // Parse the downloaded line
        let downloaded_json: serde_json::Value =
            serde_json::from_str(downloaded_content.trim()).expect("Downloaded content should be valid JSON");

        // Verify the body exists and doesn't contain priority
        let body = downloaded_json.get("body").expect("Should have body field");
        assert!(body.get("priority").is_none(), "Priority field should be stripped from body");

        // Verify other fields are preserved
        assert_eq!(
            body.get("model").and_then(|v| v.as_str()).unwrap(),
            "Qwen/Qwen3-VL-30B-A3B-Instruct-FP8"
        );
        assert!(body.get("messages").is_some(), "Messages field should be preserved");
    }

    /// Test that X-Incomplete is false for static batch input files (no pagination)
    #[sqlx::test]
    #[test_log::test]
    async fn test_x_incomplete_false_for_batch_input_file(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let deployment = create_test_deployment(&pool, user.id, "gpt-4", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Upload a batch input file with 3 requests
        let jsonl_content = r#"{"custom_id":"req-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hi"}]}}
{"custom_id":"req-2","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}
{"custom_id":"req-3","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hey"}]}}
"#;

        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test.jsonl");
        let upload_response = app
            .post("/ai/v1/files")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_text("purpose", "batch")
                    .add_part("file", file_part),
            )
            .await;

        upload_response.assert_status(axum::http::StatusCode::CREATED);
        let file: FileResponse = upload_response.json();

        // Download file content
        let response = app
            .get(&format!("/ai/v1/files/{}/content", file.id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status(axum::http::StatusCode::OK);

        // Batch input files are static - should be complete
        let incomplete_header = response.headers().get("x-incomplete");
        assert_eq!(
            incomplete_header.and_then(|h| h.to_str().ok()),
            Some("false"),
            "Batch input file should have X-Incomplete: false"
        );
    }

    /// Test that X-Incomplete is true when pagination indicates more data
    #[sqlx::test]
    #[test_log::test]
    async fn test_x_incomplete_true_with_pagination(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let deployment = create_test_deployment(&pool, user.id, "gpt-4", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Upload file with 5 requests
        let jsonl_content = r#"{"custom_id":"req-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Msg 1"}]}}
{"custom_id":"req-2","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Msg 2"}]}}
{"custom_id":"req-3","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Msg 3"}]}}
{"custom_id":"req-4","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Msg 4"}]}}
{"custom_id":"req-5","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Msg 5"}]}}
"#;

        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test.jsonl");
        let upload_response = app
            .post("/ai/v1/files")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_text("purpose", "batch")
                    .add_part("file", file_part),
            )
            .await;

        upload_response.assert_status(axum::http::StatusCode::CREATED);
        let file: FileResponse = upload_response.json();

        // Download with limit=2 (should have more data)
        let response = app
            .get(&format!("/ai/v1/files/{}/content?limit=2", file.id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status(axum::http::StatusCode::OK);

        // Should be incomplete due to pagination
        let incomplete_header = response.headers().get("x-incomplete");
        assert_eq!(
            incomplete_header.and_then(|h| h.to_str().ok()),
            Some("true"),
            "Should have X-Incomplete: true when paginated"
        );

        // Now fetch all data (no limit)
        let response = app
            .get(&format!("/ai/v1/files/{}/content", file.id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status(axum::http::StatusCode::OK);

        // Should be complete when no limit (fetched all)
        let incomplete_header = response.headers().get("x-incomplete");
        assert_eq!(
            incomplete_header.and_then(|h| h.to_str().ok()),
            Some("false"),
            "Should have X-Incomplete: false when all data fetched"
        );
    }

    /// Test that X-Incomplete reflects batch running status for output files
    #[sqlx::test]
    #[test_log::test]
    async fn test_x_incomplete_for_batch_output_file_running(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let deployment = create_test_deployment(&pool, user.id, "gpt-4", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Upload a file
        let jsonl_content = r#"{"custom_id":"req-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Test"}]}}
"#;
        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test.jsonl");
        let upload_response = app
            .post("/ai/v1/files")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_text("purpose", "batch")
                    .add_part("file", file_part),
            )
            .await;

        upload_response.assert_status(axum::http::StatusCode::CREATED);
        let file: FileResponse = upload_response.json();

        // Create a batch
        let batch_response = app
            .post("/ai/v1/batches")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .json(&serde_json::json!({
                "input_file_id": file.id,
                "endpoint": "/v1/chat/completions",
                "completion_window": "24h"
            }))
            .await;

        batch_response.assert_status(axum::http::StatusCode::CREATED);
        let batch: serde_json::Value = batch_response.json();
        let output_file_id = batch["output_file_id"].as_str().expect("Should have output_file_id");

        // Download output file content - batch is running (pending requests)
        let response = app
            .get(&format!("/ai/v1/files/{}/content", output_file_id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status(axum::http::StatusCode::OK);

        // Should be incomplete - batch has pending requests
        let incomplete_header = response.headers().get("x-incomplete");
        assert_eq!(
            incomplete_header.and_then(|h| h.to_str().ok()),
            Some("true"),
            "Output file should be incomplete while batch has pending requests"
        );
    }

    /// Test that X-Incomplete is false for output file when batch is complete
    #[sqlx::test]
    #[test_log::test]
    async fn test_x_incomplete_false_for_batch_output_file_complete(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let deployment = create_test_deployment(&pool, user.id, "gpt-4", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Upload a file
        let jsonl_content = r#"{"custom_id":"req-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Test"}]}}
"#;
        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test.jsonl");
        let upload_response = app
            .post("/ai/v1/files")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_text("purpose", "batch")
                    .add_part("file", file_part),
            )
            .await;

        upload_response.assert_status(axum::http::StatusCode::CREATED);
        let file: FileResponse = upload_response.json();

        // Create a batch
        let batch_response = app
            .post("/ai/v1/batches")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .json(&serde_json::json!({
                "input_file_id": file.id,
                "endpoint": "/v1/chat/completions",
                "completion_window": "24h"
            }))
            .await;

        batch_response.assert_status(axum::http::StatusCode::CREATED);
        let batch: serde_json::Value = batch_response.json();
        let batch_id = batch["id"].as_str().expect("Should have id");
        let output_file_id = batch["output_file_id"].as_str().expect("Should have output_file_id");

        // Manually complete all requests by updating their state in the database
        // Extract batch UUID from "batch_xxx" format
        let batch_uuid = batch_id.strip_prefix("batch_").unwrap_or(batch_id);
        let batch_uuid = Uuid::parse_str(batch_uuid).expect("Valid batch UUID");

        // Use unchecked query since fusillade schema is created at runtime
        // Must set completed_at to satisfy the completed_fields_check constraint
        sqlx::query(
            r#"
            UPDATE fusillade.requests
            SET state = 'completed', response_status = 200, response_body = '{"choices":[]}', completed_at = NOW()
            WHERE batch_id = $1
            "#,
        )
        .bind(batch_uuid)
        .execute(&pool)
        .await
        .expect("Failed to complete requests");

        // Download output file content - batch is now complete
        let response = app
            .get(&format!("/ai/v1/files/{}/content", output_file_id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status(axum::http::StatusCode::OK);

        // Should be complete - all requests finished
        let incomplete_header = response.headers().get("x-incomplete");
        assert_eq!(
            incomplete_header.and_then(|h| h.to_str().ok()),
            Some("false"),
            "Output file should be complete when batch has no pending/in-progress requests"
        );
    }
}
