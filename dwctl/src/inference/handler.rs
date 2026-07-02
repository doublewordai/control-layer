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
use serde::{Deserialize, Serialize};
use sqlx_pool_router::PoolProvider;
use std::collections::HashSet;
use utoipa::ToSchema;

use crate::AppState;
use crate::inference::store::ResponseLookup;
use crate::errors::{Error, Result};

/// Response body for `DELETE /v1/responses/{response_id}`.
///
/// Matches the OpenAI Responses API delete shape — clients (including the
/// OpenAI SDK) parse this into a typed `ResponseDeleted`, so the field names
/// and the literal `"object": "response"` discriminator are load-bearing.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[schema(example = json!({
    "id": "resp_abc123",
    "object": "response",
    "deleted": true
}))]
pub struct ResponseDeleted {
    #[schema(example = "resp_abc123")]
    pub id: String,
    #[serde(rename = "object")]
    #[schema(example = "response")]
    pub object_type: ResponseDeletedObjectType,
    #[schema(example = true)]
    pub deleted: bool,
}

/// Always `"response"` for the deletion envelope.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ResponseDeletedObjectType {
    Response,
}

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

    // Convert the detail to an Open Responses API response object. ZDR gone
    // (encrypted body whose key was shredded on a prior retrieval or expiry) is
    // decided inside `get_response` from the same keystore lookup that would
    // decrypt, so there is no separate probe and no time-of-check gap.
    match state.response_store.get_response(&response_id).await.map_err(|e| match e {
        StoreError::NotFound(_) => Error::NotFound {
            resource: "response".to_string(),
            id: response_id.clone(),
        },
        _ => Error::Database(crate::db::errors::DbError::Other(anyhow::anyhow!("{e}"))),
    })? {
        ResponseLookup::Found(resp) => Ok(Json(resp)),
        ResponseLookup::NotFound => Err(Error::NotFound {
            resource: "response".to_string(),
            id: response_id,
        }),
        ResponseLookup::Gone => Err(Error::Gone {
            message: "zdr_request_unavailable: this zero-data-retention response is no longer available".to_string(),
        }),
    }
}

/// Delete a response by ID.
///
/// Right-to-erasure: hard-deletes every `fusillade.requests` row that backs
/// the response (and each row's dedicated batchless `request_templates`
/// row carrying the prompt body — see `fusillade::Storage::delete_request`).
/// For multi-step responses, walks the `response_steps` chain from the head
/// step and deletes each step's sub-request — the `response_steps` rows
/// themselves cascade via FK. For single-step responses (chat completions
/// / embeddings retrieved via the same GET surface), deletes the one row.
///
/// **Preserved by design**: `http_analytics` has no FK to requests (token
/// counts, cost, status code), and `credits_transactions` is immutable
/// (denormalized `fusillade_batch_id` only, no request-id link). Billing
/// and usage records survive an erasure of the inference data.
///
/// **Auth pattern** mirrors `get_response`: direct lookup against
/// `public.api_keys` for the Bearer key. Session/cookie-authed dashboard
/// deletes flow through `DELETE /admin/api/v1/batches/requests/{id}`
/// instead (uses the standard `RequiresPermission` middleware) — the
/// `admin_ai_proxy_middleware` rewrite path used by chat-completions /
/// responses POST can't cover GET/DELETE because it requires a request body
/// to extract the model name.
#[utoipa::path(
    delete,
    path = "/responses/{response_id}",
    tag = "responses-api",
    summary = "Delete response",
    description = "Hard-deletes a response and every fusillade row that backs it (\
including the dedicated request_templates row carrying the prompt body). \
Provided for right-to-erasure compliance.

Token / cost analytics in `http_analytics` and billing transactions in \
`credits_transactions` are preserved — they are denormalized off the fusillade \
schema and survive the erasure.",
    params(
        ("response_id" = String, Path, description = "The response ID returned when the response was created (with or without the `resp_` prefix).")
    ),
    responses(
        (status = 200, description = "Response deleted.", body = ResponseDeleted),
        (status = 401, description = "Invalid or missing API key. Ensure your `Authorization` header is set to `Bearer YOUR_API_KEY`."),
        (status = 404, description = "Response not found or you don't have access to it."),
        (status = 500, description = "An unexpected error occurred. Retry the request or contact support if the issue persists.")
    ),
    security(("BearerAuth" = []))
)]
#[tracing::instrument(skip_all, fields(response_id = %response_id))]
pub async fn delete_response<P: PoolProvider>(
    State(state): State<AppState<P>>,
    headers: HeaderMap,
    Path(response_id): Path<String>,
) -> Result<Json<ResponseDeleted>> {
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
    //     `request_id` from its rows (`None` on tool_call steps, which have
    //     no backing fusillade row — tool dispatch lives in
    //     `tool_call_analytics`).
    //   * Single-step — head_step_uuid is itself the fusillade.requests id.
    //
    // For the ownership check we use the head row's `created_by` (same row
    // `get_response` authorizes against): the chain is owned end-to-end by
    // the user who initiated the response, so authorizing the head implies
    // authorizing every sub-request in it.
    let (auth_request_id, request_ids_to_delete): (fusillade::RequestId, Vec<fusillade::RequestId>) =
        match state.response_step_manager.as_ref() {
            Some(step_manager) => match step_manager
                .get_step(fusillade::StepId(head_step_uuid))
                .await
                .map_err(|e| Error::Database(crate::db::errors::DbError::Other(anyhow::anyhow!("{e}"))))?
            {
                Some(head_step) => {
                    let chain = step_manager
                        .list_chain(fusillade::StepId(head_step_uuid))
                        .await
                        .map_err(|e| Error::Database(crate::db::errors::DbError::Other(anyhow::anyhow!("{e}"))))?;
                    let ids: Vec<fusillade::RequestId> = chain
                        .iter()
                        .filter_map(|s| s.request_id)
                        .collect::<HashSet<_>>()
                        .into_iter()
                        .collect();
                    // Fallback: a pure tool_call head with no model_call descendants
                    // has an empty chain (no `request_id`s anywhere). Treat the head
                    // step uuid as a fusillade.requests id — the delete loop below
                    // tolerates `RequestNotFound` so this resolves to a 404 on the
                    // ownership check rather than a 500.
                    let auth_id = head_step.request_id.unwrap_or(fusillade::RequestId(head_step_uuid));
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

    // Best-effort multi-row deletion. Each delete_request call is atomic in
    // fusillade (single transaction), but the loop itself isn't — if a row
    // delete fails partway through a chain, earlier rows are already gone.
    // We continue past failures (instead of bailing on the first one) so the
    // erasure makes maximum progress; remaining rows can be cleaned up by a
    // retry of the DELETE call (RequestNotFound is tolerated for already-
    // deleted rows). Per-row failures are logged at error level so partial
    // states are reconcilable from logs.
    //
    // A future fusillade primitive that accepts `Vec<RequestId>` and deletes
    // them in one transaction would close this gap entirely.
    let total = request_ids_to_delete.len();
    let mut failed: Vec<fusillade::RequestId> = Vec::new();
    for id in request_ids_to_delete {
        match state.request_manager.delete_request(id).await {
            Ok(()) => {}
            Err(fusillade::FusilladeError::RequestNotFound(_)) => {}
            Err(e) => {
                tracing::error!(
                    response_id = %response_id,
                    request_id = %*id,
                    error = %e,
                    "delete_response: per-row delete failed; continuing to maximize erasure progress",
                );
                failed.push(id);
            }
        }
    }

    if !failed.is_empty() {
        tracing::error!(
            response_id = %response_id,
            failed_count = failed.len(),
            total_count = total,
            "delete_response: partial failure; client may retry the DELETE to clean up remaining rows",
        );
        return Err(Error::Database(crate::db::errors::DbError::Other(anyhow::anyhow!(
            "deleted {}/{} backing rows for response {}; {} remain — retry to complete erasure",
            total - failed.len(),
            total,
            response_id,
            failed.len(),
        ))));
    }

    // Echo the id back in the canonical OpenAI form (with `resp_` prefix if
    // the caller used it). Matches the OpenAI Responses API delete shape so
    // clients (including the OpenAI SDK's `ResponseDeleted` parser) accept
    // the body.
    Ok(Json(ResponseDeleted {
        id: response_id,
        object_type: ResponseDeletedObjectType::Response,
        deleted: true,
    }))
}
