//! Handler for retrieving Open Responses API responses.
//!
//! `GET /ai/v1/responses/{response_id}` reads directly from fusillade's
//! `requests` table, mapping the row to an Open Responses API Response object.
//!
//! Authentication is via Bearer API key (same as AI proxy requests).
//! Ownership is verified by checking the batch's `created_by` against the
//! API key's `user_id` (which is the org ID for org-scoped keys).

use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use fusillade::{ResponseStepStore, Storage};
use onwards::StoreError;
use sqlx_pool_router::PoolProvider;

use crate::AppState;
use crate::errors::{Error, Result};

/// Retrieve a response by ID.
///
/// Authenticates via Bearer API key. The response_id is the head step's
/// uuid (with optional `resp_` prefix); the head step's sub-request
/// fusillade row carries `created_by` for ownership.
#[tracing::instrument(skip_all)]
pub async fn get_response<P: PoolProvider>(
    State(state): State<AppState<P>>,
    headers: HeaderMap,
    Path(response_id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    // Require a valid API key and resolve the owner (user_id).
    // For org-scoped keys, user_id is the org ID.
    let api_key = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .ok_or_else(|| Error::Unauthenticated { message: None })?;

    let owner_id: String = sqlx::query_scalar("SELECT user_id::text FROM public.api_keys WHERE secret = $1 AND is_deleted = false LIMIT 1")
        .bind(api_key)
        .fetch_optional(state.db.read())
        .await
        .map_err(|e| Error::Database(e.into()))?
        .ok_or_else(|| Error::Unauthenticated { message: None })?;

    // Parse the response ID to a head_step UUID. After the
    // response_steps re-anchoring (fusillade 16.8) `resp_<id>` is the
    // head step's id, NOT a fusillade.requests id.
    let uuid_str = response_id.strip_prefix("resp_").unwrap_or(&response_id);
    let head_step_uuid = uuid::Uuid::parse_str(uuid_str).map_err(|_| Error::NotFound {
        resource: "response".to_string(),
        id: response_id.clone(),
    })?;

    // Resolve the row that carries `created_by` for ownership.
    // Two paths, mirroring `FusilladeResponseStore::get_response`:
    //   * Multi-step — head step → its sub-request fusillade row.
    //   * Single-step — the id is itself a fusillade.requests row
    //     (chat completions / embeddings retrieved via the same GET).
    // The auth resolution happens here on the row that's actually
    // backing this response; `response_store.get_response` then
    // assembles the API envelope.
    let auth_request_id = match state.response_step_manager.as_ref() {
        Some(step_manager) => match step_manager.get_step(fusillade::StepId(head_step_uuid)).await {
            Ok(Some(head_step)) => head_step.request_id.unwrap_or(fusillade::RequestId(head_step_uuid)),
            Ok(None) => fusillade::RequestId(head_step_uuid),
            Err(e) => return Err(Error::Database(crate::db::errors::DbError::Other(anyhow::anyhow!("{e}")))),
        },
        None => fusillade::RequestId(head_step_uuid),
    };

    let detail = state
        .request_manager
        .get_request_detail(auth_request_id)
        .await
        .map_err(|e| match e {
            fusillade::FusilladeError::RequestNotFound(_) => Error::NotFound {
                resource: "response".to_string(),
                id: response_id.clone(),
            },
            _ => Error::Database(crate::db::errors::DbError::Other(anyhow::anyhow!("{e}"))),
        })?;

    // Verify ownership: the row's created_by must match the API key's user_id.
    // Return 404 (not 403) to avoid leaking existence of other users' responses.
    // `detail.created_by` is non-empty by fusillade's schema invariants —
    // `get_request_detail` filters out batched rows, and `create_realtime` /
    // `create_flex` coerce empty inputs to NULL which the XOR CHECK rejects.
    let owner = detail.created_by.as_str();
    if owner != owner_id {
        return Err(Error::NotFound {
            resource: "response".to_string(),
            id: response_id,
        });
    }

    // Convert the detail to an Open Responses API response object.
    let resp = state
        .response_store
        .get_response(&response_id)
        .await
        .map_err(|e| match e {
            StoreError::NotFound(_) => Error::NotFound {
                resource: "response".to_string(),
                id: response_id.clone(),
            },
            _ => Error::Database(crate::db::errors::DbError::Other(anyhow::anyhow!("{e}"))),
        })?
        .ok_or_else(|| Error::NotFound {
            resource: "response".to_string(),
            id: response_id,
        })?;

    Ok(Json(resp))
}
