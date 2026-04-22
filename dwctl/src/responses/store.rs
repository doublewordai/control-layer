//! Fusillade-backed implementation of onwards' `ResponseStore` trait (read-only)
//! and standalone functions for creating/completing response records.
//!
//! The `ResponseStore` trait implementation handles read operations only:
//! - `get()` for `GET /v1/responses/{id}`
//! - `get_context()` for `previous_response_id` resolution
//! - `store()` for the adapter's post-completion persistence
//!
//! Write operations use fusillade's `Storage` trait methods where possible.
//! The complete/fail operations use raw SQL since fusillade's `persist()` requires
//! a full `Request<T>` state machine object.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use fusillade::{
    BatchInput, PostgresRequestManager, RequestDetail, RequestId, RequestTemplateInput,
    ReqwestHttpClient, Storage,
};
use onwards::{ResponseStore, StoreError};
use sqlx::PgPool;
use sqlx_pool_router::PoolProvider;
use uuid::Uuid;

/// Header set by the responses middleware so the outlet handler knows which
/// fusillade row to update with the response body.
pub const ONWARDS_RESPONSE_ID_HEADER: &str = "x-onwards-response-id";

/// A fusillade daemon ID assigned to this onwards instance.
#[derive(Debug, Clone, Copy)]
pub struct OnwardsDaemonId(pub Uuid);

/// ResponseStore implementation backed by fusillade's requests table.
///
/// Uses fusillade's `Storage` trait for reads (via `get_request_detail`)
/// and raw SQL only for complete/fail state transitions.
pub struct FusilladeResponseStore<P: PoolProvider + Clone> {
    request_manager: Arc<PostgresRequestManager<P, ReqwestHttpClient>>,
}

impl<P: PoolProvider + Clone> FusilladeResponseStore<P> {
    pub fn new(request_manager: Arc<PostgresRequestManager<P, ReqwestHttpClient>>) -> Self {
        Self { request_manager }
    }

    /// Retrieve a response by ID. Used by the GET /v1/responses/{id} handler.
    pub async fn get_response(&self, response_id: &str) -> Result<Option<serde_json::Value>, StoreError> {
        let id = parse_response_id(response_id)?;

        match self.request_manager.get_request_detail(RequestId(id)).await {
            Ok(detail) => Ok(Some(detail_to_response_object(&detail))),
            Err(fusillade::FusilladeError::RequestNotFound(_)) => Ok(None),
            Err(e) => Err(StoreError::StorageError(format!("Failed to fetch request: {e}"))),
        }
    }
}

/// Create a pending response in fusillade with a pre-generated response ID.
///
/// Creates a file + batch wrapping a single request template. The request starts
/// in "pending" state and is immediately transitioned to "processing" since
/// onwards is about to proxy it.
pub async fn create_pending_with_id<P: PoolProvider + Clone>(
    request_manager: &PostgresRequestManager<P, ReqwestHttpClient>,
    response_id: &str,
    request: &serde_json::Value,
    model: &str,
    endpoint: &str,
    daemon_id: OnwardsDaemonId,
) -> Result<(), StoreError> {
    let id = parse_response_id(response_id)?;
    create_pending_inner(request_manager, id, request, model, endpoint, daemon_id).await?;
    Ok(())
}

/// Create a pending response in fusillade (file + batch + request).
///
/// Called by the responses middleware before onwards proxies the request.
/// Returns the response ID (e.g., `resp_<uuid>`).
pub async fn create_pending<P: PoolProvider + Clone>(
    request_manager: &PostgresRequestManager<P, ReqwestHttpClient>,
    request: &serde_json::Value,
    model: &str,
    endpoint: &str,
    daemon_id: OnwardsDaemonId,
) -> Result<String, StoreError> {
    let id = Uuid::new_v4();
    create_pending_inner(request_manager, id, request, model, endpoint, daemon_id).await?;
    Ok(format!("resp_{id}"))
}

async fn create_pending_inner<P: PoolProvider + Clone>(
    request_manager: &PostgresRequestManager<P, ReqwestHttpClient>,
    id: Uuid,
    request: &serde_json::Value,
    model: &str,
    endpoint: &str,
    daemon_id: OnwardsDaemonId,
) -> Result<(), StoreError> {
    let body = request.to_string();
    let now = Utc::now();

    let template = RequestTemplateInput {
        custom_id: None,
        endpoint: endpoint.to_string(),
        method: "POST".to_string(),
        path: endpoint.to_string(),
        body,
        model: model.to_string(),
        api_key: String::new(),
    };

    let file_id = request_manager
        .create_file("responses_api_realtime".into(), None, vec![template])
        .await
        .map_err(|e| StoreError::StorageError(format!("Failed to create file: {e}")))?;

    let batch = request_manager
        .create_batch(BatchInput {
            file_id,
            endpoint: endpoint.to_string(),
            completion_window: "5m".to_string(),
            metadata: None,
            created_by: None,
            api_key_id: None,
            api_key: None,
            total_requests: Some(1),
        })
        .await
        .map_err(|e| StoreError::StorageError(format!("Failed to create batch: {e}")))?;

    // Get the request created by create_batch so we can find its ID
    let requests = request_manager
        .get_batch_requests(batch.id)
        .await
        .map_err(|e| StoreError::StorageError(format!("Failed to get batch requests: {e}")))?;

    let request_id = requests
        .first()
        .map(|r| r.id())
        .ok_or_else(|| StoreError::StorageError("Batch created with no requests".into()))?;

    let pool = request_manager.pool();

    // Update the request ID to match the pre-generated response ID,
    // and transition from pending to processing since onwards will proxy it.
    sqlx::query(
        "UPDATE requests SET id = $2, state = 'processing', daemon_id = $3, claimed_at = $4, started_at = $4
         WHERE id = $1",
    )
    .bind(*request_id)
    .bind(id)
    .bind(daemon_id.0)
    .bind(now)
    .execute(pool)
    .await
    .map_err(|e| StoreError::StorageError(format!("Failed to transition request to processing: {e}")))?;

    Ok(())
}

/// Mark a response as completed with the response body.
///
/// Called by the `FusilladeOutletHandler` after outlet captures the response.
/// Uses raw SQL since fusillade's `persist()` requires a full `Request<Completed>`.
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
/// Uses raw SQL since fusillade's `persist()` requires a full `Request<Failed>`.
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
pub async fn poll_until_complete<P: PoolProvider + Clone>(
    request_manager: &PostgresRequestManager<P, ReqwestHttpClient>,
    response_id: &str,
    poll_interval: std::time::Duration,
    timeout: std::time::Duration,
) -> Result<serde_json::Value, StoreError> {
    let id = parse_response_id(response_id)?;
    let start = std::time::Instant::now();

    loop {
        match request_manager.get_request_detail(RequestId(id)).await {
            Ok(detail) => {
                match detail.status.as_str() {
                    "completed" | "failed" | "canceled" => {
                        return Ok(detail_to_response_object(&detail));
                    }
                    _ => {}
                }
            }
            Err(fusillade::FusilladeError::RequestNotFound(_)) => {}
            Err(e) => {
                return Err(StoreError::StorageError(format!("Failed to poll request: {e}")));
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
/// Uses fusillade's `create_file` + `create_batch` instead of raw SQL.
/// The fusillade daemon will pick up the pending request and process it.
///
/// Returns `(response_id, request_id)` where response_id is `resp_<uuid>`.
pub async fn create_batch_of_1<P: PoolProvider + Clone>(
    request_manager: &PostgresRequestManager<P, ReqwestHttpClient>,
    request: &serde_json::Value,
    model: &str,
    base_url: &str,
    path: &str,
    completion_window: &str,
    api_key: Option<&str>,
) -> Result<(String, Uuid), StoreError> {
    let pool = request_manager.pool();
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
                use sqlx::Row;
                let user_id: Uuid = row.get("user_id");
                user_id.to_string()
            }
            None => String::new(),
        }
    } else {
        String::new()
    };

    let template = RequestTemplateInput {
        custom_id: None,
        endpoint: base_url.to_string(),
        method: "POST".to_string(),
        path: path.to_string(),
        body,
        model: model.to_string(),
        api_key: String::new(),
    };

    let file_id = request_manager
        .create_file("responses_api_single".into(), None, vec![template])
        .await
        .map_err(|e| StoreError::StorageError(format!("Failed to create file: {e}")))?;

    let batch = request_manager
        .create_batch(BatchInput {
            file_id,
            endpoint: path.to_string(),
            completion_window: completion_window.to_string(),
            metadata: None,
            created_by: if created_by.is_empty() { None } else { Some(created_by) },
            api_key_id: None,
            api_key: api_key.map(|s| s.to_string()),
            total_requests: Some(1),
        })
        .await
        .map_err(|e| StoreError::StorageError(format!("Failed to create batch: {e}")))?;

    let requests = request_manager
        .get_batch_requests(batch.id)
        .await
        .map_err(|e| StoreError::StorageError(format!("Failed to get batch requests: {e}")))?;

    let request_id = requests
        .first()
        .map(|r| *r.id())
        .ok_or_else(|| StoreError::StorageError("Batch created with no requests".into()))?;

    let response_id = format!("resp_{request_id}");
    tracing::debug!(
        response_id = %response_id,
        batch_id = %batch.id,
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

/// Convert a `RequestDetail` into an Open Responses API Response object.
fn detail_to_response_object(detail: &RequestDetail) -> serde_json::Value {
    let status = state_to_status(&detail.status);

    let mut resp = serde_json::json!({
        "id": format!("resp_{}", detail.id),
        "object": "response",
        "created_at": detail.created_at.timestamp(),
        "status": status,
        "model": detail.model,
        "background": true,
        "output": [],
    });

    if status == "completed" {
        if let Some(ref body) = detail.response_body
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
        resp["completed_at"] = serde_json::json!(detail.completed_at.map(|t| t.timestamp()));
    }

    if status == "failed" {
        if let Some(ref err) = detail.error {
            resp["error"] = serde_json::json!({
                "type": "server_error",
                "message": err,
            });
        }
    }

    resp
}

#[async_trait]
impl<P: PoolProvider + Clone + Send + Sync + 'static> ResponseStore for FusilladeResponseStore<P> {
    async fn store(&self, response: &serde_json::Value) -> Result<String, StoreError> {
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
