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
use fusillade::Storage;
use onwards::StoreError;
use sqlx_pool_router::PoolProvider;

use crate::AppState;
use crate::errors::{Error, Result};

/// Retrieve a response by ID.
///
/// Authenticates via Bearer API key. Returns the Open Responses API Response
/// object only if the response's batch is owned by the API key's user.
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

    // Parse the response ID to a UUID for fusillade lookup.
    let uuid_str = response_id.strip_prefix("resp_").unwrap_or(&response_id);
    let request_id = uuid::Uuid::parse_str(uuid_str).map_err(|_| Error::NotFound {
        resource: "response".to_string(),
        id: response_id.clone(),
    })?;

    // Fetch the request detail from fusillade (includes batch_created_by).
    let detail = state
        .request_manager
        .get_request_detail(fusillade::RequestId(request_id))
        .await
        .map_err(|e| match e {
            fusillade::FusilladeError::RequestNotFound(_) => Error::NotFound {
                resource: "response".to_string(),
                id: response_id.clone(),
            },
            _ => Error::Database(crate::db::errors::DbError::Other(anyhow::anyhow!("{e}"))),
        })?;

    // Verify ownership: the batch's created_by must match the API key's user_id.
    // Return 404 (not 403) to avoid leaking existence of other users' responses.
    let batch_owner = detail.batch_created_by.as_deref().unwrap_or("");
    if batch_owner != owner_id {
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
