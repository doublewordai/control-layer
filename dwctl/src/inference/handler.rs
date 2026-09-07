//! Handler for retrieving Open Responses API responses.
//!
//! `GET /ai/v1/responses/{response_id}` resolves through fusillade's live or
//! active retained-response route and maps the snapshot to an Open Responses
//! API Response object.
//!
//! Authentication is via Bearer API key (same as AI proxy requests).
//! Ownership is verified by checking the resolved request snapshot's
//! `created_by` against the API key's `user_id` (which is the org ID for
//! org-scoped keys).

use crate::inference::response_store::StoreError;
use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use fusillade::Storage;
use serde::{Deserialize, Serialize};
use sqlx_pool_router::PoolProvider;
use utoipa::ToSchema;

use crate::AppState;
use crate::errors::{Error, Result};
use crate::inference::store::ResponseLookup;

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
/// live or retained fusillade request carries `created_by` for ownership.
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

    // Parse the response ID: `resp_<uuid>` is the fusillade.requests id.
    let uuid_str = response_id.strip_prefix("resp_").unwrap_or(&response_id);
    let response_uuid = uuid::Uuid::parse_str(uuid_str).map_err(|_| Error::NotFound {
        resource: "response".to_string(),
        id: response_id.clone(),
    })?;

    // Resolve the live row or retained snapshot that carries `created_by`
    // for ownership. The auth resolution happens here on the row that's
    // actually backing this response; `response_store.get_response` then
    // assembles the API envelope.
    let auth_request_id = fusillade::RequestId(response_uuid);

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
/// Right-to-erasure: atomically hard-deletes the complete live or retained
/// response graph, including every dedicated batchless template.
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
#[tracing::instrument(skip_all)]
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
    let response_uuid = uuid::Uuid::parse_str(uuid_str).map_err(|_| Error::NotFound {
        resource: "response".to_string(),
        id: response_id.clone(),
    })?;

    state
        .request_manager
        .delete_owned_response_group(response_uuid, &owner_id)
        .await
        .map_err(|error| match error {
            fusillade::FusilladeError::RequestNotFound(_) => Error::NotFound {
                resource: "response".to_string(),
                id: response_id.clone(),
            },
            _ => erasure_error(&error),
        })?;

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

/// Map a retained-store erasure failure without leaking payload. A day that
/// is mid-retirement is a retryable conflict, not a server fault; everything
/// else keeps only its content-free class so the log line is diagnosable.
fn erasure_error(error: &fusillade::FusilladeError) -> Error {
    use fusillade::RetainedResponseMaintenanceError as Maintenance;
    match Maintenance::from_fusillade_error(error) {
        Some(Maintenance::RetirementPending) => Error::Conflict {
            message: "Response erasure is temporarily unavailable while retention maintenance runs; retry shortly".to_string(),
            conflicts: None,
        },
        Some(class) => Error::Database(crate::db::errors::DbError::Other(anyhow::anyhow!(
            "Response erasure failed ({class:?})"
        ))),
        None => Error::Database(crate::db::errors::DbError::Other(anyhow::anyhow!(
            "Response erasure failed (storage)"
        ))),
    }
}
