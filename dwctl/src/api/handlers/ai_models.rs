//! OpenAI-compatible model listing for inference API keys.

use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{QueryBuilder, Row};
use sqlx_pool_router::PoolProvider;

use crate::AppState;

const EVERYONE_GROUP_ID: uuid::Uuid = uuid::Uuid::nil();

#[derive(Serialize)]
pub struct ModelsListResponse {
    object: String,
    data: Vec<ModelObject>,
}

#[derive(Debug, Deserialize)]
pub struct ModelsListQuery {
    group: Option<String>,
    available_for_realtime: Option<String>,
}

enum ModelsListQueryError {
    InvalidGroup,
    InvalidBoolean { param: &'static str },
}

impl ModelsListQueryError {
    fn into_response(self) -> Response {
        match self {
            Self::InvalidGroup => openai_error(
                StatusCode::BAD_REQUEST,
                "Invalid group query parameter. Expected comma-separated UUIDs.",
                "invalid_request_error",
                "invalid_group",
            ),
            Self::InvalidBoolean { param } => openai_error(
                StatusCode::BAD_REQUEST,
                &format!("Invalid {param} query parameter. Expected true or false."),
                "invalid_request_error",
                "invalid_query_parameter",
            ),
        }
    }
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
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?.trim();
    let (scheme, token) = value.split_once(char::is_whitespace)?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then_some(token.trim())
        .filter(|token| !token.is_empty())
}

fn database_error(operation: &str, error: impl std::fmt::Display) -> Response {
    tracing::error!(%error, operation, "Failed to list AI models");
    openai_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "Internal server error",
        "server_error",
        "database_error",
    )
}

fn parse_group_filter(group: Option<&str>) -> Result<Vec<uuid::Uuid>, ModelsListQueryError> {
    let Some(group) = group else {
        return Ok(Vec::new());
    };

    group
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.parse::<uuid::Uuid>().map_err(|_| ModelsListQueryError::InvalidGroup))
        .collect()
}

fn parse_optional_bool(param: &'static str, value: Option<&str>) -> Result<Option<bool>, ModelsListQueryError> {
    let Some(value) = value else {
        return Ok(None);
    };

    match value.trim().to_ascii_lowercase().as_str() {
        "true" => Ok(Some(true)),
        "false" => Ok(Some(false)),
        _ => Err(ModelsListQueryError::InvalidBoolean { param }),
    }
}

/// List active models that the presented inference API key has group access to.
///
/// This intentionally does not filter by credit balance. Credit balance controls
/// dispatch eligibility in the onwards key sync; model discovery should reflect
/// access grants so users can still see what would be available after top-up.
pub async fn list_ai_models<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Query(query): Query<ModelsListQuery>,
    headers: HeaderMap,
) -> Result<Json<ModelsListResponse>, Response> {
    let Some(token) = bearer_token(&headers) else {
        return Err(openai_error(
            StatusCode::UNAUTHORIZED,
            "Missing Authorization header",
            "authentication_error",
            "missing_authorization",
        ));
    };

    let group_ids = parse_group_filter(query.group.as_deref()).map_err(ModelsListQueryError::into_response)?;
    let available_for_realtime = parse_optional_bool("available_for_realtime", query.available_for_realtime.as_deref())
        .map_err(ModelsListQueryError::into_response)?;

    let mut conn = state
        .db
        .read()
        .acquire()
        .await
        .map_err(|e| database_error("acquire_read_connection", e))?;

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
    .map_err(|e| database_error("lookup_api_key", e))?;

    let Some(user_id) = user_id else {
        return Err(openai_error(
            StatusCode::UNAUTHORIZED,
            "Invalid API key",
            "authentication_error",
            "invalid_api_key",
        ));
    };

    let mut models_query = QueryBuilder::new(
        r#"
        SELECT DISTINCT
            dm.alias,
            EXTRACT(EPOCH FROM dm.created_at)::BIGINT AS created
        FROM deployed_models dm
        INNER JOIN deployment_groups dg ON dg.deployment_id = dm.id
        WHERE dm.deleted = FALSE
          AND dm.status = 'active'
          AND (
              dg.group_id = "#,
    );
    models_query.push_bind(EVERYONE_GROUP_ID);
    models_query.push(
        r#"
              OR dg.group_id IN (
                  SELECT ug.group_id
                  FROM user_groups ug
                  WHERE ug.user_id = "#,
    );
    models_query.push_bind(user_id);
    models_query.push(
        r#"
              )
        "#,
    );
    models_query.push(")");

    if !group_ids.is_empty() {
        models_query.push(" AND dg.group_id = ANY(");
        models_query.push_bind(group_ids);
        models_query.push(")");
    }

    if let Some(available_for_realtime) = available_for_realtime {
        if available_for_realtime {
            models_query.push(
                " AND NOT EXISTS (
                    SELECT 1 FROM model_traffic_rules mtr
                    WHERE mtr.deployed_model_id = dm.id
                    AND mtr.api_key_purpose = 'realtime'
                    AND mtr.action = 'deny'
                )",
            );
        } else {
            models_query.push(
                " AND EXISTS (
                    SELECT 1 FROM model_traffic_rules mtr
                    WHERE mtr.deployed_model_id = dm.id
                    AND mtr.api_key_purpose = 'realtime'
                    AND mtr.action = 'deny'
                )",
            );
        }
    }

    models_query.push(" ORDER BY dm.alias");

    let rows = models_query
        .build()
        .fetch_all(&mut *conn)
        .await
        .map_err(|e| database_error("list_accessible_models", e))?;

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
