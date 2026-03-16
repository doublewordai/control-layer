//! HTTP handlers for tool source management endpoints.

use sqlx_pool_router::PoolProvider;

use crate::api::models::tool_sources::{ToolSourceCreate, ToolSourceResponse, ToolSourceUpdate};
use crate::auth::permissions::{RequiresPermission, operation, resource};
use crate::db::handlers::ToolSources;
use crate::db::models::tool_sources::{ToolSourceCreateDBRequest, ToolSourceUpdateDBRequest};
use crate::errors::{Error, Result};
use crate::{AppState, types::DeploymentId};
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Tool source CRUD
// ---------------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/tool-sources",
    tag = "tool-sources",
    summary = "List tool sources",
    responses(
        (status = 200, description = "List of tool sources", body = Vec<ToolSourceResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn list_tool_sources<P: PoolProvider>(
    State(state): State<AppState<P>>,
    _: RequiresPermission<resource::ToolSources, operation::ReadAll>,
) -> Result<Json<Vec<ToolSourceResponse>>> {
    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ToolSources::new(&mut conn);
    let sources = repo.list().await?;
    Ok(Json(sources.into_iter().map(ToolSourceResponse::from).collect()))
}

#[utoipa::path(
    post,
    path = "/tool-sources",
    tag = "tool-sources",
    summary = "Create tool source",
    request_body = ToolSourceCreate,
    responses(
        (status = 201, description = "Tool source created", body = ToolSourceResponse),
        (status = 400, description = "Bad request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn create_tool_source<P: PoolProvider>(
    State(state): State<AppState<P>>,
    _: RequiresPermission<resource::ToolSources, operation::CreateAll>,
    Json(body): Json<ToolSourceCreate>,
) -> Result<(StatusCode, Json<ToolSourceResponse>)> {
    if body.timeout_secs <= 0 {
        return Err(Error::BadRequest {
            message: "timeout_secs must be positive".to_string(),
        });
    }

    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ToolSources::new(&mut conn);

    let request = ToolSourceCreateDBRequest {
        kind: body.kind,
        name: body.name,
        description: body.description,
        parameters: body.parameters,
        url: body.url,
        api_key: body.api_key,
        timeout_secs: body.timeout_secs,
    };

    let source = repo.create(&request).await?;
    Ok((StatusCode::CREATED, Json(ToolSourceResponse::from(source))))
}

#[utoipa::path(
    get,
    path = "/tool-sources/{id}",
    tag = "tool-sources",
    summary = "Get tool source",
    params(
        ("id" = Uuid, Path, description = "Tool source ID"),
    ),
    responses(
        (status = 200, description = "Tool source details", body = ToolSourceResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn get_tool_source<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(id): Path<Uuid>,
    _: RequiresPermission<resource::ToolSources, operation::ReadAll>,
) -> Result<Json<ToolSourceResponse>> {
    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ToolSources::new(&mut conn);

    let source = repo.get_by_id(id).await?.ok_or_else(|| Error::NotFound {
        resource: "ToolSource".to_string(),
        id: id.to_string(),
    })?;

    Ok(Json(ToolSourceResponse::from(source)))
}

#[utoipa::path(
    patch,
    path = "/tool-sources/{id}",
    tag = "tool-sources",
    summary = "Update tool source",
    params(
        ("id" = Uuid, Path, description = "Tool source ID"),
    ),
    request_body = ToolSourceUpdate,
    responses(
        (status = 200, description = "Tool source updated", body = ToolSourceResponse),
        (status = 400, description = "Bad request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn update_tool_source<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(id): Path<Uuid>,
    _: RequiresPermission<resource::ToolSources, operation::UpdateAll>,
    Json(body): Json<ToolSourceUpdate>,
) -> Result<Json<ToolSourceResponse>> {
    if let Some(timeout) = body.timeout_secs
        && timeout <= 0
    {
        return Err(Error::BadRequest {
            message: "timeout_secs must be positive".to_string(),
        });
    }

    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ToolSources::new(&mut conn);

    let request = ToolSourceUpdateDBRequest {
        name: body.name,
        description: body.description,
        parameters: body.parameters,
        url: body.url,
        api_key: body.api_key,
        timeout_secs: body.timeout_secs,
    };

    let source = repo.update(id, &request).await?;
    Ok(Json(ToolSourceResponse::from(source)))
}

#[utoipa::path(
    delete,
    path = "/tool-sources/{id}",
    tag = "tool-sources",
    summary = "Delete tool source",
    params(
        ("id" = Uuid, Path, description = "Tool source ID"),
    ),
    responses(
        (status = 204, description = "Deleted"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn delete_tool_source<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(id): Path<Uuid>,
    _: RequiresPermission<resource::ToolSources, operation::DeleteAll>,
) -> Result<StatusCode> {
    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ToolSources::new(&mut conn);

    let deleted = repo.delete(id).await?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Error::NotFound {
            resource: "ToolSource".to_string(),
            id: id.to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// Deployment attachment endpoints
// ---------------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/deployments/{id}/tool-sources",
    tag = "tool-sources",
    summary = "List tool sources for deployment",
    params(
        ("id" = Uuid, Path, description = "Deployment ID"),
    ),
    responses(
        (status = 200, description = "List of attached tool sources", body = Vec<ToolSourceResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn list_deployment_tool_sources<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(deployment_id): Path<DeploymentId>,
    _: RequiresPermission<resource::ToolSources, operation::ReadAll>,
) -> Result<Json<Vec<ToolSourceResponse>>> {
    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ToolSources::new(&mut conn);
    let sources = repo.list_for_deployment(deployment_id).await?;
    Ok(Json(sources.into_iter().map(ToolSourceResponse::from).collect()))
}

#[utoipa::path(
    put,
    path = "/deployments/{id}/tool-sources/{source_id}",
    tag = "tool-sources",
    summary = "Attach tool source to deployment",
    params(
        ("id" = Uuid, Path, description = "Deployment ID"),
        ("source_id" = Uuid, Path, description = "Tool source ID"),
    ),
    responses(
        (status = 204, description = "Attached"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Deployment or tool source not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn attach_tool_source_to_deployment<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path((deployment_id, tool_source_id)): Path<(DeploymentId, Uuid)>,
    _: RequiresPermission<resource::ToolSources, operation::UpdateAll>,
) -> Result<StatusCode> {
    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ToolSources::new(&mut conn);
    repo.attach_to_deployment(deployment_id, tool_source_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete,
    path = "/deployments/{id}/tool-sources/{source_id}",
    tag = "tool-sources",
    summary = "Detach tool source from deployment",
    params(
        ("id" = Uuid, Path, description = "Deployment ID"),
        ("source_id" = Uuid, Path, description = "Tool source ID"),
    ),
    responses(
        (status = 204, description = "Detached"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Attachment not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn detach_tool_source_from_deployment<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path((deployment_id, tool_source_id)): Path<(DeploymentId, Uuid)>,
    _: RequiresPermission<resource::ToolSources, operation::UpdateAll>,
) -> Result<StatusCode> {
    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ToolSources::new(&mut conn);
    let removed = repo.detach_from_deployment(deployment_id, tool_source_id).await?;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Error::NotFound {
            resource: "DeploymentToolSource".to_string(),
            id: format!("{deployment_id}/{tool_source_id}"),
        })
    }
}

// ---------------------------------------------------------------------------
// Group attachment endpoints
// ---------------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/groups/{id}/tool-sources",
    tag = "tool-sources",
    summary = "List tool sources for group",
    params(
        ("id" = Uuid, Path, description = "Group ID"),
    ),
    responses(
        (status = 200, description = "List of attached tool sources", body = Vec<ToolSourceResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn list_group_tool_sources<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(group_id): Path<Uuid>,
    _: RequiresPermission<resource::ToolSources, operation::ReadAll>,
) -> Result<Json<Vec<ToolSourceResponse>>> {
    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ToolSources::new(&mut conn);
    let sources = repo.list_for_group(group_id).await?;
    Ok(Json(sources.into_iter().map(ToolSourceResponse::from).collect()))
}

#[utoipa::path(
    put,
    path = "/groups/{id}/tool-sources/{source_id}",
    tag = "tool-sources",
    summary = "Attach tool source to group",
    params(
        ("id" = Uuid, Path, description = "Group ID"),
        ("source_id" = Uuid, Path, description = "Tool source ID"),
    ),
    responses(
        (status = 204, description = "Attached"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Group or tool source not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn attach_tool_source_to_group<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path((group_id, tool_source_id)): Path<(Uuid, Uuid)>,
    _: RequiresPermission<resource::ToolSources, operation::UpdateAll>,
) -> Result<StatusCode> {
    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ToolSources::new(&mut conn);
    repo.attach_to_group(group_id, tool_source_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    delete,
    path = "/groups/{id}/tool-sources/{source_id}",
    tag = "tool-sources",
    summary = "Detach tool source from group",
    params(
        ("id" = Uuid, Path, description = "Group ID"),
        ("source_id" = Uuid, Path, description = "Tool source ID"),
    ),
    responses(
        (status = 204, description = "Detached"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Attachment not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn detach_tool_source_from_group<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path((group_id, tool_source_id)): Path<(Uuid, Uuid)>,
    _: RequiresPermission<resource::ToolSources, operation::UpdateAll>,
) -> Result<StatusCode> {
    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ToolSources::new(&mut conn);
    let removed = repo.detach_from_group(group_id, tool_source_id).await?;
    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Error::NotFound {
            resource: "GroupToolSource".to_string(),
            id: format!("{group_id}/{tool_source_id}"),
        })
    }
}
