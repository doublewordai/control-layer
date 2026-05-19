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
    http::{HeaderMap, StatusCode},
};
use fusillade::{ResponseStepStore, Storage};
use onwards::StoreError;
use sqlx_pool_router::PoolProvider;
use std::collections::HashSet;

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

/// Delete a response by ID.
///
/// Right-to-erasure: hard-deletes every `fusillade.requests` row that backs
/// the response. For multi-step responses, walks the `response_steps` chain
/// from the head step and deletes each step's sub-request — the `response_steps`
/// rows themselves cascade via FK. For single-step responses (chat completions
/// / embeddings retrieved via the same GET surface), deletes the one row.
///
/// `http_analytics` has no FK to requests and is preserved so billing / usage
/// records survive.
#[tracing::instrument(skip_all, fields(response_id = %response_id))]
pub async fn delete_response<P: PoolProvider>(
    State(state): State<AppState<P>>,
    headers: HeaderMap,
    Path(response_id): Path<String>,
) -> Result<StatusCode> {
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

    let uuid_str = response_id.strip_prefix("resp_").unwrap_or(&response_id);
    let head_step_uuid = uuid::Uuid::parse_str(uuid_str).map_err(|_| Error::NotFound {
        resource: "response".to_string(),
        id: response_id.clone(),
    })?;

    // Resolve which fusillade.requests rows back this response, mirroring
    // `get_response`'s resolution:
    //   * Multi-step — head_step exists; walk the chain and collect every
    //     `request_id` from its rows (None on tool_call steps).
    //   * Single-step — head_step_uuid is itself the fusillade.requests id.
    //
    // For the ownership check we use the head row's `created_by` (same row
    // `get_response` authorizes against): the chain is owned end-to-end by
    // the user who initiated the response, so authorizing the head implies
    // authorizing every sub-request in it.
    let (auth_request_id, request_ids_to_delete): (
        fusillade::RequestId,
        Vec<fusillade::RequestId>,
    ) = match state.response_step_manager.as_ref() {
        Some(step_manager) => match step_manager
            .get_step(fusillade::StepId(head_step_uuid))
            .await
            .map_err(|e| {
                Error::Database(crate::db::errors::DbError::Other(anyhow::anyhow!("{e}")))
            })? {
            Some(head_step) => {
                let chain = step_manager
                    .list_chain(fusillade::StepId(head_step_uuid))
                    .await
                    .map_err(|e| {
                        Error::Database(crate::db::errors::DbError::Other(anyhow::anyhow!("{e}")))
                    })?;
                let ids: Vec<fusillade::RequestId> = chain
                    .iter()
                    .filter_map(|s| s.request_id)
                    .collect::<HashSet<_>>()
                    .into_iter()
                    .collect();
                let auth_id = head_step
                    .request_id
                    .unwrap_or(fusillade::RequestId(head_step_uuid));
                // Fall back to the auth id if the chain query returned nothing
                // (head with no model_call sub-request, e.g. a pure tool_call
                // head — unusual but not impossible).
                let ids = if ids.is_empty() { vec![auth_id] } else { ids };
                (auth_id, ids)
            }
            None => {
                let id = fusillade::RequestId(head_step_uuid);
                (id, vec![id])
            }
        },
        None => {
            let id = fusillade::RequestId(head_step_uuid);
            (id, vec![id])
        }
    };

    // Ownership check against the head row's created_by. 404 (not 403) to
    // avoid leaking existence of other users' responses.
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

    if detail.created_by.as_str() != owner_id {
        return Err(Error::NotFound {
            resource: "response".to_string(),
            id: response_id,
        });
    }

    // Delete each backing row. response_steps cascade via FK as each request
    // is removed. We tolerate `RequestNotFound` on individual rows so retries
    // of a partial-failure delete are idempotent.
    for id in request_ids_to_delete {
        match state.request_manager.delete_request(id).await {
            Ok(()) => {}
            Err(fusillade::FusilladeError::RequestNotFound(_)) => {}
            Err(e) => {
                return Err(Error::Database(crate::db::errors::DbError::Other(
                    anyhow::anyhow!("{e}"),
                )));
            }
        }
    }

    Ok(StatusCode::NO_CONTENT)
}
