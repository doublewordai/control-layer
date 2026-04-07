//! HTTP handlers for connections and sync operations.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use sqlx_pool_router::PoolProvider;
use uuid::Uuid;

use crate::AppState;
use crate::api::models::connections::{
    ConnectionListResponse, ConnectionResponse, ConnectionTestResponse, CreateConnectionRequest, ExternalFileListResponse,
    ExternalFileResponse, ListConnectionsQuery, ListExternalFilesQuery, SyncEntryListResponse, SyncEntryResponse,
    SyncOperationListResponse, SyncOperationResponse, SyncedKeyResponse, TriggerSyncRequest,
};
use crate::auth::permissions::{RequiresPermission, operation, resource};
use crate::connections::provider::{self, ProviderError};
use crate::db::handlers::connections::{Connections, SyncEntries, SyncOperations};
use crate::encryption;
use crate::errors::{Error, Result};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn get_encryption_key<P: PoolProvider>(state: &AppState<P>) -> Result<Vec<u8>> {
    let config = state.config.snapshot();
    let secret = config
        .connections
        .encryption_key
        .as_deref()
        .or(config.secret_key.as_deref())
        .ok_or_else(|| Error::Internal {
            operation: "connections encryption key not configured".to_string(),
        })?;
    encryption::derive_encryption_key(secret).map_err(|e| Error::Internal {
        operation: format!("invalid encryption key: {e}"),
    })
}

fn map_provider_error(e: ProviderError) -> Error {
    match e {
        ProviderError::AuthenticationFailed(msg) => Error::BadRequest {
            message: format!("authentication failed: {msg}"),
        },
        ProviderError::AccessDenied(msg) => Error::BadRequest {
            message: format!("access denied: {msg}"),
        },
        ProviderError::NotFound(msg) => Error::NotFound {
            resource: "external file".to_string(),
            id: msg,
        },
        ProviderError::InvalidConfig(msg) => Error::BadRequest {
            message: format!("invalid provider config: {msg}"),
        },
        ProviderError::Internal(msg) => Error::Internal {
            operation: format!("provider error: {msg}"),
        },
    }
}

// ---------------------------------------------------------------------------
// Connection CRUD
// ---------------------------------------------------------------------------

#[utoipa::path(
    post,
    path = "/connections",
    tag = "connections",
    summary = "Create connection",
    request_body = CreateConnectionRequest,
    responses(
        (status = 201, body = ConnectionResponse),
        (status = 400, description = "Invalid request"),
    )
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id))]
pub async fn create_connection<P: PoolProvider>(
    State(state): State<AppState<P>>,
    current_user: RequiresPermission<resource::Connections, operation::CreateOwn>,
    Json(req): Json<CreateConnectionRequest>,
) -> Result<(StatusCode, Json<ConnectionResponse>)> {
    let kind = req.kind.as_deref().unwrap_or("source");
    if kind != "source" {
        return Err(Error::BadRequest {
            message: "only kind=\"source\" is supported".to_string(),
        });
    }

    // Validate provider type
    if !matches!(req.provider.as_str(), "s3") {
        return Err(Error::BadRequest {
            message: format!("unsupported provider: {}. Supported: s3", req.provider),
        });
    }

    // Validate config parses correctly for the provider
    provider::create_provider(&req.provider, req.config.clone()).map_err(map_provider_error)?;

    let key = get_encryption_key(&state)?;
    let encrypted = encryption::encrypt_json(&key, &req.config).map_err(|e| Error::Internal {
        operation: format!("encrypt config: {e}"),
    })?;

    let target_user_id = current_user.active_organization.unwrap_or(current_user.id);

    // Get or create hidden batch key for per-member attribution (same pattern as files/batches).
    // key owned by target_user_id (org or user), created_by = individual user.
    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let (_secret, api_key_id) = crate::db::handlers::api_keys::ApiKeys::new(&mut conn)
        .get_or_create_hidden_key_with_id(target_user_id, crate::db::models::api_keys::ApiKeyPurpose::Batch, current_user.id)
        .await
        .map_err(Error::Database)?;

    let connection = Connections::new(&mut conn)
        .create(target_user_id, Some(api_key_id), kind, &req.provider, &req.name, &encrypted)
        .await
        .map_err(Error::Database)?;

    Ok((StatusCode::CREATED, Json(ConnectionResponse::from(connection))))
}

#[utoipa::path(
    get,
    path = "/connections",
    tag = "connections",
    summary = "List connections",
    responses((status = 200, body = ConnectionListResponse)),
    params(("kind" = Option<String>, Query, description = "Filter by kind (source)"))
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id))]
pub async fn list_connections<P: PoolProvider>(
    State(state): State<AppState<P>>,
    current_user: RequiresPermission<resource::Connections, operation::ReadOwn>,
    Query(query): Query<ListConnectionsQuery>,
) -> Result<Json<ConnectionListResponse>> {
    let target_user_id = current_user.active_organization.unwrap_or(current_user.id);

    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let connections = Connections::new(&mut conn)
        .list_by_user(target_user_id, query.kind.as_deref())
        .await
        .map_err(Error::Database)?;

    Ok(Json(ConnectionListResponse {
        data: connections.into_iter().map(ConnectionResponse::from).collect(),
    }))
}

#[utoipa::path(
    get,
    path = "/connections/{connection_id}",
    tag = "connections",
    summary = "Get connection",
    responses(
        (status = 200, body = ConnectionResponse),
        (status = 404, description = "Connection not found"),
    )
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id, connection_id = %connection_id))]
pub async fn get_connection<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(connection_id): Path<Uuid>,
    current_user: RequiresPermission<resource::Connections, operation::ReadOwn>,
) -> Result<Json<ConnectionResponse>> {
    let target_user_id = current_user.active_organization.unwrap_or(current_user.id);

    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let connection = Connections::new(&mut conn)
        .get_by_id(connection_id)
        .await
        .map_err(Error::Database)?
        .ok_or_else(|| Error::NotFound {
            resource: "Connection".to_string(),
            id: connection_id.to_string(),
        })?;

    // Check ownership
    if connection.user_id != target_user_id {
        return Err(Error::NotFound {
            resource: "Connection".to_string(),
            id: connection_id.to_string(),
        });
    }

    Ok(Json(ConnectionResponse::from(connection)))
}

#[utoipa::path(
    delete,
    path = "/connections/{connection_id}",
    tag = "connections",
    summary = "Delete connection",
    responses(
        (status = 204, description = "Connection deleted"),
        (status = 404, description = "Connection not found"),
    )
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id, connection_id = %connection_id))]
pub async fn delete_connection<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(connection_id): Path<Uuid>,
    current_user: RequiresPermission<resource::Connections, operation::DeleteOwn>,
) -> Result<StatusCode> {
    let target_user_id = current_user.active_organization.unwrap_or(current_user.id);

    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;

    // Verify ownership before deleting
    let connection = Connections::new(&mut conn)
        .get_by_id(connection_id)
        .await
        .map_err(Error::Database)?
        .ok_or_else(|| Error::NotFound {
            resource: "Connection".to_string(),
            id: connection_id.to_string(),
        })?;

    if connection.user_id != target_user_id {
        return Err(Error::NotFound {
            resource: "Connection".to_string(),
            id: connection_id.to_string(),
        });
    }

    Connections::new(&mut conn)
        .soft_delete(connection_id)
        .await
        .map_err(Error::Database)?;

    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Test connection
// ---------------------------------------------------------------------------

#[utoipa::path(
    post,
    path = "/connections/{connection_id}/test",
    tag = "connections",
    summary = "Test connection",
    responses(
        (status = 200, body = ConnectionTestResponse),
        (status = 404, description = "Connection not found"),
    )
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id, connection_id = %connection_id))]
pub async fn test_connection<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(connection_id): Path<Uuid>,
    current_user: RequiresPermission<resource::Connections, operation::ReadOwn>,
) -> Result<Json<ConnectionTestResponse>> {
    let target_user_id = current_user.active_organization.unwrap_or(current_user.id);

    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let connection = Connections::new(&mut conn)
        .get_by_id(connection_id)
        .await
        .map_err(Error::Database)?
        .ok_or_else(|| Error::NotFound {
            resource: "Connection".to_string(),
            id: connection_id.to_string(),
        })?;

    if connection.user_id != target_user_id {
        return Err(Error::NotFound {
            resource: "Connection".to_string(),
            id: connection_id.to_string(),
        });
    }

    let key = get_encryption_key(&state)?;
    let config = encryption::decrypt_json(&key, &connection.config_encrypted).map_err(|e| Error::Internal {
        operation: format!("decrypt config: {e}"),
    })?;

    let prov = provider::create_provider(&connection.provider, config).map_err(map_provider_error)?;
    let result = prov.test_connection().await.map_err(map_provider_error)?;

    Ok(Json(ConnectionTestResponse {
        ok: result.ok,
        provider: connection.provider,
        message: result.message,
        scope: result.scope,
    }))
}

// ---------------------------------------------------------------------------
// Sync operations
// ---------------------------------------------------------------------------

#[utoipa::path(
    post,
    path = "/connections/{connection_id}/sync",
    tag = "connections",
    summary = "Trigger sync",
    request_body = TriggerSyncRequest,
    responses(
        (status = 202, body = SyncOperationResponse),
        (status = 404, description = "Connection not found"),
    )
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id, connection_id = %connection_id))]
pub async fn trigger_sync<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(connection_id): Path<Uuid>,
    current_user: RequiresPermission<resource::Connections, operation::CreateOwn>,
    Json(req): Json<TriggerSyncRequest>,
) -> Result<(StatusCode, Json<SyncOperationResponse>)> {
    let target_user_id = current_user.active_organization.unwrap_or(current_user.id);

    // Validate strategy
    if !matches!(req.strategy.as_str(), "snapshot" | "select") {
        return Err(Error::BadRequest {
            message: format!("unsupported strategy: {}. Supported: snapshot, select", req.strategy),
        });
    }

    if req.strategy == "select" && req.file_keys.as_ref().is_none_or(|k| k.is_empty()) {
        return Err(Error::BadRequest {
            message: "strategy \"select\" requires non-empty file_keys".to_string(),
        });
    }

    // Verify connection exists and user owns it
    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let connection = Connections::new(&mut conn)
        .get_by_id(connection_id)
        .await
        .map_err(Error::Database)?
        .ok_or_else(|| Error::NotFound {
            resource: "Connection".to_string(),
            id: connection_id.to_string(),
        })?;

    if connection.user_id != target_user_id {
        return Err(Error::NotFound {
            resource: "Connection".to_string(),
            id: connection_id.to_string(),
        });
    }

    // Build sync config from request + defaults
    let config = state.config.snapshot();
    let host = if config.host == "0.0.0.0" { "127.0.0.1" } else { &config.host };
    let ai_base_url = format!("http://{}:{}/ai", host, config.port);
    let sync_config = serde_json::json!({
        "endpoint": req.endpoint.as_deref().unwrap_or(&config.connections.sync.default_endpoint),
        "completion_window": req.completion_window.as_deref().unwrap_or(&config.connections.sync.default_completion_window),
        "ai_base_url": ai_base_url,
    });

    let strategy_config = match req.strategy.as_str() {
        "select" => Some(serde_json::json!({ "file_keys": req.file_keys, "force": req.force })),
        _ => {
            if req.force {
                Some(serde_json::json!({ "force": true }))
            } else {
                None
            }
        }
    };

    let sync_op = SyncOperations::new(&mut conn)
        .create(
            connection_id,
            current_user.id,
            &req.strategy,
            strategy_config.as_ref(),
            &sync_config,
        )
        .await
        .map_err(Error::Database)?;

    // Enqueue the SyncConnectionJob via underway
    if let Err(e) = state
        .task_runner
        .sync_connection_job
        .enqueue(&crate::connections::sync::SyncConnectionInput {
            sync_id: sync_op.id,
            connection_id,
        })
        .await
    {
        // Mark sync as failed if we can't enqueue
        SyncOperations::new(&mut conn).update_status(sync_op.id, "failed").await.ok();
        return Err(Error::Internal {
            operation: format!("enqueue sync job: {e}"),
        });
    }

    Ok((StatusCode::ACCEPTED, Json(SyncOperationResponse::from(sync_op))))
}

#[utoipa::path(
    get,
    path = "/connections/{connection_id}/syncs",
    tag = "connections",
    summary = "List sync operations",
    responses((status = 200, body = SyncOperationListResponse))
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id, connection_id = %connection_id))]
pub async fn list_syncs<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(connection_id): Path<Uuid>,
    current_user: RequiresPermission<resource::Connections, operation::ReadOwn>,
) -> Result<Json<SyncOperationListResponse>> {
    let target_user_id = current_user.active_organization.unwrap_or(current_user.id);

    // Verify ownership
    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let connection = Connections::new(&mut conn)
        .get_by_id(connection_id)
        .await
        .map_err(Error::Database)?
        .ok_or_else(|| Error::NotFound {
            resource: "Connection".to_string(),
            id: connection_id.to_string(),
        })?;

    if connection.user_id != target_user_id {
        return Err(Error::NotFound {
            resource: "Connection".to_string(),
            id: connection_id.to_string(),
        });
    }

    let syncs = SyncOperations::new(&mut conn)
        .list_by_connection(connection_id)
        .await
        .map_err(Error::Database)?;

    Ok(Json(SyncOperationListResponse {
        data: syncs.into_iter().map(SyncOperationResponse::from).collect(),
    }))
}

#[utoipa::path(
    get,
    path = "/connections/{connection_id}/syncs/{sync_id}",
    tag = "connections",
    summary = "Get sync operation",
    responses(
        (status = 200, body = SyncOperationResponse),
        (status = 404, description = "Sync operation not found"),
    )
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id, sync_id = %sync_id))]
pub async fn get_sync<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path((connection_id, sync_id)): Path<(Uuid, Uuid)>,
    current_user: RequiresPermission<resource::Connections, operation::ReadOwn>,
) -> Result<Json<SyncOperationResponse>> {
    let target_user_id = current_user.active_organization.unwrap_or(current_user.id);

    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;

    // Verify connection ownership
    let connection = Connections::new(&mut conn)
        .get_by_id(connection_id)
        .await
        .map_err(Error::Database)?
        .ok_or_else(|| Error::NotFound {
            resource: "Connection".to_string(),
            id: connection_id.to_string(),
        })?;

    if connection.user_id != target_user_id {
        return Err(Error::NotFound {
            resource: "Connection".to_string(),
            id: connection_id.to_string(),
        });
    }

    let sync_op = SyncOperations::new(&mut conn)
        .get_by_id(sync_id)
        .await
        .map_err(Error::Database)?
        .ok_or_else(|| Error::NotFound {
            resource: "SyncOperation".to_string(),
            id: sync_id.to_string(),
        })?;

    // Verify the sync belongs to this connection
    if sync_op.connection_id != connection_id {
        return Err(Error::NotFound {
            resource: "SyncOperation".to_string(),
            id: sync_id.to_string(),
        });
    }

    Ok(Json(SyncOperationResponse::from(sync_op)))
}

#[utoipa::path(
    get,
    path = "/connections/{connection_id}/syncs/{sync_id}/entries",
    tag = "connections",
    summary = "List sync entries",
    responses((status = 200, body = SyncEntryListResponse))
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id, sync_id = %sync_id))]
pub async fn list_sync_entries<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path((connection_id, sync_id)): Path<(Uuid, Uuid)>,
    current_user: RequiresPermission<resource::Connections, operation::ReadOwn>,
) -> Result<Json<SyncEntryListResponse>> {
    let target_user_id = current_user.active_organization.unwrap_or(current_user.id);

    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;

    // Verify connection ownership
    let connection = Connections::new(&mut conn)
        .get_by_id(connection_id)
        .await
        .map_err(Error::Database)?
        .ok_or_else(|| Error::NotFound {
            resource: "Connection".to_string(),
            id: connection_id.to_string(),
        })?;

    if connection.user_id != target_user_id {
        return Err(Error::NotFound {
            resource: "Connection".to_string(),
            id: connection_id.to_string(),
        });
    }

    // Verify the sync belongs to this connection
    let sync_op = SyncOperations::new(&mut conn)
        .get_by_id(sync_id)
        .await
        .map_err(Error::Database)?
        .ok_or_else(|| Error::NotFound {
            resource: "SyncOperation".to_string(),
            id: sync_id.to_string(),
        })?;
    if sync_op.connection_id != connection_id {
        return Err(Error::NotFound {
            resource: "SyncOperation".to_string(),
            id: sync_id.to_string(),
        });
    }

    let entries = SyncEntries::new(&mut conn).list_by_sync(sync_id).await.map_err(Error::Database)?;

    Ok(Json(SyncEntryListResponse {
        data: entries.into_iter().map(SyncEntryResponse::from).collect(),
    }))
}

// ---------------------------------------------------------------------------
// Synced keys (for UI file status display)
// ---------------------------------------------------------------------------

#[tracing::instrument(skip_all, fields(user_id = %current_user.id, connection_id = %connection_id))]
pub async fn list_synced_keys<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(connection_id): Path<Uuid>,
    current_user: RequiresPermission<resource::Connections, operation::ReadOwn>,
) -> Result<Json<Vec<SyncedKeyResponse>>> {
    let target_user_id = current_user.active_organization.unwrap_or(current_user.id);

    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let connection = Connections::new(&mut conn)
        .get_by_id(connection_id)
        .await
        .map_err(Error::Database)?
        .ok_or_else(|| Error::NotFound {
            resource: "Connection".to_string(),
            id: connection_id.to_string(),
        })?;

    if connection.user_id != target_user_id {
        return Err(Error::NotFound {
            resource: "Connection".to_string(),
            id: connection_id.to_string(),
        });
    }

    let synced = SyncEntries::new(&mut conn)
        .list_synced_keys(connection_id)
        .await
        .map_err(Error::Database)?;

    Ok(Json(
        synced
            .into_iter()
            .map(|(key, last_modified)| SyncedKeyResponse {
                key,
                last_modified: last_modified.map(|dt| dt.timestamp()),
            })
            .collect(),
    ))
}

// ---------------------------------------------------------------------------
// List files from source (external file browsing)
// ---------------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/connections/{connection_id}/files",
    tag = "connections",
    summary = "List files from source",
    responses((status = 200, body = ExternalFileListResponse)),
    params(
        ("connection_id" = Uuid, Path),
        ("limit" = Option<usize>, Query, description = "Max files per page (default 100, max 1000)"),
        ("cursor" = Option<String>, Query, description = "Pagination cursor from previous response"),
        ("search" = Option<String>, Query, description = "Filter by filename (case-insensitive substring match)"),
    )
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id, connection_id = %connection_id))]
pub async fn list_connection_files<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(connection_id): Path<Uuid>,
    Query(query): Query<ListExternalFilesQuery>,
    current_user: RequiresPermission<resource::Connections, operation::ReadOwn>,
) -> Result<Json<ExternalFileListResponse>> {
    let target_user_id = current_user.active_organization.unwrap_or(current_user.id);

    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let connection = Connections::new(&mut conn)
        .get_by_id(connection_id)
        .await
        .map_err(Error::Database)?
        .ok_or_else(|| Error::NotFound {
            resource: "Connection".to_string(),
            id: connection_id.to_string(),
        })?;

    if connection.user_id != target_user_id {
        return Err(Error::NotFound {
            resource: "Connection".to_string(),
            id: connection_id.to_string(),
        });
    }

    let key = get_encryption_key(&state)?;
    let config = encryption::decrypt_json(&key, &connection.config_encrypted).map_err(|e| Error::Internal {
        operation: format!("decrypt config: {e}"),
    })?;

    let prov = provider::create_provider(&connection.provider, config).map_err(map_provider_error)?;
    let page = prov
        .list_files_paged(provider::ListFilesOptions {
            limit: query.limit,
            cursor: query.cursor,
            search: query.search,
        })
        .await
        .map_err(map_provider_error)?;

    Ok(Json(ExternalFileListResponse {
        data: page
            .files
            .into_iter()
            .map(|f| ExternalFileResponse {
                key: f.key,
                size_bytes: f.size_bytes,
                last_modified: f.last_modified.map(|dt| dt.timestamp()),
                display_name: f.display_name,
            })
            .collect(),
        has_more: page.has_more,
        next_cursor: page.next_cursor,
    }))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use sqlx::PgPool;

    use crate::api::models::users::Role;
    use crate::test::utils::{add_auth_headers, create_test_app, create_test_user_with_roles};

    fn create_connection_body(name: &str) -> serde_json::Value {
        serde_json::json!({
            "provider": "s3",
            "name": name,
            "config": {
                "bucket": "test-bucket",
                "region": "us-east-1",
                "access_key_id": "AKIAIOSFODNN7EXAMPLE",
                "secret_access_key": "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
                "endpoint_url": "http://localhost:9000"
            }
        })
    }

    // -- CRUD --

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_connection(pool: PgPool) {
        let (app, _bg) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::PlatformManager]).await;
        let auth = add_auth_headers(&user);

        let resp = app
            .post("/admin/api/v1/connections")
            .json(&create_connection_body("test-conn"))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;

        resp.assert_status(StatusCode::CREATED);
        let body: serde_json::Value = resp.json();
        assert_eq!(body["name"], "test-conn");
        assert_eq!(body["provider"], "s3");
        assert_eq!(body["kind"], "source");
        assert!(body["id"].as_str().is_some());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_connection_requires_platform_manager(pool: PgPool) {
        let (app, _bg) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser]).await;
        let auth = add_auth_headers(&user);

        let resp = app
            .post("/admin/api/v1/connections")
            .json(&create_connection_body("test-conn"))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;

        resp.assert_status(StatusCode::FORBIDDEN);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_connections(pool: PgPool) {
        let (app, _bg) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::PlatformManager]).await;
        let auth = add_auth_headers(&user);

        app.post("/admin/api/v1/connections")
            .json(&create_connection_body("conn-1"))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await
            .assert_status(StatusCode::CREATED);

        app.post("/admin/api/v1/connections")
            .json(&create_connection_body("conn-2"))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await
            .assert_status(StatusCode::CREATED);

        let resp = app
            .get("/admin/api/v1/connections")
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;

        resp.assert_status_ok();
        let body: serde_json::Value = resp.json();
        assert_eq!(body["data"].as_array().unwrap().len(), 2);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_connection(pool: PgPool) {
        let (app, _bg) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::PlatformManager]).await;
        let auth = add_auth_headers(&user);

        let created: serde_json::Value = app
            .post("/admin/api/v1/connections")
            .json(&create_connection_body("my-conn"))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await
            .json();
        let id = created["id"].as_str().unwrap();

        let resp = app
            .get(&format!("/admin/api/v1/connections/{id}"))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;

        resp.assert_status_ok();
        let body: serde_json::Value = resp.json();
        assert_eq!(body["name"], "my-conn");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_connection(pool: PgPool) {
        let (app, _bg) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::PlatformManager]).await;
        let auth = add_auth_headers(&user);

        let created: serde_json::Value = app
            .post("/admin/api/v1/connections")
            .json(&create_connection_body("to-delete"))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await
            .json();
        let id = created["id"].as_str().unwrap();

        app.delete(&format!("/admin/api/v1/connections/{id}"))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await
            .assert_status(StatusCode::NO_CONTENT);

        // Verify gone
        app.get(&format!("/admin/api/v1/connections/{id}"))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }

    // -- Ownership isolation --

    #[sqlx::test]
    #[test_log::test]
    async fn test_connection_not_visible_to_other_user(pool: PgPool) {
        let (app, _bg) = create_test_app(pool.clone(), false).await;
        let user_a = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::PlatformManager]).await;
        let user_b = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::PlatformManager]).await;
        let auth_a = add_auth_headers(&user_a);
        let auth_b = add_auth_headers(&user_b);

        let created: serde_json::Value = app
            .post("/admin/api/v1/connections")
            .json(&create_connection_body("user-a-conn"))
            .add_header(&auth_a[0].0, &auth_a[0].1)
            .add_header(&auth_a[1].0, &auth_a[1].1)
            .await
            .json();
        let id = created["id"].as_str().unwrap();

        // User B cannot see it
        app.get(&format!("/admin/api/v1/connections/{id}"))
            .add_header(&auth_b[0].0, &auth_b[0].1)
            .add_header(&auth_b[1].0, &auth_b[1].1)
            .await
            .assert_status(StatusCode::NOT_FOUND);

        // User B list is empty
        let list: serde_json::Value = app
            .get("/admin/api/v1/connections")
            .add_header(&auth_b[0].0, &auth_b[0].1)
            .add_header(&auth_b[1].0, &auth_b[1].1)
            .await
            .json();
        assert_eq!(list["data"].as_array().unwrap().len(), 0);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_cannot_delete_other_users_connection(pool: PgPool) {
        let (app, _bg) = create_test_app(pool.clone(), false).await;
        let user_a = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::PlatformManager]).await;
        let user_b = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::PlatformManager]).await;
        let auth_a = add_auth_headers(&user_a);
        let auth_b = add_auth_headers(&user_b);

        let created: serde_json::Value = app
            .post("/admin/api/v1/connections")
            .json(&create_connection_body("private"))
            .add_header(&auth_a[0].0, &auth_a[0].1)
            .add_header(&auth_a[1].0, &auth_a[1].1)
            .await
            .json();
        let id = created["id"].as_str().unwrap();

        app.delete(&format!("/admin/api/v1/connections/{id}"))
            .add_header(&auth_b[0].0, &auth_b[0].1)
            .add_header(&auth_b[1].0, &auth_b[1].1)
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }

    // -- Sync scoping --

    #[sqlx::test]
    #[test_log::test]
    async fn test_trigger_sync(pool: PgPool) {
        let (app, _bg) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::PlatformManager]).await;
        let auth = add_auth_headers(&user);

        let conn: serde_json::Value = app
            .post("/admin/api/v1/connections")
            .json(&create_connection_body("sync-test"))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await
            .json();
        let conn_id = conn["id"].as_str().unwrap();

        let resp = app
            .post(&format!("/admin/api/v1/connections/{conn_id}/sync"))
            .json(&serde_json::json!({"strategy": "snapshot"}))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;

        resp.assert_status(StatusCode::ACCEPTED);
        let body: serde_json::Value = resp.json();
        assert_eq!(body["status"], "pending");
        assert_eq!(body["strategy"], "snapshot");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_sync_cross_connection_rejected(pool: PgPool) {
        let (app, _bg) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::PlatformManager]).await;
        let auth = add_auth_headers(&user);

        let conn_a: serde_json::Value = app
            .post("/admin/api/v1/connections")
            .json(&create_connection_body("conn-a"))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await
            .json();
        let conn_b: serde_json::Value = app
            .post("/admin/api/v1/connections")
            .json(&create_connection_body("conn-b"))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await
            .json();

        let conn_a_id = conn_a["id"].as_str().unwrap();
        let conn_b_id = conn_b["id"].as_str().unwrap();

        // Sync on conn_a
        let sync: serde_json::Value = app
            .post(&format!("/admin/api/v1/connections/{conn_a_id}/sync"))
            .json(&serde_json::json!({"strategy": "snapshot"}))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await
            .json();
        let sync_id = sync["id"].as_str().unwrap();

        // Access via conn_b URL — rejected
        app.get(&format!("/admin/api/v1/connections/{conn_b_id}/syncs/{sync_id}"))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_sync_entries_cross_connection_rejected(pool: PgPool) {
        let (app, _bg) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::PlatformManager]).await;
        let auth = add_auth_headers(&user);

        let conn_a: serde_json::Value = app
            .post("/admin/api/v1/connections")
            .json(&create_connection_body("conn-a"))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await
            .json();
        let conn_b: serde_json::Value = app
            .post("/admin/api/v1/connections")
            .json(&create_connection_body("conn-b"))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await
            .json();

        let conn_a_id = conn_a["id"].as_str().unwrap();
        let conn_b_id = conn_b["id"].as_str().unwrap();

        let sync: serde_json::Value = app
            .post(&format!("/admin/api/v1/connections/{conn_a_id}/sync"))
            .json(&serde_json::json!({"strategy": "snapshot"}))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await
            .json();
        let sync_id = sync["id"].as_str().unwrap();

        // Entries via conn_b — rejected
        app.get(&format!("/admin/api/v1/connections/{conn_b_id}/syncs/{sync_id}/entries"))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await
            .assert_status(StatusCode::NOT_FOUND);
    }

    // -- Validation --

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_connection_invalid_provider(pool: PgPool) {
        let (app, _bg) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::PlatformManager]).await;
        let auth = add_auth_headers(&user);

        app.post("/admin/api/v1/connections")
            .json(&serde_json::json!({
                "provider": "bigquery",
                "name": "bad",
                "config": {}
            }))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_trigger_sync_invalid_strategy(pool: PgPool) {
        let (app, _bg) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::PlatformManager]).await;
        let auth = add_auth_headers(&user);

        let conn: serde_json::Value = app
            .post("/admin/api/v1/connections")
            .json(&create_connection_body("strat-test"))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await
            .json();
        let conn_id = conn["id"].as_str().unwrap();

        app.post(&format!("/admin/api/v1/connections/{conn_id}/sync"))
            .json(&serde_json::json!({"strategy": "invalid"}))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_select_strategy_requires_file_keys(pool: PgPool) {
        let (app, _bg) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::PlatformManager]).await;
        let auth = add_auth_headers(&user);

        let conn: serde_json::Value = app
            .post("/admin/api/v1/connections")
            .json(&create_connection_body("select-test"))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await
            .json();
        let conn_id = conn["id"].as_str().unwrap();

        app.post(&format!("/admin/api/v1/connections/{conn_id}/sync"))
            .json(&serde_json::json!({"strategy": "select"}))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await
            .assert_status(StatusCode::BAD_REQUEST);
    }
}
