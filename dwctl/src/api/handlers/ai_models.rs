//! OpenAI-compatible model listing for inference API keys.

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use serde_json::json;
use sqlx::Row;
use sqlx_pool_router::PoolProvider;

use crate::AppState;

const EVERYONE_GROUP_ID: uuid::Uuid = uuid::Uuid::nil();

#[derive(Serialize)]
pub struct ModelsListResponse {
    object: String,
    data: Vec<ModelObject>,
}

#[derive(Serialize)]
struct ModelObject {
    id: String,
    object: String,
    created: i64,
    owned_by: String,
}

fn openai_error(status: StatusCode, message: &str, error_type: &str, code: &str) -> Response {
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": error_type,
                "param": null,
                "code": code
            }
        })),
    )
        .into_response()
}

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

/// List active models that the presented inference API key has group access to.
///
/// This intentionally does not filter by credit balance. Credit balance controls
/// dispatch eligibility in the onwards key sync; model discovery should reflect
/// access grants so users can still see what would be available after top-up.
pub async fn list_ai_models<P: PoolProvider>(
    State(state): State<AppState<P>>,
    headers: HeaderMap,
) -> Result<Json<ModelsListResponse>, Response> {
    let Some(token) = bearer_token(&headers) else {
        return Err(openai_error(
            StatusCode::UNAUTHORIZED,
            "Missing Authorization header",
            "invalid_request_error",
            "missing_authorization",
        ));
    };

    let mut conn = state.db.read().acquire().await.map_err(|e| {
        openai_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Database error: {e}"),
            "server_error",
            "database_error",
        )
    })?;

    let user_id = sqlx::query_scalar::<_, uuid::Uuid>(
        r#"
        SELECT ak.user_id
        FROM api_keys ak
        INNER JOIN users u ON u.id = ak.user_id
        WHERE ak.secret = $1
          AND ak.is_deleted = FALSE
          AND u.is_deleted = FALSE
          AND ak.purpose IN ('realtime', 'batch', 'playground')
        LIMIT 1
        "#,
    )
    .bind(token)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| {
        openai_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Database error: {e}"),
            "server_error",
            "database_error",
        )
    })?;

    let Some(user_id) = user_id else {
        return Err(openai_error(
            StatusCode::FORBIDDEN,
            "Invalid API key",
            "invalid_request_error",
            "invalid_api_key",
        ));
    };

    let rows = sqlx::query(
        r#"
        SELECT DISTINCT
            dm.alias,
            EXTRACT(EPOCH FROM dm.created_at)::BIGINT AS created
        FROM deployed_models dm
        INNER JOIN deployment_groups dg ON dg.deployment_id = dm.id
        WHERE dm.deleted = FALSE
          AND dm.status = 'active'
          AND (
              dg.group_id = $2
              OR dg.group_id IN (
                  SELECT ug.group_id
                  FROM user_groups ug
                  WHERE ug.user_id = $1
              )
        )
        ORDER BY dm.alias
        "#,
    )
    .bind(user_id)
    .bind(EVERYONE_GROUP_ID)
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| {
        openai_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Database error: {e}"),
            "server_error",
            "database_error",
        )
    })?;

    Ok(Json(ModelsListResponse {
        object: "list".to_string(),
        data: rows
            .into_iter()
            .map(|row| ModelObject {
                id: row.get("alias"),
                object: "model".to_string(),
                created: row.get::<Option<i64>, _>("created").unwrap_or_default(),
                owned_by: "None".to_string(),
            })
            .collect(),
    }))
}

pub async fn list_ai_models_middleware<P: PoolProvider>(
    State(state): State<AppState<P>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if request.method() == axum::http::Method::GET && request.uri().path() == "/models" {
        let headers = request.headers().clone();
        return match list_ai_models(State(state), headers).await {
            Ok(response) => response.into_response(),
            Err(response) => response,
        };
    }

    next.run(request).await
}
