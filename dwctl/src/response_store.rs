//! Fusillade-backed implementation of onwards' `ResponseStore` trait.
//!
//! Writes every Open Responses API request into fusillade's `request_templates`
//! and `requests` tables so that `GET /v1/responses/{id}` can retrieve them.
//! Onwards registers as a fusillade daemon, so realtime requests get a real
//! `daemon_id` and satisfy the existing CHECK constraints.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use onwards::{ResponseStore, StoreError};
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// A fusillade daemon ID assigned to this onwards instance.
#[derive(Debug, Clone, Copy)]
pub struct OnwardsDaemonId(pub Uuid);

/// ResponseStore implementation backed by fusillade's requests table.
pub struct FusilladeResponseStore {
    pool: PgPool,
    daemon_id: OnwardsDaemonId,
}

impl FusilladeResponseStore {
    pub fn new(pool: PgPool, daemon_id: OnwardsDaemonId) -> Self {
        Self { pool, daemon_id }
    }

    /// Retrieve a response by ID. Used by the GET /v1/responses/{id} handler.
    pub async fn get_response(
        &self,
        response_id: &str,
    ) -> Result<Option<serde_json::Value>, StoreError> {
        self.get(response_id).await
    }
}

/// Parse a response ID like "resp_<uuid>" into a UUID.
fn parse_response_id(response_id: &str) -> Result<Uuid, StoreError> {
    let uuid_str = response_id.strip_prefix("resp_").unwrap_or(response_id);
    Uuid::parse_str(uuid_str)
        .map_err(|e| StoreError::NotFound(format!("Invalid response ID: {e}")))
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
        if let Some(ref body) = response_body {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(body) {
                if let Some(output) = parsed.get("output") {
                    resp["output"] = output.clone();
                }
                if let Some(usage) = parsed.get("usage") {
                    resp["usage"] = usage.clone();
                }
                if parsed.get("choices").is_some() {
                    resp["output"] = serde_json::json!([{
                        "type": "message",
                        "role": "assistant",
                        "content": parsed
                    }]);
                }
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
    async fn create_pending(
        &self,
        request: &serde_json::Value,
        model: &str,
        endpoint: &str,
    ) -> Result<String, StoreError> {
        let id = Uuid::new_v4();
        let now = Utc::now();
        let body = request.to_string();
        let daemon_id = self.daemon_id.0;

        // Insert template row (stores the original request body, file_id = NULL)
        sqlx::query(
            "INSERT INTO request_templates (id, file_id, custom_id, endpoint, method, path, body, model, api_key)
             VALUES ($1, NULL, NULL, $2, 'POST', $2, $3, $4, '')",
        )
        .bind(id)
        .bind(endpoint)
        .bind(&body)
        .bind(model)
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::StorageError(format!("Failed to insert template: {e}")))?;

        // Insert request row in processing state (onwards is the daemon)
        sqlx::query(
            "INSERT INTO requests (id, batch_id, template_id, endpoint, method, path, body, model, api_key, state, daemon_id, claimed_at, started_at)
             VALUES ($1, NULL, $1, $2, 'POST', $2, $3, $4, '', 'processing', $5, $6, $6)",
        )
        .bind(id)
        .bind(endpoint)
        .bind(&body)
        .bind(model)
        .bind(daemon_id)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::StorageError(format!("Failed to insert request: {e}")))?;

        Ok(format!("resp_{id}"))
    }

    async fn complete(
        &self,
        response_id: &str,
        response: &serde_json::Value,
        status_code: u16,
    ) -> Result<(), StoreError> {
        let id = parse_response_id(response_id)?;
        let body = response.to_string();
        let size = body.len() as i64;

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
        .bind(&body)
        .bind(size)
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::StorageError(format!("Failed to complete request: {e}")))?;

        Ok(())
    }

    async fn fail(&self, response_id: &str, error: &str) -> Result<(), StoreError> {
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
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::StorageError(format!("Failed to mark request failed: {e}")))?;

        Ok(())
    }

    async fn get(&self, response_id: &str) -> Result<Option<serde_json::Value>, StoreError> {
        let id = parse_response_id(response_id)?;

        let row = sqlx::query(
            "SELECT id, state, model, body, response_body, response_status,
                    error, created_at, completed_at, failed_at, batch_id
             FROM requests
             WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| StoreError::StorageError(format!("Failed to fetch request: {e}")))?;

        Ok(row.as_ref().map(row_to_response_object))
    }

    async fn store(&self, response: &serde_json::Value) -> Result<String, StoreError> {
        let id = response
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        if !id.is_empty() {
            self.complete(&id, response, 200).await?;
        }

        Ok(id)
    }

    async fn get_context(
        &self,
        response_id: &str,
    ) -> Result<Option<serde_json::Value>, StoreError> {
        self.get(response_id).await
    }
}
