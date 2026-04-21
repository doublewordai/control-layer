//! Handler for retrieving Open Responses API responses.
//!
//! `GET /ai/v1/responses/{response_id}` reads directly from fusillade's
//! `requests` table, mapping the row to an Open Responses API Response object.

use axum::{
    Json,
    extract::{Path, State},
};
use sqlx_pool_router::PoolProvider;

use crate::AppState;
use crate::errors::{Error, Result};

/// Retrieve a response by ID.
///
/// Returns the Open Responses API Response object for any request — realtime,
/// background, or batch — as long as it has a row in fusillade's requests table.
#[tracing::instrument(skip_all)]
pub async fn get_response<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(response_id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let response = state.response_store.get_response(&response_id).await.map_err(|e| {
        tracing::error!(error = %e, "Failed to retrieve response");
        Error::Database(crate::db::errors::DbError::Other(anyhow::anyhow!("{e}")))
    })?;

    match response {
        Some(resp) => Ok(Json(resp)),
        None => Err(Error::NotFound {
            resource: "response".to_string(),
            id: response_id,
        }),
    }
}
