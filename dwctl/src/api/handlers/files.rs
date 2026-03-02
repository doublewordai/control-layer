//! This file deals with the Files API.
//! This is designed to match (as far as possible) the OpenAI Files
//! [API](https://platform.openai.com/docs/api-reference/files/).
//!
//! Repository methods are delegated to the fusillade/ crate - which (as of 04/11/2025) stores
//! files disaggregated in postgres.

use sqlx_pool_router::PoolProvider;

use crate::api::models::files::{
    FileContentQuery, FileCostEstimate, FileCostEstimateQuery, FileDeleteResponse, FileListResponse, FileResponse, ListFilesQuery,
    ListObject, ObjectType, Purpose,
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
    body::Body,
    extract::{FromRequest, Multipart, Path, Query, State},
    http::StatusCode,
};
use bytes::Bytes;
use chrono::Utc;
use fusillade::Storage;
use futures::StreamExt;
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;
// Note: We import multer directly to get typed error matching for body limit errors.
// axum's multipart wraps multer, and checking error variants is more robust than string matching.
use crate::limits::MULTIPART_OVERHEAD;
use axum::extract::rejection::LengthLimitError;
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

fn validate_url_path(url: &str, allowed_url_paths: &[String]) -> Result<()> {
    if !allowed_url_paths.iter().any(|path| path == url) {
        return Err(Error::BadRequest {
            message: format!(
                "Unsupported URL path '{}'. Allowed paths are: {}",
                url,
                allowed_url_paths.join(", ")
            ),
        });
    }
    Ok(())
}

/// Maximum length for custom_id (matches OpenAI's limit)
const MAX_CUSTOM_ID_LENGTH: usize = 64;

/// Validate that a custom_id is safe to use as an HTTP header value.
fn validate_custom_id(custom_id: &str) -> Result<()> {
    if custom_id.len() > MAX_CUSTOM_ID_LENGTH {
        return Err(Error::BadRequest {
            message: format!(
                "custom_id exceeds maximum length of {} characters (got {})",
                MAX_CUSTOM_ID_LENGTH,
                custom_id.len()
            ),
        });
    }

    // Use the http crate's HeaderValue validation - same validation used by reqwest
    if axum::http::HeaderValue::from_str(custom_id).is_err() {
        return Err(Error::BadRequest {
            message: "custom_id contains invalid characters: must be valid ASCII without control characters".to_string(),
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
    fn to_internal(
        &self,
        endpoint: &str,
        api_key: String,
        accessible_models: &HashSet<String>,
        allowed_url_paths: &[String],
    ) -> Result<fusillade::RequestTemplateInput> {
        // Validate custom_id is safe for HTTP headers
        validate_custom_id(&self.custom_id)?;

        // Validate HTTP method
        let _validated_method = self.method.parse::<AllowedHttpMethod>()?;

        // Validate URL path
        validate_url_path(&self.url, allowed_url_paths)?;

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
            return Err(Error::ModelAccessDenied {
                model_name: model.clone(),
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
            api_key: api_key.to_string(),
        })
    }

    /// Transform internal format to OpenAI format
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

/// Configuration for file stream processing.
#[derive(Debug)]
struct FileStreamConfig {
    /// Maximum file size in bytes (0 = unlimited)
    max_file_size: u64,
    /// Maximum number of requests per file (0 = unlimited)
    max_requests_per_file: usize,
    /// Maximum body size in bytes for individual requests (0 = unlimited)
    max_request_body_size: u64,
    /// Channel buffer size for streaming
    buffer_size: usize,
}

/// Errors that can occur during file upload processing.
/// These are handled in control-layer, not round-tripped through fusillade.
#[derive(Debug, Clone)]
enum FileUploadError {
    /// HTTP stream was interrupted (connection dropped, body limit exceeded, etc.)
    StreamInterrupted { message: String },
    /// File exceeds the configured maximum size
    FileTooLarge { max: u64 },
    /// File contains too many requests
    TooManyRequests { count: usize, max: usize },
    /// Invalid JSON on a specific line
    InvalidJson { line: u64, error: String },
    /// Invalid UTF-8 encoding in the file
    InvalidUtf8 { line: u64, byte_offset: i64, error: String },
    /// No file field in multipart upload
    NoFile,
    /// File contains no valid request templates
    EmptyFile,
    /// User doesn't have access to a model referenced in the file
    ModelAccessDenied { model: String, line: u64 },
    /// Per-line validation error (custom_id, method, url, etc.)
    ValidationError { line: u64, message: String },
}

impl FileUploadError {
    /// Convert to the appropriate HTTP error type
    fn into_http_error(self) -> Error {
        match self {
            FileUploadError::StreamInterrupted { message } => {
                // Stream errors are server-side issues we can't determine the cause of
                Error::Internal {
                    operation: format!("upload file: {}", message),
                }
            }
            FileUploadError::FileTooLarge { max } => {
                if max == 0 {
                    // max_file_size=0 means unlimited, so this error shouldn't occur
                    // in normal operation. Treat as internal error.
                    Error::Internal {
                        operation: "upload file: unexpected size limit error with unlimited file size configured".to_string(),
                    }
                } else {
                    Error::PayloadTooLarge {
                        message: format!("File exceeds the maximum allowed size of {} bytes", max),
                    }
                }
            }
            FileUploadError::TooManyRequests { count, max } => Error::BadRequest {
                message: format!("File contains {} requests, which exceeds the maximum of {}", count, max),
            },
            FileUploadError::InvalidJson { line, error } => Error::BadRequest {
                message: format!("Invalid JSON on line {}: {}", line, error),
            },
            FileUploadError::InvalidUtf8 { line, byte_offset, error } => Error::BadRequest {
                message: format!(
                    "File contains invalid UTF-8 on/near line {} at byte offset {}: {}",
                    line, byte_offset, error
                ),
            },
            FileUploadError::NoFile => Error::BadRequest {
                message: "No file field found in multipart upload".to_string(),
            },
            FileUploadError::EmptyFile => Error::BadRequest {
                message: "File contains no valid request templates".to_string(),
            },
            FileUploadError::ModelAccessDenied { model, line } => Error::ModelAccessDenied {
                model_name: model.clone(),
                message: format!(
                    "Line {}: Model '{}' has not been configured or is not available to user",
                    line, model
                ),
            },
            FileUploadError::ValidationError { line, message } => Error::BadRequest {
                message: format!("Line {}: {}", line, message),
            },
        }
    }
}

/// Check if an error (or any error in its source chain) is a body/stream length limit error.
/// Uses typed error matching via downcasting, with string-based fallback for wrapped errors.
fn is_length_limit_error(err: &(dyn std::error::Error + 'static)) -> bool {
    // Check for axum's LengthLimitError directly
    if err.downcast_ref::<LengthLimitError>().is_some() {
        return true;
    }

    // Check for multer stream/field size exceeded
    if let Some(multer_err) = err.downcast_ref::<multer::Error>()
        && is_multer_length_limit(multer_err)
    {
        return true;
    }

    // String-based fallback: check error message for length limit indicators.
    // This catches cases where the error is wrapped in a way that prevents downcasting
    // (e.g., http_body_util::LengthLimitError wrapped in Box<dyn Error>).
    let err_string = err.to_string().to_lowercase();
    if err_string.contains("length limit exceeded") {
        return true;
    }

    // Recursively check the source chain
    if let Some(source) = std::error::Error::source(err) {
        return is_length_limit_error(source);
    }

    false
}

/// Check if a multer error indicates a length/size limit was exceeded.
fn is_multer_length_limit(err: &multer::Error) -> bool {
    match err {
        multer::Error::StreamSizeExceeded { .. } | multer::Error::FieldSizeExceeded { .. } => true,
        multer::Error::StreamReadFailed(boxed) => {
            // Recursively check the boxed error using the main function
            is_length_limit_error(boxed.as_ref())
        }
        _ => false,
    }
}

/// Result from create_file_stream: the stream and an error slot.
/// If the stream aborts, check the error slot for the typed error.
type FileStreamResult = (
    Pin<Box<dyn Stream<Item = fusillade::FileStreamItem> + Send>>,
    Arc<Mutex<Option<FileUploadError>>>,
);

/// Helper function to create a stream of FileStreamItem from multipart upload
/// This handles the entire multipart parsing inside the stream
///
/// # Arguments
/// * `endpoint` - Target endpoint for batch requests (e.g., "http://localhost:8080/ai")
/// * `api_key` - API key to inject for request execution
#[tracing::instrument(skip(multipart, api_key, accessible_models), fields(config.max_file_size, config.max_requests_per_file, uploaded_by = ?uploaded_by, endpoint = %endpoint, config.buffer_size))]
fn create_file_stream(
    mut multipart: Multipart,
    config: FileStreamConfig,
    uploaded_by: Option<String>,
    endpoint: String,
    api_key: String,
    accessible_models: HashSet<String>,
    allowed_url_paths: Vec<String>,
) -> FileStreamResult {
    let (tx, rx) = mpsc::channel(config.buffer_size);
    // std::sync::Mutex is appropriate here because:
    // 1. Lock is held only briefly (no await points while locked)
    // 2. No contention (only writer is spawned task, only reader is error handler)
    let error_slot: Arc<Mutex<Option<FileUploadError>>> = Arc::new(Mutex::new(None));
    let error_slot_clone = Arc::clone(&error_slot);

    tokio::spawn(async move {
        let mut total_size = 0u64;
        let mut line_count = 0u64;
        let mut incomplete_line = String::with_capacity(1024);
        let mut incomplete_utf8_bytes = Vec::with_capacity(4);
        let mut metadata = fusillade::FileMetadata {
            uploaded_by,
            ..Default::default()
        };
        let mut file_processed = false;

        /// Store error and signal abort to fusillade
        macro_rules! abort {
            ($error:expr) => {{
                // Use into_inner() on poisoned mutex - the data is still valid
                match error_slot_clone.lock() {
                    Ok(mut guard) => *guard = Some($error),
                    Err(poisoned) => *poisoned.into_inner() = Some($error),
                }
                let _ = tx.send(fusillade::FileStreamItem::Error("aborted".to_string())).await;
                return;
            }};
        }

        // Parse multipart fields
        loop {
            let field = match multipart.next_field().await {
                Ok(Some(field)) => field,
                Ok(None) => break, // No more fields
                Err(e) => {
                    // Check if this is a body limit error using typed matching
                    if is_length_limit_error(&e) {
                        abort!(FileUploadError::FileTooLarge { max: config.max_file_size });
                    } else {
                        abort!(FileUploadError::StreamInterrupted {
                            message: format!("Multipart parsing failed: {}", e),
                        });
                    }
                }
            };

            let field_name = field.name().unwrap_or("");

            match field_name {
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

                    loop {
                        match field.chunk().await {
                            Ok(Some(chunk)) => {
                                let chunk_size = chunk.len() as u64;
                                total_size += chunk_size;

                                tracing::debug!(
                                    "Processing chunk: {} bytes, total: {} bytes, lines so far: {}",
                                    chunk_size,
                                    total_size,
                                    line_count
                                );

                                // Check size limit (0 = unlimited)
                                if config.max_file_size > 0 && total_size > config.max_file_size {
                                    abort!(FileUploadError::FileTooLarge { max: config.max_file_size });
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
                                        incomplete_utf8_bytes.clear();
                                        (s.to_string(), Vec::new())
                                    }
                                    Err(e) => {
                                        let valid_up_to = e.valid_up_to();

                                        if e.error_len().is_some() {
                                            // Actual invalid UTF-8, not incomplete sequence
                                            let byte_offset = (total_size - chunk_size) as i64 + valid_up_to as i64;
                                            tracing::error!(
                                                "UTF-8 parsing error on/near line {}, byte offset {}",
                                                line_count + 1,
                                                byte_offset
                                            );

                                            abort!(FileUploadError::InvalidUtf8 {
                                                line: line_count + 1,
                                                byte_offset,
                                                error: e.to_string(),
                                            });
                                        }

                                        // Incomplete UTF-8 sequence at end - buffer for next chunk
                                        let valid_str = std::str::from_utf8(&combined_bytes[..valid_up_to])
                                            .expect("valid_up_to should point to valid UTF-8");
                                        let remaining = combined_bytes[valid_up_to..].to_vec();

                                        tracing::debug!("Incomplete UTF-8 sequence at chunk boundary, buffering {} bytes", remaining.len());

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

                                    // Check request count limit before parsing (0 = unlimited)
                                    if config.max_requests_per_file > 0 && line_count >= config.max_requests_per_file as u64 {
                                        abort!(FileUploadError::TooManyRequests {
                                            count: (line_count + 1).try_into().unwrap_or(usize::MAX),
                                            max: config.max_requests_per_file,
                                        });
                                    }

                                    // Parse JSON line as OpenAI Batch format, then transform to internal
                                    match serde_json::from_str::<OpenAIBatchRequest>(trimmed) {
                                        Ok(openai_req) => {
                                            // Transform to internal format (includes model access validation)
                                            match openai_req.to_internal(&endpoint, api_key.clone(), &accessible_models, &allowed_url_paths)
                                            {
                                                Ok(template) => {
                                                    // Check per-request body size limit (0 = unlimited)
                                                    if config.max_request_body_size > 0
                                                        && template.body.len() as u64 > config.max_request_body_size
                                                    {
                                                        abort!(FileUploadError::ValidationError {
                                                            line: line_count + 1,
                                                            message: format!(
                                                                "Request body is {} bytes, which exceeds the maximum allowed size of {} bytes",
                                                                template.body.len(),
                                                                config.max_request_body_size
                                                            ),
                                                        });
                                                    }

                                                    line_count += 1;
                                                    incomplete_line.clear();
                                                    if tx.send(fusillade::FileStreamItem::Template(template)).await.is_err() {
                                                        return;
                                                    }
                                                }
                                                Err(e) => {
                                                    // Map the Error back to FileUploadError
                                                    let upload_err = match &e {
                                                        Error::ModelAccessDenied { model_name, .. } => FileUploadError::ModelAccessDenied {
                                                            model: model_name.clone(),
                                                            line: line_count + 1,
                                                        },
                                                        _ => FileUploadError::ValidationError {
                                                            line: line_count + 1,
                                                            message: e.to_string(),
                                                        },
                                                    };
                                                    abort!(upload_err);
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            abort!(FileUploadError::InvalidJson {
                                                line: line_count + 1,
                                                error: e.to_string(),
                                            });
                                        }
                                    }
                                }
                            }
                            Ok(None) => {
                                // Normal end of stream
                                break;
                            }
                            Err(e) => {
                                // Log Display and Debug representations
                                tracing::warn!(
                                    error_display = %e,
                                    error_debug = ?e,
                                    "File upload stream error"
                                );

                                // Check if this is a body limit error using typed matching
                                if is_length_limit_error(&e) {
                                    abort!(FileUploadError::FileTooLarge { max: config.max_file_size });
                                } else {
                                    // Genuine stream error
                                    abort!(FileUploadError::StreamInterrupted { message: e.to_string() });
                                }
                            }
                        }
                    }

                    // Process any remaining incomplete line at end of file
                    if !incomplete_line.is_empty() {
                        let trimmed = incomplete_line.trim();
                        if !trimmed.is_empty() {
                            // Check request count limit
                            if config.max_requests_per_file > 0 && line_count >= config.max_requests_per_file as u64 {
                                abort!(FileUploadError::TooManyRequests {
                                    count: (line_count + 1).try_into().unwrap_or(usize::MAX),
                                    max: config.max_requests_per_file,
                                });
                            }

                            match serde_json::from_str::<OpenAIBatchRequest>(trimmed) {
                                Ok(openai_req) => {
                                    match openai_req.to_internal(&endpoint, api_key.clone(), &accessible_models, &allowed_url_paths) {
                                        Ok(template) => {
                                            // Check per-request body size limit (0 = unlimited)
                                            if config.max_request_body_size > 0 && template.body.len() as u64 > config.max_request_body_size
                                            {
                                                abort!(FileUploadError::ValidationError {
                                                    line: line_count + 1,
                                                    message: format!(
                                                        "Request body is {} bytes, which exceeds the maximum allowed size of {} bytes",
                                                        template.body.len(),
                                                        config.max_request_body_size
                                                    ),
                                                });
                                            }

                                            line_count += 1;
                                            if tx.send(fusillade::FileStreamItem::Template(template)).await.is_err() {
                                                return;
                                            }
                                        }
                                        Err(e) => {
                                            let upload_err = match &e {
                                                Error::ModelAccessDenied { model_name, .. } => FileUploadError::ModelAccessDenied {
                                                    model: model_name.clone(),
                                                    line: line_count + 1,
                                                },
                                                _ => FileUploadError::ValidationError {
                                                    line: line_count + 1,
                                                    message: e.to_string(),
                                                },
                                            };
                                            abort!(upload_err);
                                        }
                                    }
                                }
                                Err(e) => {
                                    abort!(FileUploadError::InvalidJson {
                                        line: line_count + 1,
                                        error: e.to_string(),
                                    });
                                }
                            }
                        }
                    }

                    // Check if file is empty (no templates parsed)
                    if line_count == 0 {
                        abort!(FileUploadError::EmptyFile);
                    }

                    metadata.size_bytes = match i64::try_from(total_size) {
                        Ok(size) => Some(size),
                        Err(_) => {
                            // File size exceeds i64::MAX (~9.2 exabytes) - treat as too large
                            abort!(FileUploadError::FileTooLarge { max: config.max_file_size });
                        }
                    };
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
            abort!(FileUploadError::NoFile);
        }

        // Send final metadata with all fields (including any that came after the file)
        let _ = tx.send(fusillade::FileStreamItem::Metadata(metadata.clone())).await;
    });

    (Box::pin(ReceiverStream::new(rx)), error_slot)
}

#[utoipa::path(
    post,
    path = "/files",
    tag = "files",
    summary = "Upload file",
    description = "Upload a JSONL file for batch processing.

Each line must be a valid JSON object containing `custom_id`, `method`, `url`, and `body` fields. The `model` field in the body must reference a model your API key has access to.",
    request_body(
        content_type = "multipart/form-data",
        description = "Multipart form with `file` (the JSONL file) and `purpose` (must be `batch`)."
    ),
    responses(
        (status = 201, description = "File uploaded and validated successfully.", body = FileResponse),
        (status = 400, description = "Invalid file format, malformed JSON, missing required fields, etc."),
        (status = 403, description = "Model referenced in the file is not configured or not accessible to your account."),
        (status = 413, description = "File exceeds the maximum allowed size."),
        (status = 429, description = "Too many concurrent uploads. Retry after a short delay."),
        (status = 500, description = "An unexpected error occurred. Retry the request or contact support if the issue persists.")
    )
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id))]
pub async fn upload_file<P: PoolProvider>(
    State(state): State<AppState<P>>,
    current_user: RequiresPermission<resource::Files, operation::CreateOwn>,
    request: axum::http::Request<axum::body::Body>,
) -> Result<(StatusCode, Json<FileResponse>)> {
    // Acquire upload permit (if limiter is configured)
    // The permit is held for the duration of the upload to limit concurrency
    let _permit = if let Some(ref limiter) = state.limiters.file_uploads {
        Some(limiter.acquire().await?)
    } else {
        None
    };

    let max_file_size = state.config.limits.files.max_file_size;

    // Early rejection based on Content-Length header (if present)
    // This avoids streaming a large file only to reject it later.
    // Note: Content-Length may be absent (chunked encoding) or spoofed,
    // so we still verify during streaming.
    // We add 10KB overhead for multipart encoding (boundaries, headers) to match
    // the DefaultBodyLimit layer configuration in lib.rs.
    if max_file_size > 0
        && let Some(content_length) = request.headers().get(axum::http::header::CONTENT_LENGTH)
        && let Ok(length_str) = content_length.to_str()
        && let Ok(length) = length_str.parse::<u64>()
        && length > max_file_size.saturating_add(MULTIPART_OVERHEAD)
    {
        return Err(Error::PayloadTooLarge {
            message: format!("File exceeds the maximum allowed size of {} bytes", max_file_size),
        });
    }

    // Extract multipart from request body
    let multipart = Multipart::from_request(request, &state).await.map_err(|e| Error::BadRequest {
        message: format!("Invalid multipart request: {}", e),
    })?;

    let stream_config = FileStreamConfig {
        max_file_size: state.config.limits.files.max_file_size,
        max_requests_per_file: state.config.limits.files.max_requests_per_file,
        max_request_body_size: state.config.limits.requests.max_body_size,
        buffer_size: state.config.batches.files.upload_buffer_size,
    };
    let uploaded_by = Some(current_user.id.to_string());

    // Get or create user-specific hidden batch API key for batch request execution
    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
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
    let (file_stream, error_slot) = create_file_stream(
        multipart,
        stream_config,
        uploaded_by,
        endpoint,
        user_api_key,
        accessible_models,
        state.config.batches.allowed_url_paths.clone(),
    );

    // Create file via request manager with streaming
    let created_file_id = state.request_manager.create_file_stream(file_stream).await.map_err(|e| {
        // Check if WE aborted (control-layer error in slot)
        // Handle poisoned mutex gracefully - the data is still valid
        let upload_err = match error_slot.lock() {
            Ok(mut guard) => guard.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(upload_err) = upload_err {
            tracing::warn!("File upload aborted with error: {:?}", upload_err);
            return upload_err.into_http_error();
        }

        // Otherwise it's a fusillade error
        tracing::warn!("Fusillade error during file upload: {:?}", e);
        match e {
            fusillade::FusilladeError::ValidationError(msg) => Error::BadRequest { message: msg },
            _ => Error::Internal {
                operation: format!("create file: {}", e),
            },
        }
    })?;

    tracing::debug!("File {} uploaded successfully", created_file_id);

    // Build response using the fusillade file
    // We use the primary pool to avoid transaction or read lags if using replicas
    let file = state
        .request_manager
        .get_file_from_primary_pool(created_file_id)
        .await
        .map_err(|e| Error::Internal {
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
    description = "Returns a paginated list of your uploaded files.

Use cursor-based pagination: pass `last_id` from the response as the `after` parameter to fetch the next page.",
    responses(
        (status = 200, description = "List of files. Check `has_more` to determine if additional pages exist.", body = FileListResponse),
        (status = 500, description = "An unexpected error occurred. Retry the request or contact support if the issue persists.")
    ),
    params(
        ListFilesQuery
    )
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id))]
pub async fn list_files<P: PoolProvider>(
    State(state): State<AppState<P>>,
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
        // Filter by ownership if user can't read all files, or if explicitly requested
        uploaded_by: if !can_read_all_files || query.own {
            Some(current_user.id.to_string())
        } else {
            None
        },
        // No status filtering
        status: None,
        purpose: query.purpose.clone(),
        search: query.search.clone(),
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
    description = "Returns metadata about a specific file, including its size, creation time, and purpose.",
    responses(
        (status = 200, description = "File metadata.", body = FileResponse),
        (status = 404, description = "File not found or you don't have access to it."),
        (status = 500, description = "An unexpected error occurred. Retry the request or contact support if the issue persists.")
    ),
    params(
        ("file_id" = String, Path, description = "The file ID returned when the file was uploaded.")
    )
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id, file_id = %file_id_str))]
pub async fn get_file<P: PoolProvider>(
    State(state): State<AppState<P>>,
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
    description = "Download the content of a file as JSONL.

For input files, returns the original request templates. For output files, returns the completed responses. Supports pagination via `limit` and `offset` query parameters.",
    responses(
        (status = 200, description = "File content as newline-delimited JSON. Check the `X-Incomplete` header to determine if more content exists.", content_type = "application/x-ndjson"),
        (status = 404, description = "File not found or you don't have access to it."),
        (status = 500, description = "An unexpected error occurred. Retry the request or contact support if the issue persists.")
    ),
    params(
        ("file_id" = String, Path, description = "The file ID returned when the file was uploaded."),
        FileContentQuery
    )
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id, file_id = %file_id_str))]
pub async fn get_file_content<P: PoolProvider>(
    State(state): State<AppState<P>>,
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

    // For BatchOutput and BatchError files, check if the batch is still running
    // (which means more data may be written to this file in the future).
    // Also capture the expected content count for streaming X-Last-Line.
    let (file_may_receive_more_data, file_content_count) = match file.purpose {
        Some(fusillade::batch::Purpose::Batch) => (false, None), // Input files: count unknown without query
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
                let still_processing = status.pending_requests > 0 || status.in_progress_requests > 0;
                (still_processing, Some(status.completed_requests as usize))
            } else {
                (false, None)
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
                let still_processing = status.pending_requests > 0 || status.in_progress_requests > 0;
                (still_processing, Some(status.failed_requests as usize))
            } else {
                (false, None)
            }
        }
        None => (false, None), // Shouldn't happen, but assume complete
    };

    // Stream the file content as JSONL, starting from offset
    let offset = query.pagination.skip() as usize;
    let search = query.search.clone();
    let requested_limit = query.pagination.limit.map(|_| query.pagination.limit() as usize);

    // Helper to serialize FileContentItem to JSON line
    fn serialize_content_item(content_item: fusillade::FileContentItem) -> fusillade::Result<String> {
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
            fusillade::FileContentItem::Output(output) => serde_json::to_string(&output)
                .map(|json| format!("{}\n", json))
                .map_err(|e| fusillade::FusilladeError::Other(anyhow::anyhow!("JSON serialization failed: {}", e))),
            fusillade::FileContentItem::Error(error) => serde_json::to_string(&error)
                .map(|json| format!("{}\n", json))
                .map_err(|e| fusillade::FusilladeError::Other(anyhow::anyhow!("JSON serialization failed: {}", e))),
        }
    }

    if let Some(limit) = requested_limit {
        // Pagination case: buffer only N+1 items to check for more pages
        let content_stream = state
            .request_manager
            .get_file_content_stream(fusillade::FileId(file_id), offset, search);

        let mut buffer: Vec<_> = content_stream.take(limit + 1).collect().await;
        let has_more_pages = buffer.len() > limit;
        buffer.truncate(limit);

        let line_count = buffer.len();
        let last_line = offset + line_count;
        let has_more = has_more_pages || file_may_receive_more_data;

        // Serialize buffered items
        let mut jsonl_lines = Vec::new();
        for content_result in buffer {
            let json_line = content_result.and_then(serialize_content_item).map_err(|e| Error::Internal {
                operation: format!("serialize content: {}", e),
            })?;
            jsonl_lines.push(json_line);
        }

        let jsonl_content = jsonl_lines.join("");

        let mut response = axum::response::Response::new(Body::from(jsonl_content));
        response
            .headers_mut()
            .insert("content-type", "application/x-ndjson".parse().unwrap());
        response.headers_mut().insert("X-Incomplete", has_more.to_string().parse().unwrap());
        response.headers_mut().insert("X-Last-Line", last_line.to_string().parse().unwrap());
        *response.status_mut() = StatusCode::OK;

        Ok(response)
    } else {
        // Unlimited case: true streaming to avoid OOM on large result sets
        //
        // Derive expected count from batch status when available, so we can
        // set X-Last-Line before streaming. Search filters make the count
        // unknown, so we skip X-Last-Line in that case.
        let expected_count = if search.is_none() {
            file_content_count.map(|c| c.saturating_sub(offset))
        } else {
            None
        };

        let content_stream = state
            .request_manager
            .get_file_content_stream(fusillade::FileId(file_id), offset, search);

        // Limit stream to expected count so X-Last-Line is accurate
        let content_stream: Pin<Box<dyn Stream<Item = fusillade::Result<fusillade::FileContentItem>> + Send>> =
            if let Some(count) = expected_count {
                Box::pin(content_stream.take(count))
            } else {
                Box::pin(content_stream)
            };

        let body_stream = content_stream.map(|result| {
            result
                .and_then(|item| serialize_content_item(item).map(Bytes::from))
                .map_err(|e| std::io::Error::other(e.to_string()))
        });

        let body = Body::from_stream(body_stream);
        let mut response = axum::response::Response::new(body);
        response
            .headers_mut()
            .insert("content-type", "application/x-ndjson".parse().unwrap());
        response
            .headers_mut()
            .insert("X-Incomplete", file_may_receive_more_data.to_string().parse().unwrap());
        if let Some(count) = expected_count {
            let last_line = offset + count;
            response.headers_mut().insert("X-Last-Line", last_line.to_string().parse().unwrap());
        }
        *response.status_mut() = StatusCode::OK;

        Ok(response)
    }
}

#[utoipa::path(
    delete,
    path = "/files/{file_id}",
    tag = "files",
    summary = "Delete file",
    description = "Permanently delete a file.

Deleting a file also deletes any batches that were created from it. This action cannot be undone.",
    responses(
        (status = 200, description = "File deleted successfully.", body = FileDeleteResponse),
        (status = 404, description = "File not found or you don't have access to it."),
        (status = 500, description = "An unexpected error occurred. Retry the request or contact support if the issue persists.")
    ),
    params(
        ("file_id" = String, Path, description = "The file ID returned when the file was uploaded.")
    )
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id, file_id = %file_id_str))]
pub async fn delete_file<P: PoolProvider>(
    State(state): State<AppState<P>>,
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
    description = "Estimate the cost of processing a batch file before creating a batch.

Returns a breakdown by model including estimated input/output tokens and cost. Useful for validating costs before committing to a batch run.",
    responses(
        (status = 200, description = "Cost estimate with per-model breakdown.", body = FileCostEstimate),
        (status = 404, description = "File not found or you don't have access to it."),
        (status = 500, description = "An unexpected error occurred. Retry the request or contact support if the issue persists.")
    ),
    params(
        ("file_id" = String, Path, description = "The ID of the file to estimate cost for"),
        FileCostEstimateQuery
    )
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id, file_id = %file_id_str))]
pub async fn get_file_cost_estimate<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(file_id_str): Path<String>,
    Query(query): Query<FileCostEstimateQuery>,
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

    // Get aggregated template statistics first to know which models are in the file
    let template_stats = state
        .request_manager
        .get_file_template_stats(fusillade::FileId(file_id))
        .await
        .map_err(|e| Error::Internal {
            operation: format!("get file template stats: {}", e),
        })?;

    // Convert to the format needed for cost calculation
    let mut model_stats: HashMap<String, (i64, i64)> = HashMap::new(); // (request_count, input_tokens)

    for stat in &template_stats {
        // Estimate input tokens: body size in bytes / 4
        let estimated_input_tokens = stat.total_body_bytes / 4;
        model_stats.insert(stat.model.clone(), (stat.request_count, estimated_input_tokens));
    }

    // Get the list of models actually used in this file
    let models_in_file: Vec<String> = template_stats.iter().map(|s| s.model.clone()).collect();

    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut deployments_repo = Deployments::new(&mut conn);

    // Only fetch deployments for models in the file
    let filter = DeploymentFilter::new(0, 1000)
        .with_statuses(vec![ModelStatus::Active])
        .with_deleted(false);
    let all_deployments = deployments_repo.list(&filter).await.map_err(Error::Database)?;

    // Build a lookup map of model alias -> (deployment, avg_output_tokens, model_type)
    // Only for models that are actually in the file
    let mut model_info: HashMap<
        String,
        (
            crate::db::models::deployments::DeploymentDBResponse,
            Option<i64>,
            Option<crate::db::models::deployments::ModelType>,
        ),
    > = HashMap::new();

    for deployment in all_deployments {
        // Skip deployments not used in this file
        if !models_in_file.contains(&deployment.alias) {
            model_info.insert(deployment.alias.clone(), (deployment.clone(), None, deployment.model_type.clone()));
            continue;
        }

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

    let mut total_cost = Decimal::ZERO;
    let mut model_breakdowns = Vec::new();

    // Create tariffs repository once for all pricing lookups
    let mut tariffs_repo = Tariffs::new(&mut conn);
    let current_time = Utc::now();

    // Use the completion_window from query params, defaulting to "24h"
    let completion_window = query.completion_window.as_deref().unwrap_or("24h");

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
                .get_pricing_at_timestamp_with_fallback(
                    deployment.id,
                    Some(&ApiKeyPurpose::Batch),
                    &ApiKeyPurpose::Realtime,
                    current_time,
                    Some(completion_window),
                )
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
    use crate::test::utils::*;
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

        // Should reject with 403 Forbidden due to model access denied
        upload_response.assert_status(axum::http::StatusCode::FORBIDDEN);
        let error_body = upload_response.text();
        assert!(error_body.contains("Model"));
        assert!(error_body.contains("has not been configured or is not available to user"));
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
    async fn test_upload_custom_id_with_control_characters(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;
        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // custom_id with newline (header injection attempt)
        let jsonl_content = "{\"custom_id\":\"request-1\\r\\nX-Injected: malicious\",\"method\":\"POST\",\"url\":\"/v1/chat/completions\",\"body\":{\"model\":\"gpt-4\",\"messages\":[{\"role\":\"user\",\"content\":\"Hello\"}]}}";

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
        assert!(error_body.contains("invalid characters"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_upload_custom_id_too_long(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;
        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // custom_id exceeding 64 character limit
        let long_id = "a".repeat(65);
        let jsonl_content = format!(
            r#"{{"custom_id":"{}","method":"POST","url":"/v1/chat/completions","body":{{"model":"gpt-4","messages":[{{"role":"user","content":"Hello"}}]}}}}"#,
            long_id
        );

        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes().to_vec()).file_name("test-batch.jsonl");

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
        assert!(error_body.contains("exceeds maximum length"));
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
                completion_window: Some("24h".to_string()),
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
                completion_window: Some("24h".to_string()),
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
    async fn test_get_file_cost_estimate_with_different_slas(pool: PgPool) {
        use rust_decimal::Decimal;
        use std::str::FromStr;

        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create deployment
        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Set pricing for different SLAs
        use crate::db::handlers::Tariffs;
        use crate::db::models::tariffs::TariffCreateDBRequest;

        let mut conn = pool.acquire().await.unwrap();
        let mut tariffs_repo = Tariffs::new(&mut conn);

        // Create batch tariff for 24h priority (standard pricing)
        tariffs_repo
            .create(&TariffCreateDBRequest {
                deployed_model_id: deployment.id,
                name: "batch-24h".to_string(),
                input_price_per_token: Decimal::from_str("0.00003").unwrap(), // $0.03 per 1K tokens
                output_price_per_token: Decimal::from_str("0.00006").unwrap(), // $0.06 per 1K tokens
                api_key_purpose: Some(ApiKeyPurpose::Batch),
                completion_window: Some("24h".to_string()),
                valid_from: None,
            })
            .await
            .unwrap();

        // Create batch tariff for 1h priority (higher pricing for faster turnaround)
        tariffs_repo
            .create(&TariffCreateDBRequest {
                deployed_model_id: deployment.id,
                name: "batch-1h".to_string(),
                input_price_per_token: Decimal::from_str("0.00006").unwrap(), // $0.06 per 1K tokens (2x)
                output_price_per_token: Decimal::from_str("0.00012").unwrap(), // $0.12 per 1K tokens (2x)
                api_key_purpose: Some(ApiKeyPurpose::Batch),
                completion_window: Some("1h".to_string()),
                valid_from: None,
            })
            .await
            .unwrap();

        drop(conn);

        // Create test JSONL content
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;

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

        // Get cost estimate with default (24h) priority
        let estimate_24h_response = app
            .get(&format!("/ai/v1/files/{}/cost-estimate", file_id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        estimate_24h_response.assert_status(axum::http::StatusCode::OK);
        let estimate_24h: crate::api::models::files::FileCostEstimate = estimate_24h_response.json();

        // Get cost estimate with 1h priority
        let estimate_1h_response = app
            .get(&format!("/ai/v1/files/{}/cost-estimate?completion_window=1h", file_id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        estimate_1h_response.assert_status(axum::http::StatusCode::OK);
        let estimate_1h: crate::api::models::files::FileCostEstimate = estimate_1h_response.json();

        // Verify both estimates have the same token counts
        assert_eq!(estimate_24h.total_estimated_input_tokens, estimate_1h.total_estimated_input_tokens);
        assert_eq!(
            estimate_24h.total_estimated_output_tokens,
            estimate_1h.total_estimated_output_tokens
        );

        // Verify 1h priority costs more than 24h priority (should be 2x)
        let cost_24h = Decimal::from_str(&estimate_24h.total_estimated_cost).unwrap();
        let cost_1h = Decimal::from_str(&estimate_1h.total_estimated_cost).unwrap();

        assert!(cost_1h > cost_24h, "1h priority should cost more than 24h priority");
        assert!(cost_24h > Decimal::ZERO, "24h priority cost should be greater than zero");

        // Verify the ratio is approximately 2x (allowing for rounding)
        let ratio = cost_1h / cost_24h;
        assert!(
            ratio > Decimal::from_str("1.9").unwrap() && ratio < Decimal::from_str("2.1").unwrap(),
            "1h priority should be approximately 2x the cost of 24h priority, got ratio: {}",
            ratio
        );
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
        assert!(error_body.contains("/v1/responses"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_upload_accepts_responses_url_path(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/responses","body":{"model":"gpt-4","input":"Hello"}}"#;

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

    #[tokio::test]
    async fn test_upload_rate_limiting_rejects_when_queue_full() {
        use crate::config::FileLimitsConfig;
        use crate::limits::UploadLimiter;
        use std::sync::Arc;

        // Test the limiter directly with max_concurrent=1, max_waiting=1
        let config = FileLimitsConfig {
            max_concurrent_uploads: 1,
            max_waiting_uploads: 1,  // Only allow 1 waiter
            max_upload_wait_secs: 0, // Reject immediately when can't acquire
            ..Default::default()
        };
        let limiter = Arc::new(UploadLimiter::new(&config).unwrap());

        // Acquire the only permit
        let _permit1 = limiter.acquire().await.unwrap();

        // Second request joins the waiting queue (1 allowed)
        let limiter_clone = limiter.clone();
        let handle = tokio::spawn(async move { limiter_clone.acquire().await });

        // Give time for the waiter to enter queue
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Third request should be rejected (queue full)
        let result = limiter.acquire().await;
        assert!(result.is_err(), "Third request should be rejected when queue is full");

        if let Err(crate::errors::Error::TooManyRequests { message }) = result {
            assert!(message.contains("Too many file uploads"));
        } else {
            panic!("Expected TooManyRequests error");
        }

        // Clean up
        drop(_permit1);
        let _ = handle.await;
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_upload_with_rate_limiter_configured(pool: PgPool) {
        // Create app with rate limiting enabled
        let mut config = create_test_config();
        config.limits.files.max_concurrent_uploads = 10; // High enough to not block this test
        config.limits.files.max_waiting_uploads = 20;
        config.limits.files.max_upload_wait_secs = 60;

        let (app, _bg_services) = create_test_app_with_config(pool.clone(), config, false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create a deployment and add to group so user has access to the model
        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Create test JSONL content
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;

        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test.jsonl");

        // Upload should succeed when rate limiter is configured but not at capacity
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
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_upload_rejects_file_exceeding_max_requests(pool: PgPool) {
        // Create app with max_requests_per_file = 2
        let mut config = create_test_config();
        config.limits.files.max_requests_per_file = 2;

        let (app, _bg_services) = create_test_app_with_config(pool.clone(), config, false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create a deployment and add to group so user has access to the model
        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Create JSONL content with 3 requests (exceeds limit of 2)
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}
{"custom_id":"request-2","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}
{"custom_id":"request-3","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;

        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test.jsonl");

        // Upload should fail because file has 3 requests but limit is 2
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
        let body = upload_response.text();
        assert!(
            body.contains("exceeds the maximum of"),
            "Expected error about request limit, got: {}",
            body
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_upload_allows_file_at_max_requests(pool: PgPool) {
        // Create app with max_requests_per_file = 2
        let mut config = create_test_config();
        config.limits.files.max_requests_per_file = 2;

        let (app, _bg_services) = create_test_app_with_config(pool.clone(), config, false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create a deployment and add to group so user has access to the model
        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Create JSONL content with exactly 2 requests (at the limit)
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}
{"custom_id":"request-2","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;

        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test.jsonl");

        // Upload should succeed because file has exactly 2 requests (at the limit)
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
    }

    /// Regression test for streaming file content (output files).
    ///
    /// Previously, get_file_content collected ALL items into memory before
    /// sending the response. This test verifies that:
    /// 1. Unlimited downloads (no limit param) return all results correctly
    /// 2. Unlimited responses use streaming (no content-length header)
    /// 3. Paginated downloads return correct subset and headers
    /// 4. Each line is valid JSON
    #[sqlx::test]
    #[test_log::test]
    async fn test_file_content_streaming(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let deployment = create_test_deployment(&pool, user.id, "gpt-4", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Build a file with many requests
        let num_requests = 30;
        let jsonl_lines: Vec<String> = (0..num_requests)
            .map(|i| {
                format!(
                    r#"{{"custom_id":"req-{}","method":"POST","url":"/v1/chat/completions","body":{{"model":"gpt-4","messages":[{{"role":"user","content":"Test {}"}}]}}}}"#,
                    i, i
                )
            })
            .collect();
        let jsonl_content = jsonl_lines.join("\n") + "\n";

        // Upload
        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes().to_vec()).file_name("test.jsonl");
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

        // Create batch to get an output file
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
        let batch_id_str = batch["id"].as_str().expect("Should have id");
        let output_file_id = batch["output_file_id"].as_str().expect("Should have output_file_id");

        // Complete all requests so the output file has content
        let batch_uuid_str = batch_id_str.strip_prefix("batch_").unwrap_or(batch_id_str);
        let batch_uuid = Uuid::parse_str(batch_uuid_str).expect("Valid batch UUID");

        sqlx::query(
            r#"
            UPDATE fusillade.requests
            SET state = 'completed', response_status = 200,
                response_body = '{"choices":[{"message":{"content":"ok"}}]}',
                completed_at = NOW()
            WHERE batch_id = $1
            "#,
        )
        .bind(batch_uuid)
        .execute(&pool)
        .await
        .expect("Failed to complete requests");

        let auth = add_auth_headers(&user);

        // Test 1: Unlimited download (streaming path)
        let response = app
            .get(&format!("/ai/v1/files/{}/content", output_file_id))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;

        response.assert_status(axum::http::StatusCode::OK);
        response.assert_header("content-type", "application/x-ndjson");
        response.assert_header("X-Incomplete", "false");
        // Streaming responses must not have content-length (regression guard)
        assert!(
            response.headers().get("content-length").is_none(),
            "Unlimited download should be streamed without content-length"
        );

        let body = response.text();
        let lines: Vec<&str> = body.trim().lines().collect();
        assert_eq!(lines.len(), num_requests, "Should return all {} results", num_requests);

        // Verify each line is valid JSON
        for line in &lines {
            let item: serde_json::Value = serde_json::from_str(line).expect("Each line should be valid JSON");
            assert!(item.get("custom_id").is_some(), "Each result should have custom_id");
        }

        // Test 2: Paginated download (buffered path)
        let page_size = 10;
        let response = app
            .get(&format!("/ai/v1/files/{}/content?limit={}", output_file_id, page_size))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;

        response.assert_status(axum::http::StatusCode::OK);
        response.assert_header("X-Incomplete", "true"); // more pages exist
        response.assert_header("X-Last-Line", &page_size.to_string());

        let body = response.text();
        let lines: Vec<&str> = body.trim().lines().collect();
        assert_eq!(lines.len(), page_size, "Should return exactly {} results", page_size);

        // Test 3: Last page should have X-Incomplete=false
        let response = app
            .get(&format!(
                "/ai/v1/files/{}/content?limit={}&skip={}",
                output_file_id,
                page_size,
                num_requests - page_size
            ))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;

        response.assert_status(axum::http::StatusCode::OK);
        response.assert_header("X-Incomplete", "false"); // no more pages, batch complete
        response.assert_header("X-Last-Line", &num_requests.to_string());
    }

    #[test]
    fn test_file_upload_error_into_http_error_stream_interrupted() {
        let err = super::FileUploadError::StreamInterrupted {
            message: "connection reset".to_string(),
        };
        let http_err = err.into_http_error();
        match http_err {
            crate::errors::Error::Internal { operation } => {
                assert!(operation.contains("connection reset"));
            }
            _ => panic!("Expected Internal error"),
        }
    }

    #[test]
    fn test_file_upload_error_into_http_error_file_too_large() {
        let err = super::FileUploadError::FileTooLarge { max: 100_000_000 };
        let http_err = err.into_http_error();
        match http_err {
            crate::errors::Error::PayloadTooLarge { message } => {
                assert!(message.contains("100000000"));
                // Should NOT contain the partial size
                assert!(!message.contains("200000000"));
            }
            _ => panic!("Expected PayloadTooLarge error"),
        }
    }

    #[test]
    fn test_file_upload_error_into_http_error_too_many_requests() {
        let err = super::FileUploadError::TooManyRequests { count: 1001, max: 1000 };
        let http_err = err.into_http_error();
        match http_err {
            crate::errors::Error::BadRequest { message } => {
                assert!(message.contains("1001"));
                assert!(message.contains("1000"));
            }
            _ => panic!("Expected BadRequest error"),
        }
    }

    #[test]
    fn test_file_upload_error_into_http_error_invalid_json() {
        let err = super::FileUploadError::InvalidJson {
            line: 42,
            error: "expected comma".to_string(),
        };
        let http_err = err.into_http_error();
        match http_err {
            crate::errors::Error::BadRequest { message } => {
                assert!(message.contains("line 42"));
                assert!(message.contains("expected comma"));
            }
            _ => panic!("Expected BadRequest error"),
        }
    }

    #[test]
    fn test_file_upload_error_into_http_error_invalid_utf8() {
        let err = super::FileUploadError::InvalidUtf8 {
            line: 5,
            byte_offset: 128,
            error: "invalid byte sequence".to_string(),
        };
        let http_err = err.into_http_error();
        match http_err {
            crate::errors::Error::BadRequest { message } => {
                assert!(message.contains("line 5"));
                assert!(message.contains("byte offset 128"));
            }
            _ => panic!("Expected BadRequest error"),
        }
    }

    #[test]
    fn test_file_upload_error_into_http_error_no_file() {
        let err = super::FileUploadError::NoFile;
        let http_err = err.into_http_error();
        match http_err {
            crate::errors::Error::BadRequest { message } => {
                assert!(message.contains("No file field"));
            }
            _ => panic!("Expected BadRequest error"),
        }
    }

    #[test]
    fn test_file_upload_error_into_http_error_empty_file() {
        let err = super::FileUploadError::EmptyFile;
        let http_err = err.into_http_error();
        match http_err {
            crate::errors::Error::BadRequest { message } => {
                assert!(message.contains("no valid request templates"));
            }
            _ => panic!("Expected BadRequest error"),
        }
    }

    #[test]
    fn test_file_upload_error_into_http_error_model_access_denied() {
        let error = super::FileUploadError::ModelAccessDenied {
            model: "gpt-5".to_string(),
            line: 42,
        };
        let http_error = error.into_http_error();
        match http_error {
            crate::errors::Error::ModelAccessDenied { model_name, message } => {
                assert_eq!(model_name, "gpt-5");
                assert!(message.contains("42"));
                assert!(message.contains("gpt-5"));
            }
            _ => panic!("Expected ModelAccessDenied error, got {:?}", http_error),
        }
    }

    #[test]
    fn test_file_upload_error_into_http_error_validation_error() {
        let err = super::FileUploadError::ValidationError {
            line: 3,
            message: "custom_id too long".to_string(),
        };
        let http_err = err.into_http_error();
        match http_err {
            crate::errors::Error::BadRequest { message } => {
                assert!(message.contains("Line 3"));
                assert!(message.contains("custom_id too long"));
            }
            _ => panic!("Expected BadRequest error"),
        }
    }

    /// Test that Content-Length header triggers early rejection for oversized files
    #[sqlx::test]
    #[test_log::test]
    async fn test_upload_content_length_early_rejection(pool: PgPool) {
        // Create app with a small file size limit
        let mut config = create_test_config();
        config.limits.files.max_file_size = 1000; // 1KB limit

        let (app, _bg_services) = create_test_app_with_config(pool.clone(), config, false).await;

        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        // The early rejection check allows max_file_size + 10KB overhead for multipart encoding.
        // With a 1KB limit, we need Content-Length > 1000 + 10240 = 11240 bytes.
        // Use 15KB of content to guarantee rejection via the early Content-Length check.
        let large_content = "x".repeat(15 * 1024);
        let file_part = axum_test::multipart::Part::bytes(large_content.into_bytes()).file_name("test.jsonl");

        let upload_response = app
            .post("/ai/v1/files")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_part("file", file_part)
                    .add_text("purpose", "batch"),
            )
            .await;

        // Should get 413 Payload Too Large
        upload_response.assert_status(axum::http::StatusCode::PAYLOAD_TOO_LARGE);
        let body = upload_response.text();
        assert!(body.contains("exceeds the maximum allowed size"));
    }

    /// Test that files exceeding size limit during streaming return 413
    #[sqlx::test]
    #[test_log::test]
    async fn test_upload_streaming_size_limit_returns_413(pool: PgPool) {
        // Use a limit large enough for multipart overhead but small enough
        // that our test content will exceed it during streaming
        let mut config = create_test_config();
        config.limits.files.max_file_size = 5000; // 5KB file limit

        let (app, _bg_services) = create_test_app_with_config(pool.clone(), config, false).await;

        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;
        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Create content that will exceed 5KB limit during streaming
        // Each line is ~150 bytes, so 50 lines is ~7.5KB which exceeds 5KB limit
        let mut lines = Vec::new();
        for i in 0..50 {
            lines.push(format!(
                r#"{{"custom_id":"req-{}","method":"POST","url":"/v1/chat/completions","body":{{"model":"gpt-4","messages":[{{"role":"user","content":"Hello world number {}"}}]}}}}"#,
                i, i
            ));
        }
        let large_content = lines.join("\n");
        let file_part = axum_test::multipart::Part::bytes(large_content.into_bytes()).file_name("test.jsonl");

        let upload_response = app
            .post("/ai/v1/files")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_part("file", file_part)
                    .add_text("purpose", "batch"),
            )
            .await;

        // Should get 413 Payload Too Large (not 500 or confusing error)
        upload_response.assert_status(axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_upload_invalid_utf8(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        // Create content with invalid UTF-8 bytes
        // 0xFF 0xFE is not valid UTF-8
        let mut content = b"{\"custom_id\":\"req-1\",\"method\":\"POST\",\"url\":\"/v1/chat/completions\",\"body\":{\"model\":\"gpt-4\",\"messages\":[{\"role\":\"user\",\"content\":\"Hello ".to_vec();
        content.extend_from_slice(&[0xFF, 0xFE]); // Invalid UTF-8
        content.extend_from_slice(b"\"}]}}");

        let file_part = axum_test::multipart::Part::bytes(content).file_name("test.jsonl");

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

        // Should reject with 400 Bad Request for invalid UTF-8
        upload_response.assert_status(axum::http::StatusCode::BAD_REQUEST);
        let body = upload_response.text();
        assert!(
            body.contains("UTF-8") || body.contains("utf-8") || body.contains("encoding"),
            "Expected error about UTF-8, got: {}",
            body
        );
    }

    #[test]
    fn test_multer_error_variants_exist() {
        // If multer removes/renames these, this won't compile
        let _stream_size = multer::Error::StreamSizeExceeded { limit: 0 };
        let _field_size = multer::Error::FieldSizeExceeded {
            limit: 0,
            field_name: None,
        };
        let _stream_read = multer::Error::StreamReadFailed(Box::new(std::io::Error::other("test")));
    }

    /// Compile-time test: Ensure axum's LengthLimitError exists.
    #[test]
    fn test_axum_length_limit_error_exists() {
        use super::LengthLimitError;
        // Verify it implements Error (required for downcast_ref)
        fn assert_error<T: std::error::Error + 'static>() {}
        assert_error::<LengthLimitError>();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_upload_rejects_request_exceeding_max_body_size(pool: PgPool) {
        // Create app with a small per-request body size limit
        let mut config = create_test_config();
        config.limits.requests.max_body_size = 100; // 100 bytes

        let (app, _bg_services) = create_test_app_with_config(pool.clone(), config, false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Create a request with a body that exceeds 100 bytes when serialized
        let large_content = "x".repeat(200);
        let jsonl_content = format!(
            r#"{{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{{"model":"gpt-4","messages":[{{"role":"user","content":"{}"}}]}}}}"#,
            large_content
        );

        let file_part = axum_test::multipart::Part::bytes(jsonl_content.into_bytes()).file_name("test.jsonl");

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
        let body = upload_response.text();
        assert!(
            body.contains("exceeds the maximum allowed size"),
            "Expected error about request body size, got: {}",
            body
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_upload_allows_request_within_max_body_size(pool: PgPool) {
        // Create app with a generous per-request body size limit
        let mut config = create_test_config();
        config.limits.requests.max_body_size = 10 * 1024; // 10KB

        let (app, _bg_services) = create_test_app_with_config(pool.clone(), config, false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Create a small request well within the limit
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;

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
    }
}
