//! Fusillade-backed implementation of onwards' `ResponseStore` trait (read-only)
//! and standalone functions for creating/completing response records.
//!
//! The `ResponseStore` trait implementation handles read operations only:
//! - `get()` for `GET /v1/responses/{id}`
//! - `get_context()` for `previous_response_id` resolution
//! - `store()` for the adapter's post-completion persistence
//!
//! Write operations (creating pending records, completing/failing them) are handled
//! by the responses middleware and `FusilladeOutletHandler` respectively.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use onwards::{ResponseStore, StoreError};
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// Header set by the responses middleware so the outlet handler knows which
/// fusillade row to update with the response body.
pub const ONWARDS_RESPONSE_ID_HEADER: &str = "x-onwards-response-id";

/// A fusillade daemon ID assigned to this onwards instance.
#[derive(Debug, Clone, Copy)]
pub struct OnwardsDaemonId(pub Uuid);

/// ResponseStore implementation backed by fusillade's requests table.
///
/// Read-only — lifecycle management (create/complete/fail) is handled by
/// the responses middleware and FusilladeOutletHandler.
pub struct FusilladeResponseStore {
    pool: PgPool,
}

impl FusilladeResponseStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Retrieve a response by ID. Used by the GET /v1/responses/{id} handler.
    pub async fn get_response(&self, response_id: &str) -> Result<Option<serde_json::Value>, StoreError> {
        let id = parse_response_id(response_id)?;

        let row = sqlx::query(
            "SELECT r.id, r.state, r.model, t.body, r.response_body, r.response_status,
                    r.error, r.created_at, r.completed_at, r.failed_at, r.batch_id
             FROM requests r
             LEFT JOIN request_templates t ON r.template_id = t.id
             WHERE r.id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| StoreError::StorageError(format!("Failed to fetch request: {e}")))?;

        Ok(row.as_ref().map(row_to_response_object))
    }
}

/// Create a pending response in fusillade with a pre-generated response ID.
///
/// Used by the background path where the ID is generated before the task is spawned.
pub async fn create_pending_with_id(
    pool: &PgPool,
    response_id: &str,
    request: &serde_json::Value,
    model: &str,
    endpoint: &str,
    daemon_id: OnwardsDaemonId,
) -> Result<(), StoreError> {
    let id = parse_response_id(response_id)?;
    create_pending_inner(pool, id, request, model, endpoint, daemon_id).await?;
    Ok(())
}

/// Create a pending response in fusillade (template + request rows).
///
/// Called by the responses middleware before onwards proxies the request.
/// Returns the response ID (e.g., `resp_<uuid>`).
pub async fn create_pending(
    pool: &PgPool,
    request: &serde_json::Value,
    model: &str,
    endpoint: &str,
    daemon_id: OnwardsDaemonId,
) -> Result<String, StoreError> {
    let id = Uuid::new_v4();
    create_pending_inner(pool, id, request, model, endpoint, daemon_id).await?;
    Ok(format!("resp_{id}"))
}

async fn create_pending_inner(
    pool: &PgPool,
    id: Uuid,
    request: &serde_json::Value,
    model: &str,
    endpoint: &str,
    daemon_id: OnwardsDaemonId,
) -> Result<(), StoreError> {
    let id = id;
    let now = Utc::now();
    let body = request.to_string();

    // Insert template row (stores the original request body, file_id = NULL)
    sqlx::query(
        "INSERT INTO request_templates (id, file_id, custom_id, endpoint, method, path, body, model, api_key)
         VALUES ($1, NULL, NULL, $2, 'POST', $2, $3, $4, '')",
    )
    .bind(id)
    .bind(endpoint)
    .bind(&body)
    .bind(model)
    .execute(pool)
    .await
    .map_err(|e| StoreError::StorageError(format!("Failed to insert template: {e}")))?;

    // Insert request row in processing state (onwards is the daemon).
    // Only model and custom_id are denormalized on requests; other fields
    // (endpoint, method, path, body, api_key) live on request_templates.
    sqlx::query(
        "INSERT INTO requests (id, batch_id, template_id, model, custom_id, state, daemon_id, claimed_at, started_at)
         VALUES ($1, NULL, $2, $3, NULL, 'processing', $4, $5, $6)",
    )
    .bind(id)
    .bind(id) // template_id = same as request id
    .bind(model)
    .bind(daemon_id.0)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await
    .map_err(|e| StoreError::StorageError(format!("Failed to insert request: {e}")))?;

    Ok(())
}

/// Mark a response as completed with the response body.
///
/// Called by the `FusilladeOutletHandler` after outlet captures the response.
pub async fn complete_response(pool: &PgPool, response_id: &str, response_body: &str, status_code: u16) -> Result<(), StoreError> {
    let id = parse_response_id(response_id)?;
    let size = response_body.len() as i64;

    sqlx::query(
        "UPDATE requests
         SET state = 'completed',
             response_status = $2,
             response_body = $3,
             response_size = $4,
             completed_at = NOW()
         WHERE id = $1 AND state = 'processing'",
    )
    .bind(id)
    .bind(status_code as i16)
    .bind(response_body)
    .bind(size)
    .execute(pool)
    .await
    .map_err(|e| StoreError::StorageError(format!("Failed to complete request: {e}")))?;

    Ok(())
}

/// Mark a response as failed.
///
/// Called by the `FusilladeOutletHandler` when the upstream returns an error.
pub async fn fail_response(pool: &PgPool, response_id: &str, error: &str) -> Result<(), StoreError> {
    let id = parse_response_id(response_id)?;

    let error_json = serde_json::json!({
        "type": "NonRetriableHttpStatus",
        "status": 500,
        "message": error,
    })
    .to_string();

    sqlx::query(
        "UPDATE requests
         SET state = 'failed',
             error = $2,
             failed_at = NOW()
         WHERE id = $1 AND state = 'processing'",
    )
    .bind(id)
    .bind(&error_json)
    .execute(pool)
    .await
    .map_err(|e| StoreError::StorageError(format!("Failed to mark request failed: {e}")))?;

    Ok(())
}

/// Poll a fusillade request until it reaches a terminal state (completed/failed/canceled).
///
/// Returns the full Response object once terminal, or an error if the timeout is reached.
pub async fn poll_until_complete(
    pool: &PgPool,
    response_id: &str,
    poll_interval: std::time::Duration,
    timeout: std::time::Duration,
) -> Result<serde_json::Value, StoreError> {
    let id = parse_response_id(response_id)?;
    let start = std::time::Instant::now();

    loop {
        let row = sqlx::query(
            "SELECT r.id, r.state, r.model, t.body, r.response_body, r.response_status,
                    r.error, r.created_at, r.completed_at, r.failed_at, r.batch_id
             FROM requests r
             LEFT JOIN request_templates t ON r.template_id = t.id
             WHERE r.id = $1",
        )
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(|e| StoreError::StorageError(format!("Failed to poll request: {e}")))?;

        if let Some(ref row) = row {
            let state: &str = row.get("state");
            match state {
                "completed" | "failed" | "canceled" => {
                    return Ok(row_to_response_object(row));
                }
                _ => {}
            }
        }

        if start.elapsed() >= timeout {
            return Err(StoreError::StorageError(format!(
                "Timeout waiting for request {response_id} to complete after {:?}",
                timeout
            )));
        }

        tokio::time::sleep(poll_interval).await;
    }
}

/// Create a batch of 1 in fusillade for async/flex processing.
///
/// Creates a file, template, batch, and request row. The fusillade daemon
/// will pick up the pending request and process it.
///
/// Returns `(response_id, request_id)` where response_id is `resp_<uuid>`.
pub async fn create_batch_of_1(
    pool: &PgPool,
    request: &serde_json::Value,
    model: &str,
    base_url: &str,
    path: &str,
    completion_window: &str,
    api_key: Option<&str>,
) -> Result<(String, Uuid), StoreError> {
    let file_id = Uuid::new_v4();
    let template_id = Uuid::new_v4();
    let batch_id = Uuid::new_v4();
    let request_id = Uuid::new_v4();
    let output_file_id = Uuid::new_v4();
    let error_file_id = Uuid::new_v4();
    let now = Utc::now();
    let body = request.to_string();

    // Look up user from API key for batch attribution.
    // api_keys lives in the public schema (dwctl), not the fusillade schema.
    let created_by = if let Some(key) = api_key {
        let row = sqlx::query("SELECT user_id FROM public.api_keys WHERE secret = $1 AND is_deleted = false LIMIT 1")
            .bind(key)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();

        match row {
            Some(row) => {
                let user_id: Uuid = row.get("user_id");
                user_id.to_string()
            }
            None => String::new(),
        }
    } else {
        String::new()
    };

    // Parse completion window to compute expires_at
    let std_duration =
        humantime::parse_duration(completion_window).map_err(|e| StoreError::StorageError(format!("Invalid completion_window: {e}")))?;
    let chrono_duration =
        chrono::Duration::from_std(std_duration).map_err(|e| StoreError::StorageError(format!("Duration conversion error: {e}")))?;
    let expires_at = now + chrono_duration;

    // Create file record (purpose = "batch")
    sqlx::query(
        "INSERT INTO files (id, name, status, uploaded_by, purpose, created_at, updated_at)
         VALUES ($1, 'responses_api_single', 'processed', $2, 'batch', $3, $3)",
    )
    .bind(file_id)
    .bind(&created_by)
    .bind(now)
    .execute(pool)
    .await
    .map_err(|e| StoreError::StorageError(format!("Failed to create file: {e}")))?;

    // Create virtual output/error files
    for (fid, purpose) in [(output_file_id, "batch_output"), (error_file_id, "batch_error")] {
        sqlx::query(
            "INSERT INTO files (id, name, status, uploaded_by, purpose, created_at, updated_at)
             VALUES ($1, $2, 'processed', '', $2, $3, $3)",
        )
        .bind(fid)
        .bind(purpose)
        .bind(now)
        .execute(pool)
        .await
        .map_err(|e| StoreError::StorageError(format!("Failed to create {purpose} file: {e}")))?;
    }

    // Create template
    sqlx::query(
        "INSERT INTO request_templates (id, file_id, custom_id, endpoint, method, path, body, model, api_key)
         VALUES ($1, $2, NULL, $3, 'POST', $4, $5, $6, '')",
    )
    .bind(template_id)
    .bind(file_id)
    .bind(base_url)
    .bind(path)
    .bind(&body)
    .bind(model)
    .execute(pool)
    .await
    .map_err(|e| StoreError::StorageError(format!("Failed to create template: {e}")))?;

    // Create batch with user attribution
    sqlx::query(
        "INSERT INTO batches (id, file_id, endpoint, completion_window, created_by, expires_at, output_file_id, error_file_id, total_requests, created_at, api_key)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 1, $9, $10)",
    )
    .bind(batch_id)
    .bind(file_id)
    .bind(path)
    .bind(completion_window)
    .bind(&created_by)
    .bind(expires_at)
    .bind(output_file_id)
    .bind(error_file_id)
    .bind(now)
    .bind(api_key.unwrap_or(""))
    .execute(pool)
    .await
    .map_err(|e| StoreError::StorageError(format!("Failed to create batch: {e}")))?;

    // Create request in pending state (daemon will claim it)
    sqlx::query(
        "INSERT INTO requests (id, batch_id, template_id, model, custom_id, state, service_tier)
         VALUES ($1, $2, $3, $4, NULL, 'pending', 'flex')",
    )
    .bind(request_id)
    .bind(batch_id)
    .bind(template_id)
    .bind(model)
    .execute(pool)
    .await
    .map_err(|e| StoreError::StorageError(format!("Failed to create request: {e}")))?;

    // Mark batch as started so daemon sees it as active.
    // The pending_requests counter is maintained by triggers when requests are inserted.
    sqlx::query("UPDATE batches SET requests_started_at = $2 WHERE id = $1")
        .bind(batch_id)
        .bind(now)
        .execute(pool)
        .await
        .map_err(|e| StoreError::StorageError(format!("Failed to activate batch: {e}")))?;

    let response_id = format!("resp_{request_id}");
    tracing::debug!(
        response_id = %response_id,
        batch_id = %batch_id,
        completion_window = %completion_window,
        "Created batch of 1 for async processing"
    );

    Ok((response_id, request_id))
}

/// Parse a response ID like "resp_<uuid>" into a UUID.
fn parse_response_id(response_id: &str) -> Result<Uuid, StoreError> {
    let uuid_str = response_id.strip_prefix("resp_").unwrap_or(response_id);
    Uuid::parse_str(uuid_str).map_err(|e| StoreError::NotFound(format!("Invalid response ID: {e}")))
}

/// Map a fusillade request state to an Open Responses API status.
fn state_to_status(state: &str) -> &'static str {
    match state {
        "pending" => "queued",
        "claimed" | "processing" => "in_progress",
        "completed" => "completed",
        "failed" => "failed",
        "canceled" => "cancelled",
        _ => "failed",
    }
}

/// Convert a database row into an Open Responses API Response object.
fn row_to_response_object(row: &sqlx::postgres::PgRow) -> serde_json::Value {
    let id: Uuid = row.get("id");
    let state: &str = row.get("state");
    let model: &str = row.get("model");
    let created_at: DateTime<Utc> = row.get("created_at");
    let batch_id: Option<Uuid> = row.get("batch_id");

    let status = state_to_status(state);
    let background = batch_id.is_some();

    let mut resp = serde_json::json!({
        "id": format!("resp_{id}"),
        "object": "response",
        "created_at": created_at.timestamp(),
        "status": status,
        "model": model,
        "background": background,
        "output": [],
    });

    if status == "completed" {
        let response_body: Option<String> = row.get("response_body");
        if let Some(ref body) = response_body
            && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(body)
        {
            if let Some(output) = parsed.get("output") {
                resp["output"] = output.clone();
            }
            if let Some(usage) = parsed.get("usage") {
                resp["usage"] = usage.clone();
            }
            // ChatCompletion format (batch results)
            if parsed.get("choices").is_some() {
                resp["output"] = serde_json::json!([{
                    "type": "message",
                    "role": "assistant",
                    "content": parsed
                }]);
            }
        }
        let completed_at: Option<DateTime<Utc>> = row.get("completed_at");
        resp["completed_at"] = serde_json::json!(completed_at.map(|t| t.timestamp()));
    }

    if status == "failed" {
        let error: Option<String> = row.get("error");
        if let Some(ref err) = error {
            resp["error"] = serde_json::json!({
                "type": "server_error",
                "message": err,
            });
        }
    }

    resp
}

#[async_trait]
impl ResponseStore for FusilladeResponseStore {
    async fn store(&self, response: &serde_json::Value) -> Result<String, StoreError> {
        // The adapter calls this after constructing the final response.
        // The row already exists (created by middleware), and the outlet handler
        // will write the response body. Just return the existing ID.
        let id = response.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        Ok(id)
    }

    async fn get_context(&self, response_id: &str) -> Result<Option<serde_json::Value>, StoreError> {
        self.get_response(response_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_response_id_with_prefix() {
        let uuid = Uuid::new_v4();
        let id = format!("resp_{uuid}");
        let parsed = parse_response_id(&id).unwrap();
        assert_eq!(parsed, uuid);
    }

    #[test]
    fn test_parse_response_id_without_prefix() {
        let uuid = Uuid::new_v4();
        let parsed = parse_response_id(&uuid.to_string()).unwrap();
        assert_eq!(parsed, uuid);
    }

    #[test]
    fn test_parse_response_id_invalid() {
        let result = parse_response_id("not-a-uuid");
        assert!(result.is_err());
        assert!(matches!(result, Err(StoreError::NotFound(_))));
    }

    #[test]
    fn test_state_to_status_mapping() {
        assert_eq!(state_to_status("pending"), "queued");
        assert_eq!(state_to_status("claimed"), "in_progress");
        assert_eq!(state_to_status("processing"), "in_progress");
        assert_eq!(state_to_status("completed"), "completed");
        assert_eq!(state_to_status("failed"), "failed");
        assert_eq!(state_to_status("canceled"), "cancelled");
        assert_eq!(state_to_status("unknown"), "failed");
    }

    #[test]
    fn test_store_extracts_id_from_response() {
        let response = serde_json::json!({
            "id": "resp_12345678-1234-1234-1234-123456789abc",
            "status": "completed",
        });
        let id = response.get("id").and_then(|v| v.as_str()).unwrap_or("");
        assert_eq!(id, "resp_12345678-1234-1234-1234-123456789abc");
    }

    #[test]
    fn test_store_handles_missing_id() {
        let response = serde_json::json!({"status": "completed"});
        let id = response.get("id").and_then(|v| v.as_str()).unwrap_or("");
        assert_eq!(id, "");
    }
}
