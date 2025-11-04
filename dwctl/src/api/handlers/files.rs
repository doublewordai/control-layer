use crate::api::models::files::{FileDeleteResponse, FileListResponse, FileResponse, ListFilesQuery};
use crate::db::handlers::{
    files::{FileFilter, FilePurpose},
    Files, Repository,
};
use crate::db::models::files::{FileCreateDBRequest, FileStatus, StorageBackend};
use crate::errors::{Error, Result};
use crate::types::FileId;
use crate::AppState;
use axum::{
    body::Body,
    extract::{Multipart, Path, Query, State},
    http::{header, StatusCode},
    response::Response,
    Json,
};
use bytes::Bytes;
use chrono::{Duration, Utc};
use sqlx::Acquire;

// TODO: Replace with actual user ID from auth when implemented
fn placeholder_user_id() -> uuid::Uuid {
    uuid::Uuid::parse_str("00000000-0000-0000-0000-000000000000").unwrap()
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
pub async fn upload_file(State(state): State<AppState>, mut multipart: Multipart) -> Result<(StatusCode, Json<FileResponse>)> {
    let mut file_data: Option<(String, Bytes)> = None; // (filename, content)
    let mut purpose: Option<String> = None;
    let mut expires_after_anchor: Option<String> = None;
    let mut expires_after_seconds: Option<i64> = None;

    // Parse multipart fields
    while let Some(field) = multipart.next_field().await.map_err(|e| Error::BadRequest {
        message: format!("Failed to parse multipart data: {}", e),
    })? {
        let name = field.name().unwrap_or("").to_string();

        match name.as_str() {
            "file" => {
                let filename = field
                    .file_name()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| format!("batch_{}.jsonl", uuid::Uuid::new_v4()));

                let data = field.bytes().await.map_err(|e| Error::BadRequest {
                    message: format!("Failed to read file data: {}", e),
                })?;

                file_data = Some((filename, data));
            }
            "purpose" => {
                let value = field.text().await.map_err(|e| Error::BadRequest {
                    message: format!("Failed to read purpose: {}", e),
                })?;
                purpose = Some(value);
            }
            "expires_after[anchor]" => {
                let value = field.text().await.map_err(|e| Error::BadRequest {
                    message: format!("Failed to read expires_after[anchor]: {}", e),
                })?;
                expires_after_anchor = Some(value);
            }
            "expires_after[seconds]" => {
                let value = field.text().await.map_err(|e| Error::BadRequest {
                    message: format!("Failed to read expires_after[seconds]: {}", e),
                })?;
                let seconds = value.parse::<i64>().map_err(|_| Error::BadRequest {
                    message: "Invalid expires_after[seconds] value: must be an integer".to_string(),
                })?;
                expires_after_seconds = Some(seconds);
            }
            _ => {
                // Ignore unknown fields
            }
        }
    }

    // Validate required fields
    let (filename, content) = file_data.ok_or_else(|| Error::BadRequest {
        message: "Missing required field: 'file'".to_string(),
    })?;

    // Validate file size - return 413 if too large
    let max_file_size = state.config.file_storage.max_file_size as usize;
    if content.len() > max_file_size {
        return Err(Error::PayloadTooLarge {
            message: format!(
                "File size {} bytes exceeds maximum allowed size of {} bytes ({} MB)",
                content.len(),
                max_file_size,
                max_file_size / (1024 * 1024)
            ),
        });
    }

    if content.is_empty() {
        return Err(Error::BadRequest {
            message: "File cannot be empty".to_string(),
        });
    }

    // Validate JSONL format
    validate_jsonl(&content)?;

    // Parse purpose - only "batch" is supported for now
    let purpose_str = purpose.unwrap_or_else(|| "batch".to_string());
    if purpose_str != "batch" {
        return Err(Error::BadRequest {
            message: format!("Unsupported purpose '{}'. Only 'batch' is currently supported.", purpose_str),
        });
    }
    let file_purpose = FilePurpose::Batch;

    // Calculate expiration
    let expiry_seconds = if let Some(seconds) = expires_after_seconds {
        // Validate anchor if provided
        if let Some(anchor) = expires_after_anchor {
            if anchor != "created_at" {
                return Err(Error::BadRequest {
                    message: format!("Unsupported expires_after[anchor] '{}'. Only 'created_at' is supported.", anchor),
                });
            }
        }

        // Validate against server limits
        if seconds < state.config.file_storage.min_expiry_seconds {
            return Err(Error::BadRequest {
                message: format!(
                    "Expiration time {} seconds is below minimum of {} seconds (1 hour)",
                    seconds, state.config.file_storage.min_expiry_seconds
                ),
            });
        }

        // Cap at server maximum
        let capped_seconds = seconds.min(state.config.file_storage.max_expiry_seconds);

        // Warn if we capped it
        if capped_seconds < seconds {
            tracing::warn!("Capped expiration from {} to {} seconds (server maximum)", seconds, capped_seconds);
        }

        capped_seconds
    } else {
        // Use server default
        state.config.file_storage.default_expiry_seconds
    };

    let expires_at = Utc::now() + Duration::seconds(expiry_seconds);

    let content_type = "application/jsonl".to_string();
    let size_bytes = content.len() as i64;

    // Determine storage backend from config
    let storage_backend = match &state.config.file_storage.backend {
        crate::config::FileStorageBackend::Postgres { .. } => StorageBackend::Postgres,
        crate::config::FileStorageBackend::S3 { .. } => StorageBackend::S3,
        crate::config::FileStorageBackend::Local { .. } => StorageBackend::Local,
    };

    let mut tx = state.db.begin().await.map_err(|e| Error::Database(e.into()))?;

    // Create file with content
    let file = {
        let mut repo = Files::new(
            tx.acquire().await.map_err(|e| Error::Database(e.into()))?,
            state.file_storage.clone(),
        );

        let create_request = FileCreateDBRequest {
            filename,
            content_type,
            size_bytes,
            storage_backend,
            uploaded_by: placeholder_user_id(),
            purpose: file_purpose,
            expires_at: Some(expires_at),
            storage_key: String::new(), // Will be set by create_with_content
        };

        repo.create_with_content(&create_request, content.to_vec()).await?
    };

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    Ok((StatusCode::CREATED, Json(FileResponse::from_file(&file))))
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
pub async fn list_files(State(state): State<AppState>, Query(query): Query<ListFilesQuery>) -> Result<Json<FileListResponse>> {
    // Validate order parameter
    if query.order != "asc" && query.order != "desc" {
        return Err(Error::BadRequest {
            message: "Order must be 'asc' or 'desc'".to_string(),
        });
    }

    // Extract and validate limit
    let limit = query.limit.unwrap_or(10000).clamp(1, 10000);

    // Build filter with pagination & sorting at DB level
    let mut filter = FileFilter::new()
        .status(FileStatus::Active)
        .limit(limit + 1) // Fetch one extra to determine has_more
        .order_desc(query.order == "desc");

    // Add cursor if provided
    if let Some(after_id) = query.after {
        filter = filter.after(after_id);
    }

    // Add purpose filter if specified
    if let Some(purpose) = query.purpose {
        filter = filter.purpose(purpose);
    }

    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Files::new(&mut pool_conn, state.file_storage.clone());

    let mut files = repo.list(&filter).await?;

    // Check if there are more results
    let has_more = files.len() > limit as usize;
    if has_more {
        files.truncate(limit as usize);
    }

    let first_id = files.first().map(|f| f.id.to_string());
    let last_id = files.last().map(|f| f.id.to_string());

    let data: Vec<FileResponse> = files.iter().map(FileResponse::from_file).collect();

    Ok(Json(FileListResponse {
        object: "list".to_string(),
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
pub async fn get_file(State(state): State<AppState>, Path(file_id_str): Path<String>) -> Result<Json<FileResponse>> {
    let file_id = file_id_str.parse::<FileId>().map_err(|_| Error::BadRequest {
        message: "Invalid file ID format".to_string(),
    })?;

    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Files::new(&mut pool_conn, state.file_storage.clone());

    let file = repo.get_by_id(file_id).await?.ok_or_else(|| Error::NotFound {
        resource: "File".to_string(),
        id: file_id.to_string(),
    })?;

    // TODO: Add permission check when auth is implemented

    Ok(Json(FileResponse::from_file(&file)))
}

#[utoipa::path(
    get,
    path = "/files/{file_id}/content",
    tag = "files",
    summary = "Retrieve file content",
    description = "Returns the contents of the specified file.",
    responses(
        (status = 200, description = "File content", content_type = "application/jsonl"),
        (status = 404, description = "File not found"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("file_id" = String, Path, description = "The ID of the file")
    )
)]
pub async fn get_file_content(State(state): State<AppState>, Path(file_id_str): Path<String>) -> Result<Response> {
    let file_id = file_id_str.parse::<FileId>().map_err(|_| Error::BadRequest {
        message: "Invalid file ID format".to_string(),
    })?;

    let mut pool_conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Files::new(&mut pool_conn, state.file_storage.clone());

    // Get file metadata
    let file = repo.get_by_id(file_id).await?.ok_or_else(|| Error::NotFound {
        resource: "File".to_string(),
        id: file_id.to_string(),
    })?;

    // TODO: Add permission check when auth is implemented

    // Get content through repo
    let content = repo.get_content(&file).await?;

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, file.content_type.as_str())
        .header(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{}\"", file.filename))
        .body(Body::from(content))
        .unwrap())
}

#[utoipa::path(
    delete,
    path = "/files/{file_id}",
    tag = "files",
    summary = "Delete file",
    description = "Delete a file by ID. This performs a soft delete, removing the file content but retaining metadata.",
    responses(
        (status = 200, description = "File deleted successfully", body = FileDeleteResponse),
        (status = 404, description = "File not found"),
        (status = 500, description = "Internal server error")
    ),
    params(
        ("file_id" = String, Path, description = "The ID of the file to delete")
    )
)]
pub async fn delete_file(State(state): State<AppState>, Path(file_id_str): Path<String>) -> Result<Json<FileDeleteResponse>> {
    let file_id = file_id_str.parse::<FileId>().map_err(|_| Error::BadRequest {
        message: "Invalid file ID format".to_string(),
    })?;

    let mut tx = state.db.begin().await.map_err(|e| Error::Database(e.into()))?;

    {
        let mut repo = Files::new(
            tx.acquire().await.map_err(|e| Error::Database(e.into()))?,
            state.file_storage.clone(),
        );

        // Check if file exists
        let _file = repo.get_by_id(file_id).await?.ok_or_else(|| Error::NotFound {
            resource: "File".to_string(),
            id: file_id.to_string(),
        })?;

        // TODO: Add permission check when auth is implemented

        // Soft delete file (removes content, keeps metadata)
        repo.soft_delete(file_id).await?;
    }

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    Ok(Json(FileDeleteResponse {
        id: file_id.to_string(),
        object: "file".to_string(),
        deleted: true,
    }))
}

/// Validate that content is valid JSONL format
fn validate_jsonl(content: &[u8]) -> Result<()> {
    let content_str = std::str::from_utf8(content).map_err(|_| Error::BadRequest {
        message: "File must be valid UTF-8 text".to_string(),
    })?;

    if content_str.trim().is_empty() {
        return Err(Error::BadRequest {
            message: "File cannot be empty or contain only whitespace".to_string(),
        });
    }

    let mut line_count = 0;
    for (line_num, line) in content_str.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        serde_json::from_str::<serde_json::Value>(trimmed).map_err(|e| Error::BadRequest {
            message: format!("Invalid JSON on line {}: {}", line_num + 1, e),
        })?;

        line_count += 1;
    }

    if line_count == 0 {
        return Err(Error::BadRequest {
            message: "File must contain at least one valid JSON line".to_string(),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_jsonl_valid() {
        let content = b"{\"prompt\":\"Hello\"}\n{\"prompt\":\"World\"}";
        assert!(validate_jsonl(content).is_ok());
    }

    #[test]
    fn test_validate_jsonl_empty_lines() {
        let content = b"{\"prompt\":\"Hello\"}\n\n{\"prompt\":\"World\"}";
        assert!(validate_jsonl(content).is_ok());
    }

    #[test]
    fn test_validate_jsonl_invalid() {
        let content = b"{invalid}";
        assert!(validate_jsonl(content).is_err());
    }

    #[test]
    fn test_validate_jsonl_empty() {
        let content = b"";
        assert!(validate_jsonl(content).is_err());
    }

    #[test]
    fn test_validate_jsonl_only_whitespace() {
        let content = b"   \n  \n  ";
        assert!(validate_jsonl(content).is_err());
    }
}
