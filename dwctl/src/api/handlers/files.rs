use crate::api::models::files::{FileDeleteResponse, FileListResponse, FileResponse, ListFilesQuery, ListObject, ObjectType};
use crate::auth::permissions::{
    can_delete_own_resource, can_read_all_resources, can_read_own_resource, has_permission, operation, resource, RequiresPermission,
};
use crate::db::handlers::{files::FileFilter, Files, Repository};
use crate::db::models::files::{FileCreateDBRequest, FileStatus};
use crate::errors::{Error, Result};
use crate::types::{FileId, Operation, Resource};
use crate::AppState;
use axum::{
    extract::{Multipart, Path, Query, State},
    http::StatusCode,
    Json,
};
use chrono::{Duration, Utc};
use sqlx::Acquire;
use uuid::Uuid;

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
    mut multipart: Multipart,
) -> Result<(StatusCode, Json<FileResponse>)> {
    // Generate file_id upfront - we use this so we can store requests as they stream in
    // The whole file does not need to be stored in memory or disk
    // File metadata is stored once upload is complete
    let file_id = Uuid::new_v4();

    // Collect metadata as we parse multipart fields
    let mut filename: Option<String> = None;
    let mut purpose: Option<String> = None;
    let mut expires_after_anchor: Option<String> = None;
    let mut expires_after_seconds: Option<i64> = None;

    // Track file processing metrics
    let mut total_size = 0u64;
    let mut line_count = 0u64;
    let mut incomplete_line = String::new(); // For handling lines split across chunks

    // We can abort the upload as soon as we exceed max file size
    let max_file_size = state.config.files.max_file_size;

    // Begin transaction - if there is a failure in any chunk of the file, all stored data is rolled back
    let mut tx = state.db.begin().await.map_err(|e| Error::Database(e.into()))?;

    // Process multipart fields as they stream in
    while let Some(field) = multipart.next_field().await.map_err(|e| Error::BadRequest {
        message: format!("Failed to parse multipart data: {}", e),
    })? {
        let field_name = field.name().unwrap_or("").to_string();

        match field_name.as_str() {
            "file" => {
                filename = field
                    .file_name()
                    .map(|s| s.to_string())
                    .or_else(|| Some(format!("batch_{}.jsonl", file_id)));

                tracing::info!(
                    file_id = %file_id,
                    filename = ?filename,
                    "Starting file upload stream processing"
                );

                // Stream file chunks and process directly
                let mut chunk_stream = field;
                let mut chunk_number = 0;

                while let Some(chunk) = chunk_stream.chunk().await.map_err(|e| Error::BadRequest {
                    message: format!("Failed to read file chunk: {}", e),
                })? {
                    chunk_number += 1;
                    let chunk_size = chunk.len() as u64;
                    total_size += chunk_size;

                    tracing::debug!(
                        file_id = %file_id,
                        chunk_number = chunk_number,
                        chunk_size = chunk_size,
                        total_size = total_size,
                        "Processing chunk"
                    );

                    // Check size limit incrementally to fail fast
                    if total_size > max_file_size {
                        tracing::warn!(
                            file_id = %file_id,
                            total_size = total_size,
                            max_file_size = max_file_size,
                            "File size limit exceeded, aborting upload"
                        );
                        // Rollback happens automatically when tx is dropped
                        return Err(Error::PayloadTooLarge {
                            message: format!(
                                "File size exceeds maximum allowed size of {} bytes ({} MB)",
                                max_file_size,
                                max_file_size / (1024 * 1024)
                            ),
                        });
                    }

                    // Convert chunk to UTF-8
                    let chunk_str = std::str::from_utf8(&chunk).map_err(|_| Error::BadRequest {
                        message: "File must be valid UTF-8 text".to_string(),
                    })?;

                    // Combine with incomplete line from previous chunk
                    let text_to_process = if incomplete_line.is_empty() {
                        chunk_str.to_string()
                    } else {
                        tracing::trace!(
                            file_id = %file_id,
                            incomplete_line_length = incomplete_line.len(),
                            "Prepending incomplete line from previous chunk"
                        );
                        format!("{}{}", incomplete_line, chunk_str)
                    };

                    let mut lines = text_to_process.lines().peekable();
                    let ends_with_newline = chunk_str.ends_with('\n');
                    let mut lines_in_chunk = 0;

                    // Process complete lines
                    while let Some(line) = lines.next() {
                        let is_last_line = lines.peek().is_none();

                        // If this is the last line and chunk doesn't end with newline,
                        // it might be incomplete - save it for next chunk
                        if is_last_line && !ends_with_newline {
                            tracing::trace!(
                                file_id = %file_id,
                                line_length = line.len(),
                                "Saving incomplete line for next chunk"
                            );
                            incomplete_line = line.to_string();
                            break;
                        }

                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }

                        // Validate JSON structure
                        let _json_value = serde_json::from_str::<serde_json::Value>(trimmed).map_err(|e| {
                            tracing::error!(
                                file_id = %file_id,
                                line_number = line_count + 1,
                                error = %e,
                                "Invalid JSON in line"
                            );
                            Error::BadRequest {
                                message: format!("Invalid JSON on line {}: {}", line_count + 1, e),
                            }
                        })?;

                        // TODO: Parse line into Request object and store in database
                        // This is where we'll use PostgresRequestManager:
                        //
                        // let request: Request = serde_json::from_str(trimmed)?;
                        //
                        // PostgresRequestManager::store(
                        //     &mut tx,
                        //     file_id,  // Our generated file_id
                        //     vec![request]
                        // ).await?;
                        //
                        // For now, we drop the chunk

                        line_count += 1;
                        lines_in_chunk += 1;

                        // Clear incomplete line tracker since we processed it
                        if !incomplete_line.is_empty() {
                            incomplete_line.clear();
                        }
                    }

                    tracing::debug!(
                        file_id = %file_id,
                        chunk_number = chunk_number,
                        lines_processed = lines_in_chunk,
                        total_lines = line_count,
                        "Completed chunk processing"
                    );
                }

                // Process any remaining incomplete line at end of file
                if !incomplete_line.is_empty() {
                    tracing::debug!(
                        file_id = %file_id,
                        line_length = incomplete_line.len(),
                        "Processing final incomplete line"
                    );

                    let trimmed = incomplete_line.trim();
                    if !trimmed.is_empty() {
                        let _json_value = serde_json::from_str::<serde_json::Value>(trimmed).map_err(|e| {
                            tracing::error!(
                                file_id = %file_id,
                                error = %e,
                                "Invalid JSON in final line"
                            );
                            Error::BadRequest {
                                message: format!("Invalid JSON on final line: {}", e),
                            }
                        })?;

                        // TODO: Parse and store final request
                        // let request: Request = serde_json::from_str(trimmed)?;
                        // PostgresRequestManager::store(&mut tx, file_id, vec![request]).await?;

                        line_count += 1;
                    }
                }

                tracing::info!(
                    file_id = %file_id,
                    total_chunks = chunk_number,
                    total_lines = line_count,
                    total_bytes = total_size,
                    "Completed file stream processing"
                );
            }
            "purpose" => {
                purpose = Some(field.text().await.map_err(|e| Error::BadRequest {
                    message: format!("Failed to read purpose: {}", e),
                })?);
            }
            "expires_after[anchor]" => {
                expires_after_anchor = Some(field.text().await.map_err(|e| Error::BadRequest {
                    message: format!("Failed to read expires_after[anchor]: {}", e),
                })?);
            }
            "expires_after[seconds]" => {
                let value = field.text().await.map_err(|e| Error::BadRequest {
                    message: format!("Failed to read expires_after[seconds]: {}", e),
                })?;
                expires_after_seconds = Some(value.parse::<i64>().map_err(|_| Error::BadRequest {
                    message: "Invalid expires_after[seconds] value: must be an integer".to_string(),
                })?);
            }
            _ => {
                // Ignore unknown fields (forward compatibility)
            }
        }
    }

    // Validate we received required data
    let filename = filename.ok_or_else(|| Error::BadRequest {
        message: "Missing required field: 'file'".to_string(),
    })?;

    if total_size == 0 {
        return Err(Error::BadRequest {
            message: "File cannot be empty".to_string(),
        });
    }

    if line_count == 0 {
        return Err(Error::BadRequest {
            message: "File must contain at least one valid JSON line".to_string(),
        });
    }

    // Validate purpose
    let purpose_str = purpose.unwrap_or_else(|| "batch".to_string());
    if purpose_str != "batch" {
        return Err(Error::BadRequest {
            message: format!("Unsupported purpose '{}'. Only 'batch' is currently supported.", purpose_str),
        });
    }

    // Validate expires_after anchor if provided
    if let Some(anchor) = &expires_after_anchor {
        if anchor != "created_at" {
            return Err(Error::BadRequest {
                message: format!("Unsupported expires_after[anchor] '{}'. Only 'created_at' is supported.", anchor),
            });
        }
    }

    // Calculate expiration
    let expiry_seconds = if let Some(seconds) = expires_after_seconds {
        if seconds < state.config.files.min_expiry_seconds {
            return Err(Error::BadRequest {
                message: format!(
                    "Expiration time {} seconds is below minimum of {} seconds",
                    seconds, state.config.files.min_expiry_seconds
                ),
            });
        }

        let capped_seconds = seconds.min(state.config.files.max_expiry_seconds);

        if capped_seconds < seconds {
            tracing::warn!("Capped expiration from {} to {} seconds (server maximum)", seconds, capped_seconds);
        }

        capped_seconds
    } else {
        state.config.files.default_expiry_seconds
    };

    let expires_at = Utc::now() + Duration::seconds(expiry_seconds);

    // Create file metadata record with our pre-generated ID
    let file_record = {
        let mut repo = Files::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);

        let create_request = FileCreateDBRequest {
            id: file_id, // We generated the id, to sync with the requests, rather than lets postgres do it
            filename: filename.clone(),
            size_bytes: total_size as i64,
            uploaded_by: current_user.id,
            status: FileStatus::Processed,
            error_message: None,
            expires_at: Some(expires_at),
        };

        repo.create(&create_request).await?
    };

    // Commit transaction - atomically creates file metadata and all requests
    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    tracing::info!(
        "File {} uploaded successfully: {} bytes, {} lines",
        file_record.id,
        total_size,
        line_count
    );

    Ok((StatusCode::CREATED, Json(FileResponse::from_db_response(&file_record))))
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
    let skip = query.skip.unwrap_or(0).max(0);

    let mut filter = FileFilter::new(skip, limit + 1);

    if !has_system_access {
        filter = filter.with_status(FileStatus::Processed);
    }

    if !can_read_all_files {
        filter = filter.with_uploaded_by(current_user.id);
    }

    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Files::new(&mut pool_conn);

    let mut files = repo.list(&filter).await?;

    let has_more = files.len() > limit as usize;
    if has_more {
        files.truncate(limit as usize);
    }

    let first_id = files.first().map(|f| f.id.to_string());
    let last_id = files.last().map(|f| f.id.to_string());

    let data: Vec<FileResponse> = files.iter().map(FileResponse::from_db_response).collect();

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
pub async fn get_file(
    State(state): State<AppState>,
    Path(file_id_str): Path<String>,
    current_user: RequiresPermission<resource::Files, operation::ReadOwn>,
) -> Result<Json<FileResponse>> {
    let has_system_access = has_permission(&current_user, Resource::Files, Operation::SystemAccess);
    let can_read_all_files = can_read_all_resources(&current_user, Resource::Files);

    let file_id = file_id_str.parse::<FileId>().map_err(|_| Error::BadRequest {
        message: "Invalid file ID format".to_string(),
    })?;

    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Files::new(&mut pool_conn);

    let file = repo.get_by_id(file_id).await?.ok_or_else(|| Error::NotFound {
        resource: "File".to_string(),
        id: file_id.to_string(),
    })?;

    if !can_read_all_files && !can_read_own_resource(&current_user, Resource::Files, file.uploaded_by) {
        return Err(Error::NotFound {
            resource: "File".to_string(),
            id: file_id.to_string(),
        });
    }

    if !has_system_access && file.status != FileStatus::Processed {
        return Err(Error::NotFound {
            resource: "File".to_string(),
            id: file_id.to_string(),
        });
    }

    Ok(Json(FileResponse::from_db_response(&file)))
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

    let file_id = file_id_str.parse::<FileId>().map_err(|_| Error::BadRequest {
        message: "Invalid file ID format".to_string(),
    })?;

    let mut tx = state.db.begin().await.map_err(|e| Error::Database(e.into()))?;

    {
        let mut repo = Files::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);

        let file = repo.get_by_id(file_id).await?.ok_or_else(|| Error::NotFound {
            resource: "File".to_string(),
            id: file_id.to_string(),
        })?;

        if !can_delete_all_files && !can_delete_own_resource(&current_user, Resource::Files, file.uploaded_by) {
            return Err(Error::NotFound {
                resource: "File".to_string(),
                id: file_id.to_string(),
            });
        }

        repo.soft_delete(file_id).await?;
    }

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    Ok(Json(FileDeleteResponse {
        id: file_id.to_string(),
        object_type: ObjectType::File,
        deleted: true,
    }))
}
