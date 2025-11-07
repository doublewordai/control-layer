/// This file deals with the Files API.
/// This is designed to match (as far as possible) the OpenAI Files
/// [API](https://platform.openai.com/docs/api-reference/files/).
///
/// Repository methods are delegated to the fusillade/ crate - which (as of 04/11/2025) stores
/// files disaggregated in postgres.
use crate::api::models::files::{FileDeleteResponse, FileListResponse, FileResponse, ListFilesQuery, ListObject, ObjectType, Purpose};
use crate::auth::permissions::{can_read_all_resources, has_permission, operation, resource, RequiresPermission};
use crate::errors::{Error, Result};
use crate::types::{Operation, Resource};
use crate::AppState;
use axum::{
    extract::{Multipart, Path, Query, State},
    http::StatusCode,
    Json,
};
use fusillade::Storage;
use fusillade::Storage;
use futures::stream::Stream;
use std::pin::Pin;
use uuid::Uuid;

/// Helper function to create a stream of FileStreamItem from multipart upload
/// This handles the entire multipart parsing inside the stream
fn create_file_stream(
    mut multipart: Multipart,
    max_file_size: u64,
    uploaded_by: Option<String>,
) -> Pin<Box<dyn Stream<Item = fusillade::FileStreamItem> + Send>> {
    Box::pin(async_stream::stream! {
        let mut total_size = 0i64;
        let mut line_count = 0u64;
        let mut incomplete_line = String::new();
        let mut metadata = fusillade::FileMetadata {
            uploaded_by,
            ..Default::default()
        };

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
                    // TODO: What happens if the metadata fields are sent after the file field?
                    metadata.filename = field.file_name().map(|s| s.to_string());

                    // Yield metadata before processing file content
                    yield fusillade::FileStreamItem::Metadata(metadata.clone());

                    // Now stream and parse the file content
                    let mut field = field;

                    while let Ok(Some(chunk)) = field.chunk().await {
                        let chunk_size = chunk.len() as i64;
                        total_size += chunk_size;

                        // Check size limit - just log and stop, don't yield error
                        if total_size > max_file_size as i64 {
                            tracing::error!("File size exceeds maximum: {} > {}", total_size, max_file_size);
                            return;
                        }

                        // Convert chunk to UTF-8
                        let chunk_str = match std::str::from_utf8(&chunk) {
                            Ok(s) => s,
                            Err(_) => {
                                tracing::error!("File contains invalid UTF-8");
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

                            // Parse JSON line into RequestTemplateInput
                            match serde_json::from_str::<fusillade::RequestTemplateInput>(trimmed) {
                                Ok(template) => {
                                    line_count += 1;
                                    incomplete_line.clear();
                                    yield fusillade::FileStreamItem::Template(template);
                                }
                                Err(e) => {
                                    tracing::error!("Invalid JSON on line {}: {}", line_count + 1, e);
                                    return;
                                }
                            }
                        }
                    }

                    // Process any remaining incomplete line at end of file
                    if !incomplete_line.is_empty() {
                        let trimmed = incomplete_line.trim();
                        if !trimmed.is_empty() {
                            match serde_json::from_str::<fusillade::RequestTemplateInput>(trimmed) {
                                Ok(template) => yield fusillade::FileStreamItem::Template(template),
                                Err(e) => {
                                    tracing::error!("Invalid JSON on final line: {}", e);
                                }
                            }
                        }
                    }

                    // Yield final metadata with size
                    metadata.size_bytes = Some(total_size);
                    yield fusillade::FileStreamItem::Metadata(metadata.clone());

                    // File field processed, we're done
                    return;
                }
                _ => {
                    // Unknown field, skip it
                }
            }
        }
    })
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
pub async fn upload_file(
    State(state): State<AppState>,
    current_user: RequiresPermission<resource::Files, operation::CreateOwn>,
    multipart: Multipart,
) -> Result<(StatusCode, Json<FileResponse>)> {
    let max_file_size = state.config.files.max_file_size;
    let uploaded_by = Some(current_user.id.to_string());

    // Create a stream that parses the multipart upload and yields FileStreamItems
    let file_stream = create_file_stream(multipart, max_file_size, uploaded_by);

    // Create file via request manager with streaming
    let created_file_id = state
        .request_manager
        .create_file_stream(file_stream)
        .await
        .map_err(|e| Error::Internal {
            operation: format!("create file: {}", e),
        })?;

    tracing::info!("File {} uploaded successfully", created_file_id);

    // Build response using the fusillade file
    let file = state.request_manager.get_file(created_file_id).await.map_err(|e| Error::Internal {
        operation: format!("retrieve created file: {}", e),
    })?;

    // Validate purpose
    // TODO: Can we do this more rustily? I.e. using an enum in fusillade?
    let purpose_str = file.purpose.as_deref().unwrap_or("batch");
    if purpose_str != "batch" {
        return Err(Error::BadRequest {
            message: format!("Invalid purpose '{}'. Only 'batch' is supported.", purpose_str),
        });
    }

    Ok((
        StatusCode::CREATED,
        Json(FileResponse {
            id: file.id.to_string(),
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

    let first_id = files.first().map(|f| f.id.to_string());
    let last_id = files.last().map(|f| f.id.to_string());

    let data: Vec<FileResponse> = files
        .iter()
        .map(|f| FileResponse {
            id: f.id.to_string(),
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
// TODO: Does get file just get the stub? I.e. not the actual jsonl file?
// That sounds right, but we should also figure out a streaming download method for actually
// getting the file contents.
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
    if !has_system_access && file.status != "processed" {
        return Err(Error::NotFound {
            resource: "File".to_string(),
            id: file_id_str,
        });
    }

    Ok(Json(FileResponse {
        id: file.id.to_string(),
        object_type: ObjectType::File,
        bytes: file.size_bytes,
        created_at: file.created_at.timestamp(),
        filename: file.name,
        purpose: Purpose::Batch,
    }))
}

#[utoipa::path(
    delete,
    path = "/files/{file_id}",
    tag = "files",
    summary = "Delete file",
    description = "Delete a file by ID. This performs a soft delete, marking the file as deleted while retaining metadata.",
    responses(
        (status = 200, description = "File deleted successfully", body = FileDeleteResponse),
        (status = 404, description = "File not found"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("file_id" = String, Path, description = "The ID of the file to delete")
    )
)]
pub async fn delete_file(
    State(state): State<AppState>,
    Path(file_id_str): Path<String>,
    current_user: RequiresPermission<resource::Files, operation::DeleteOwn>,
) -> Result<Json<FileDeleteResponse>> {
    let can_delete_all_files = can_read_all_resources(&current_user, Resource::Files);

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
