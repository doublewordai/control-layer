use crate::{
    api::models::inference_endpoints::{
        InferenceEndpointCreate, InferenceEndpointResponse, InferenceEndpointUpdate, InferenceEndpointValidate,
        InferenceEndpointValidateResponse, ListEndpointsQuery, OpenAIModelsResponse,
    },
    auth::permissions::{operation, resource, RequiresPermission},
    db::{
        handlers::{inference_endpoints::InferenceEndpointFilter, Deployments, InferenceEndpoints, Repository},
        models::inference_endpoints::{InferenceEndpointCreateDBRequest, InferenceEndpointUpdateDBRequest},
    },
    errors::{Error, Result},
    sync::{
        deployments::fetch_models::{FetchModels, FetchModelsReqwest, SyncConfig},
        endpoint_sync::{self, sync_endpoint_models_with_aliases, update_endpoint_aliases},
    },
    types::InferenceEndpointId,
    AppState,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
};
#[cfg(test)]
struct MockFetchModels;

#[cfg(test)]
use crate::api::models::inference_endpoints::OpenAIModel;

#[cfg(not(test))]
use crate::sync::deployments::fetch_models::StaticModelsFetcher;

#[cfg(test)]
#[async_trait::async_trait]
impl FetchModels for MockFetchModels {
    async fn fetch(&self) -> anyhow::Result<OpenAIModelsResponse> {
        Ok(OpenAIModelsResponse {
            object: "list".to_string(),
            data: vec![
                OpenAIModel {
                    id: "google/gemma-3-12b-it".to_string(),
                    object: "model".to_string(),
                    created: Some(0),
                    owned_by: "test".to_string(),
                },
                OpenAIModel {
                    id: "openai/gpt-4".to_string(),
                    object: "model".to_string(),
                    created: Some(0),
                    owned_by: "test".to_string(),
                },
            ],
        })
    }
}

// GET /endpoints - List endpoints
#[utoipa::path(
    get,
    path = "/endpoints",
    tag = "endpoints",
    summary = "List endpoints",
    description = "List all endpoints",
    params(
        ("skip" = Option<i64>, Query, description = "Number of endpoints to skip"),
        ("limit" = Option<i64>, Query, description = "Maximum number of endpoints to return"),
    ),
    responses(
        (status = 200, description = "List of endpoints", body = [InferenceEndpointResponse]),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn list_inference_endpoints(
    State(state): State<AppState>,
    Query(query): Query<ListEndpointsQuery>,
    _: RequiresPermission<resource::Endpoints, operation::ReadAll>, // Need at least read-own, users with ReadAll can see more
) -> Result<Json<Vec<InferenceEndpointResponse>>> {
    let mut conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = InferenceEndpoints::new(&mut conn);
    let skip = query.skip.unwrap_or(0);
    let limit = query.limit.unwrap_or(100).min(1000);

    let endpoints = repo.list(&InferenceEndpointFilter::new(skip, limit)).await?;
    Ok(Json(endpoints.into_iter().map(Into::into).collect()))
}

// GET /endpoints/:id - Get a specific endpoint
#[utoipa::path(
    get,
    path = "/endpoints/{id}",
    tag = "endpoints",
    summary = "Get endpoint",
    description = "Get a specific endpoint by ID",
    params(
        ("id" = i32, Path, description = "Endpoint ID"),
    ),
    responses(
        (status = 200, description = "Endpoint information", body = InferenceEndpointResponse),
        (status = 401, description = "Unauthorized"),
        (status = 404, description = "Endpoint not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn get_inference_endpoint(
    State(state): State<AppState>,
    Path(id): Path<InferenceEndpointId>,
    _: RequiresPermission<resource::Endpoints, operation::ReadAll>,
) -> Result<Json<InferenceEndpointResponse>> {
    let mut conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = InferenceEndpoints::new(&mut conn);
    match repo.get_by_id(id).await? {
        Some(endpoint) => Ok(Json(endpoint.into())),
        None => Err(Error::NotFound {
            resource: "Endpoint".to_string(),
            id: id.to_string(),
        }),
    }
}

// PATCH /endpoints/:id - Update endpoint (admin only)
#[utoipa::path(
    patch,
    path = "/endpoints/{id}",
    tag = "endpoints",
    summary = "Update endpoint",
    description = "Update an existing endpoint (admin only)",
    params(
        ("id" = i32, Path, description = "Endpoint ID to update"),
    ),
    responses(
        (status = 200, description = "Endpoint updated successfully", body = InferenceEndpointResponse),
        (status = 400, description = "Bad request - invalid endpoint data"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 404, description = "Endpoint not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn update_inference_endpoint(
    State(state): State<AppState>,
    Path(id): Path<InferenceEndpointId>,
    _: RequiresPermission<resource::Endpoints, operation::UpdateAll>,
    Json(update): Json<InferenceEndpointUpdate>,
) -> Result<Json<InferenceEndpointResponse>> {
    // Use a transaction if alias mapping is being updated
    if update.alias_mapping.is_some() {
        let mut tx = state.db.begin().await.map_err(|e| Error::Database(e.into()))?;
        let mut repo = InferenceEndpoints::new(&mut tx);
        let db_request = InferenceEndpointUpdateDBRequest {
            name: update.name,
            description: update.description,
            url: match update.url {
                Some(url_str) => Some(url_str.parse().map_err(|_| Error::BadRequest {
                    message: "Invalid URL format".to_string(),
                })?),
                None => None,
            },
            api_key: update.api_key,
            model_filter: update.model_filter.clone(),
            auth_header_name: update.auth_header_name.clone(),
            auth_header_prefix: update.auth_header_prefix.clone(),
        };

        let endpoint = repo.update(id, &db_request).await?;

        // Update aliases for existing deployments
        let alias_mapping = update.alias_mapping.unwrap(); // Safe because we checked above
        let mut deployments_repo = Deployments::new(&mut tx);
        match update_endpoint_aliases(endpoint.clone(), &mut deployments_repo, &alias_mapping).await {
            Ok(sync_result) => {
                tracing::info!(
                    "Updated aliases for endpoint {}: {} changes made",
                    endpoint.id,
                    sync_result.changes_made
                );
            }
            Err(sync_error) => {
                tracing::error!("Failed to update aliases for endpoint {}: {}", endpoint.id, sync_error);
                // Convert SyncError to our Error type
                let converted_error = match sync_error {
                    crate::sync::endpoint_sync::SyncError::AliasConflicts { conflicts } => Error::Conflict {
                        message: "Alias conflicts detected during endpoint update".to_string(),
                        conflicts: Some(conflicts),
                    },
                    crate::sync::endpoint_sync::SyncError::Other(_) => Error::Internal {
                        operation: "update endpoint aliases".to_string(),
                    },
                };
                tx.rollback().await.map_err(|e| Error::Database(e.into()))?;
                return Err(converted_error);
            }
        }
        tx.commit().await.map_err(|e| Error::Database(e.into()))?;
        Ok(Json(endpoint.into()))
    } else {
        // Simple update without alias mapping
        let mut conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
        let mut repo = InferenceEndpoints::new(&mut conn);
        let db_request = InferenceEndpointUpdateDBRequest {
            name: update.name,
            description: update.description,
            url: match update.url {
                Some(url_str) => Some(url_str.parse().map_err(|_| Error::BadRequest {
                    message: "Invalid URL format".to_string(),
                })?),
                None => None,
            },
            api_key: update.api_key,
            model_filter: update.model_filter,
            auth_header_name: update.auth_header_name,
            auth_header_prefix: update.auth_header_prefix,
        };

        let endpoint = repo.update(id, &db_request).await?;

        // Perform background sync after successful update
        match endpoint_sync::synchronize_endpoint(endpoint.id, state.db.clone()).await {
            Ok(sync_result) => {
                tracing::info!(
                    "Auto-sync after endpoint {} update: {} changes made",
                    endpoint.id,
                    sync_result.changes_made
                );
            }
            Err(e) => {
                tracing::warn!("Auto-sync failed after endpoint {} update: {}", endpoint.id, e);
                // Continue anyway - update succeeded even if sync failed
            }
        }
        Ok(Json(endpoint.into()))
    }
}

// POST /endpoints/validate - Validate endpoint connection
#[utoipa::path(
    post,
    path = "/endpoints/validate",
    tag = "endpoints",
    summary = "Validate endpoint",
    description = "Test connection to an endpoint and retrieve available models",
    request_body = InferenceEndpointValidate,
    responses(
        (status = 200, description = "Validation response", body = InferenceEndpointValidateResponse),
        (status = 400, description = "Bad request - invalid URL or data"),
        (status = 401, description = "Unauthorized"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn validate_inference_endpoint(
    State(state): State<AppState>,
    _: RequiresPermission<resource::Endpoints, operation::UpdateAll>,
    Json(validate_request): Json<InferenceEndpointValidate>,
) -> Result<Json<InferenceEndpointValidateResponse>> {
    let (url, api_key, auth_header_name, auth_header_prefix) = match validate_request {
        InferenceEndpointValidate::New {
            url,
            api_key,
            auth_header_name,
            auth_header_prefix,
        } => {
            let parsed_url = url.parse::<url::Url>().map_err(|_| Error::BadRequest {
                message: "Invalid URL format".to_string(),
            })?;
            (parsed_url, api_key, auth_header_name, auth_header_prefix)
        }
        InferenceEndpointValidate::Existing { endpoint_id } => {
            let mut conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
            let mut endpoints_repo = InferenceEndpoints::new(&mut conn);
            let endpoint = endpoints_repo.get_by_id(endpoint_id).await?;
            let endpoint = endpoint.ok_or_else(|| Error::NotFound {
                resource: "Endpoint".to_string(),
                id: endpoint_id.to_string(),
            })?;
            (
                endpoint.url,
                endpoint.api_key,
                Some(endpoint.auth_header_name),
                Some(endpoint.auth_header_prefix),
            )
        }
    };

    tracing::info!(
        "Validating endpoint: url={}, has_api_key={}, auth_header_name={:?}, auth_header_prefix={:?}",
        url,
        api_key.is_some(),
        auth_header_name,
        auth_header_prefix
    );

    let models = validate_endpoint_connection(&url, api_key.as_deref(), auth_header_name, auth_header_prefix).await?;
    Ok(Json(InferenceEndpointValidateResponse {
        status: "success".to_string(),
        models: Some(models),
        error: None,
    }))
}

// POST /endpoints - Create new endpoint (admin only)
#[utoipa::path(
    post,
    path = "/endpoints",
    tag = "endpoints",
    summary = "Create endpoint",
    description = "Create a new inference endpoint (admin only)",
    request_body = InferenceEndpointCreate,
    responses(
        (status = 201, description = "Endpoint created successfully", body = InferenceEndpointResponse),
        (status = 400, description = "Bad request - invalid endpoint data"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn create_inference_endpoint(
    State(state): State<AppState>,
    current_user: RequiresPermission<resource::Endpoints, operation::CreateAll>,
    Json(create_request): Json<InferenceEndpointCreate>,
) -> Result<(StatusCode, Json<InferenceEndpointResponse>)> {
    let url = create_request.url.parse().map_err(|_| Error::BadRequest {
        message: "Invalid URL format".to_string(),
    })?;

    // Start transaction for atomic endpoint creation + sync
    let mut tx = state.db.begin().await.map_err(|e| Error::Database(e.into()))?;

    // Create the endpoint within the transaction
    let mut repo = InferenceEndpoints::new(&mut tx);
    let db_request = InferenceEndpointCreateDBRequest {
        created_by: current_user.id,
        name: create_request.name,
        description: create_request.description,
        url,
        api_key: create_request.api_key,
        model_filter: create_request.model_filter.clone(),
        auth_header_name: create_request.auth_header_name,
        auth_header_prefix: create_request.auth_header_prefix,
    };

    let endpoint = repo.create(&db_request).await?;

    // Optionally sync models during creation
    if create_request.sync {
        let mut deployments_repo = Deployments::new(&mut tx);

        // Choose fetcher and sync based on skip_fetch flag
        #[cfg(test)]
        let sync_result = {
            let fetcher = MockFetchModels;
            sync_endpoint_models_with_aliases(endpoint.clone(), &mut deployments_repo, fetcher, &create_request.alias_mapping).await
        };

        #[cfg(not(test))]
        let sync_result = if create_request.skip_fetch {
            // Use static model list from model_filter
            let model_names = create_request.model_filter.clone().unwrap_or_default();
            if model_names.is_empty() {
                tracing::warn!("skip_fetch is true but model_filter is empty for endpoint {}", endpoint.id);
            }
            tracing::info!("Using static model list for endpoint {}: {:?}", endpoint.id, model_names);
            let fetcher = StaticModelsFetcher::new(model_names);
            sync_endpoint_models_with_aliases(endpoint.clone(), &mut deployments_repo, fetcher, &create_request.alias_mapping).await
        } else {
            // Fetch models from endpoint
            let fetcher = FetchModelsReqwest::new(SyncConfig::from_endpoint(&endpoint));
            sync_endpoint_models_with_aliases(endpoint.clone(), &mut deployments_repo, fetcher, &create_request.alias_mapping).await
        };

        match sync_result {
            Ok(result) => {
                tracing::info!("Sync succeeded: {:?}", result);
            }
            Err(sync_error) => {
                tracing::error!("Sync failed with error: {:?}", sync_error);
                match sync_error {
                    crate::sync::endpoint_sync::SyncError::AliasConflicts { conflicts } => {
                        return Err(crate::errors::Error::Conflict {
                            message: "Alias conflicts detected during endpoint creation".to_string(),
                            conflicts: Some(conflicts),
                        });
                    }
                    crate::sync::endpoint_sync::SyncError::Other(e) => {
                        tracing::error!("Other sync error: {:#}", e);
                        return Err(crate::errors::Error::Internal {
                            operation: "sync endpoint models during creation".to_string(),
                        });
                    }
                }
            }
        }
    } else {
        tracing::info!("Skipped sync during endpoint {} creation (sync=false)", endpoint.id);
    }

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;
    Ok((StatusCode::CREATED, Json(endpoint.into())))
}

// DELETE /endpoints/:id - Delete endpoint (admin only)
#[utoipa::path(
    delete,
    path = "/endpoints/{id}",
    tag = "endpoints",
    summary = "Delete endpoint",
    description = "Delete an existing endpoint (admin only)",
    params(
        ("id" = i32, Path, description = "Endpoint ID to delete"),
    ),
    responses(
        (status = 204, description = "Endpoint deleted successfully"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 404, description = "Endpoint not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn delete_inference_endpoint(
    State(state): State<AppState>,
    Path(id): Path<InferenceEndpointId>,
    _: RequiresPermission<resource::Endpoints, operation::DeleteAll>,
) -> Result<StatusCode> {
    let mut conn = state.db.acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = InferenceEndpoints::new(&mut conn);
    if repo.delete(id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(Error::NotFound {
            resource: "Endpoint".to_string(),
            id: id.to_string(),
        })
    }
}

// Helper: Validate endpoint connection and fetch models
async fn validate_endpoint_connection(
    url: &url::Url,
    api_key: Option<&str>,
    auth_header_name: Option<String>,
    auth_header_prefix: Option<String>,
) -> Result<OpenAIModelsResponse> {
    use std::time::Duration;

    let auth_header_name = auth_header_name.unwrap_or_else(|| "Authorization".to_string());
    let auth_header_prefix = auth_header_prefix.unwrap_or_else(|| "Bearer ".to_string());

    tracing::debug!(
        "Creating SyncConfig for validation: url={}, auth_header_name={}, auth_header_prefix={:?}, has_api_key={}",
        url,
        auth_header_name,
        auth_header_prefix,
        api_key.is_some()
    );

    let sync_config = SyncConfig {
        openai_api_key: api_key.map(|s| s.to_string()),
        openai_base_url: url.clone(),
        auth_header_name,
        auth_header_prefix,
        request_timeout: Duration::from_secs(10),
    };

    // Use the existing FetchModelsReqwest implementation
    let fetcher = FetchModelsReqwest::new(sync_config);

    tracing::debug!("Fetching models from endpoint...");
    let models_response = fetcher.fetch().await.map_err(|e| {
        tracing::error!("Failed to fetch models: {:#}", e);
        e
    })?;

    tracing::debug!(
        "Received models response: object={}, model_count={}",
        models_response.object,
        models_response.data.len()
    );

    if models_response.object != "list" {
        return Err(Error::BadRequest {
            message: "Invalid response format - expected 'list' object".to_string(),
        });
    }
    if models_response.data.is_empty() {
        return Err(Error::BadRequest {
            message: "No models found at this endpoint".to_string(),
        });
    }

    // The OpenAIModelsResponse is already in the right format
    Ok(models_response)
}

// POST /endpoints/:id/synchronize - Synchronize endpoint deployments (admin only)
#[utoipa::path(
    post,
    path = "/endpoints/{id}/synchronize",
    tag = "endpoints",
    summary = "Synchronize endpoint deployments",
    description = "Trigger synchronization of deployments for a specific endpoint (admin only)",
    params(
        ("id" = i32, Path, description = "Endpoint ID to synchronize"),
    ),
    responses(
        (status = 200, description = "Synchronization completed", body = crate::sync::endpoint_sync::EndpointSyncResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - admin access required"),
        (status = 404, description = "Endpoint not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("X-Doubleword-User" = [])
    )
)]
pub async fn synchronize_endpoint(
    State(state): State<AppState>,
    Path(id): Path<InferenceEndpointId>,
    _: RequiresPermission<resource::Endpoints, operation::UpdateAll>,
) -> Result<Json<endpoint_sync::EndpointSyncResponse>> {
    // Perform synchronization
    let response = endpoint_sync::synchronize_endpoint(id, state.db).await?;

    tracing::info!("Successfully synchronized endpoint {} with {} changes", id, response.changes_made);
    Ok(Json(response))
}

#[cfg(test)]
mod tests {
    use crate::api::models::deployments::DeployedModelResponse;
    use crate::api::models::inference_endpoints::InferenceEndpointResponse;
    use crate::api::models::users::Role;
    use crate::test_utils::*;
    use serde_json::json;
    use sqlx::PgPool;

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_inference_endpoints(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        let response = app
            .get("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let endpoints: Vec<InferenceEndpointResponse> = response.json();
        // Should have at least the default endpoint
        assert!(!endpoints.is_empty());
        assert!(endpoints.iter().any(|e| e.name == "test"));
    }

    // Helper function to get the test endpoint ID
    async fn get_test_endpoint_id(
        app: &axum_test::TestServer,
        user: &crate::api::models::users::UserResponse,
    ) -> crate::types::InferenceEndpointId {
        let response = app
            .get("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(user).0, add_auth_headers(user).1)
            .await;

        response.assert_status_ok();
        let endpoints: Vec<InferenceEndpointResponse> = response.json();
        endpoints.iter().find(|e| e.name == "test").expect("Test endpoint should exist").id
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_inference_endpoint(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let test_endpoint_id = get_test_endpoint_id(&app, &user).await;

        let response = app
            .get(&format!("/admin/api/v1/endpoints/{test_endpoint_id}"))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let endpoint: InferenceEndpointResponse = response.json();
        assert_eq!(endpoint.id, test_endpoint_id);
        assert_eq!(endpoint.name, "test");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_nonexistent_inference_endpoint(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let non_existent_id = uuid::Uuid::new_v4();

        let response = app
            .get(&format!("/admin/api/v1/endpoints/{non_existent_id}"))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_not_found();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_inference_endpoint_as_admin(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let test_endpoint_id = get_test_endpoint_id(&app, &admin_user).await;

        let update = json!({
            "name": "Updated Default"
        });

        let response = app
            .patch(&format!("/admin/api/v1/endpoints/{test_endpoint_id}"))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&update)
            .await;

        response.assert_status_ok();
        let updated_endpoint: InferenceEndpointResponse = response.json();
        assert_eq!(updated_endpoint.id, test_endpoint_id);
        assert_eq!(updated_endpoint.name, "Updated Default");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_inference_endpoint_as_non_admin_forbidden(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let test_endpoint_id = get_test_endpoint_id(&app, &user).await;

        let update = json!({
            "name": "Should Not Work"
        });

        let response = app
            .patch(&format!("/admin/api/v1/endpoints/{test_endpoint_id}"))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .json(&update)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_nonexistent_endpoint(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let non_existent_id = uuid::Uuid::new_v4();

        let update = json!({
            "name": "Should Not Work"
        });

        let response = app
            .patch(&format!("/admin/api/v1/endpoints/{non_existent_id}"))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&update)
            .await;

        response.assert_status_not_found(); // Repository propagates NotFound error
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_endpoint_with_empty_payload(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let test_endpoint_id = get_test_endpoint_id(&app, &admin_user).await;

        let update = json!({});

        let response = app
            .patch(&format!("/admin/api/v1/endpoints/{test_endpoint_id}"))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&update)
            .await;

        response.assert_status_ok();
        let endpoint: InferenceEndpointResponse = response.json();
        assert_eq!(endpoint.name, "test"); // Name should remain unchanged
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_endpoint_with_null_name(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let test_endpoint_id = get_test_endpoint_id(&app, &admin_user).await;

        let update = json!({
            "name": null
        });

        let response = app
            .patch(&format!("/admin/api/v1/endpoints/{test_endpoint_id}"))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&update)
            .await;

        response.assert_status_ok();
        let endpoint: InferenceEndpointResponse = response.json();
        assert_eq!(endpoint.name, "test"); // Name should remain unchanged when null
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_default_endpoint_exists(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // First get the list to find the test endpoint
        let response = app
            .get("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let endpoints: Vec<InferenceEndpointResponse> = response.json();

        // Find the test endpoint
        let test_endpoint = endpoints.iter().find(|e| e.name == "test").expect("Test endpoint should exist");
        let test_endpoint_id = test_endpoint.id;

        // Get endpoint by ID directly
        let response = app
            .get(&format!("/admin/api/v1/endpoints/{test_endpoint_id}"))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let endpoint: InferenceEndpointResponse = response.json();
        assert_eq!(endpoint.id, test_endpoint_id);
        assert_eq!(endpoint.name, "test");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_endpoints_with_pagination(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Test with limit
        let response = app
            .get("/admin/api/v1/endpoints?limit=10")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let endpoints: Vec<InferenceEndpointResponse> = response.json();
        assert!(!endpoints.is_empty());

        // Test with skip and limit
        let response = app
            .get("/admin/api/v1/endpoints?skip=0&limit=5")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let endpoints: Vec<InferenceEndpointResponse> = response.json();
        assert!(!endpoints.is_empty());

        // Test skip beyond available endpoints
        let response = app
            .get("/admin/api/v1/endpoints?skip=1000&limit=10")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_ok();
        let endpoints: Vec<InferenceEndpointResponse> = response.json();
        assert!(endpoints.is_empty());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_validate_inference_endpoint_new_valid_url(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        let validate_request = json!({
            "type": "new",
            "url": "https://api.openai.com/v1",
            "api_key": "test-key"
        });

        let response = app
            .post("/admin/api/v1/endpoints/validate")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&validate_request)
            .await;

        // This will likely fail due to network/auth, but we test the handler logic
        // The important thing is that it doesn't return 400 for valid URL format
        assert!(response.status_code() != axum::http::StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_validate_inference_endpoint_new_invalid_url(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        let validate_request = json!({
            "type": "new",
            "url": "not-a-valid-url",
            "api_key": "test-key"
        });

        let response = app
            .post("/admin/api/v1/endpoints/validate")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&validate_request)
            .await;

        response.assert_status(axum::http::StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_validate_inference_endpoint_existing_endpoint(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let test_endpoint_id = get_test_endpoint_id(&app, &admin_user).await;

        let validate_request = json!({
            "type": "existing",
            "endpoint_id": test_endpoint_id
        });

        let response = app
            .post("/admin/api/v1/endpoints/validate")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&validate_request)
            .await;

        // This will likely fail due to network/auth, but we test the handler logic
        assert!(response.status_code() != axum::http::StatusCode::BAD_REQUEST);
        assert!(response.status_code() != axum::http::StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_validate_inference_endpoint_nonexistent_endpoint(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let fake_endpoint_id = uuid::Uuid::new_v4();

        let validate_request = json!({
            "type": "existing",
            "endpoint_id": fake_endpoint_id
        });

        let response = app
            .post("/admin/api/v1/endpoints/validate")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&validate_request)
            .await;

        response.assert_status_not_found();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_validate_inference_endpoint_as_non_admin_forbidden(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        let validate_request = json!({
            "type": "new",
            "url": "https://api.openai.com/v1",
            "api_key": "test-key"
        });

        let response = app
            .post("/admin/api/v1/endpoints/validate")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .json(&validate_request)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_inference_endpoint_as_admin(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        let create_request = json!({
            "name": "New Test Endpoint",
            "description": "A new endpoint for testing",
            "url": "https://api.newtest.com/v1",
            "api_key": "new-test-key",
            "model_filter": ["gpt-4", "gpt-3.5-turbo"]
        });

        let response = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&create_request)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);
        let endpoint: InferenceEndpointResponse = response.json();
        assert_eq!(endpoint.name, "New Test Endpoint");
        assert_eq!(endpoint.description, Some("A new endpoint for testing".to_string()));
        assert_eq!(endpoint.url, "https://api.newtest.com/v1");
        assert!(endpoint.requires_api_key);
        assert_eq!(endpoint.model_filter, Some(vec!["gpt-4".to_string(), "gpt-3.5-turbo".to_string()]));
        assert_eq!(endpoint.created_by, admin_user.id);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_inference_endpoint_minimal_fields(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        let create_request = json!({
            "name": "Minimal Endpoint",
            "url": "https://api.minimal.com/v1"
        });

        let response = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&create_request)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);
        let endpoint: InferenceEndpointResponse = response.json();
        assert_eq!(endpoint.name, "Minimal Endpoint");
        assert_eq!(endpoint.description, None);
        assert_eq!(endpoint.url, "https://api.minimal.com/v1");
        assert!(!endpoint.requires_api_key);
        assert_eq!(endpoint.model_filter, None);
        assert_eq!(endpoint.created_by, admin_user.id);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_inference_endpoint_invalid_url(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        let create_request = json!({
            "name": "Invalid URL Endpoint",
            "url": "not-a-valid-url"
        });

        let response = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&create_request)
            .await;

        response.assert_status(axum::http::StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_inference_endpoint_as_non_admin_forbidden(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        let create_request = json!({
            "name": "Should Not Work",
            "url": "https://api.forbidden.com/v1"
        });

        let response = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .json(&create_request)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_inference_endpoint_as_admin(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        // First create an endpoint to delete
        let create_request = json!({
            "name": "Endpoint to Delete",
            "url": "https://api.todelete.com/v1"
        });

        let create_response = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&create_request)
            .await;

        create_response.assert_status(axum::http::StatusCode::CREATED);
        let created_endpoint: InferenceEndpointResponse = create_response.json();

        // Now delete it
        let delete_response = app
            .delete(&format!("/admin/api/v1/endpoints/{}", created_endpoint.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        delete_response.assert_status(axum::http::StatusCode::NO_CONTENT);

        // Verify it's deleted by trying to get it
        let get_response = app
            .get(&format!("/admin/api/v1/endpoints/{}", created_endpoint.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        get_response.assert_status_not_found();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_nonexistent_inference_endpoint(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let fake_endpoint_id = uuid::Uuid::new_v4();

        let response = app
            .delete(&format!("/admin/api/v1/endpoints/{fake_endpoint_id}"))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_not_found();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_inference_endpoint_as_non_admin_forbidden(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        // Create an endpoint as admin
        let create_request = json!({
            "name": "Admin Endpoint",
            "url": "https://api.admin.com/v1"
        });

        let create_response = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&create_request)
            .await;

        create_response.assert_status(axum::http::StatusCode::CREATED);
        let created_endpoint: InferenceEndpointResponse = create_response.json();

        // Try to delete as non-admin
        let delete_response = app
            .delete(&format!("/admin/api/v1/endpoints/{}", created_endpoint.id))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        delete_response.assert_status_forbidden();

        // Verify it still exists
        let get_response = app
            .get(&format!("/admin/api/v1/endpoints/{}", created_endpoint.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        get_response.assert_status_ok();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_synchronize_endpoint_as_admin(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let test_endpoint_id = get_test_endpoint_id(&app, &admin_user).await;

        let response = app
            .post(&format!("/admin/api/v1/endpoints/{test_endpoint_id}/synchronize"))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        // The sync might fail due to network issues, but we test the handler structure
        // It should not return forbidden or bad request for valid endpoint ID
        assert!(response.status_code() != axum::http::StatusCode::FORBIDDEN);
        assert!(response.status_code() != axum::http::StatusCode::BAD_REQUEST);
        assert!(response.status_code() != axum::http::StatusCode::NOT_FOUND);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_synchronize_nonexistent_endpoint(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let fake_endpoint_id = uuid::Uuid::new_v4();

        let response = app
            .post(&format!("/admin/api/v1/endpoints/{fake_endpoint_id}/synchronize"))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        // Should return an error status when trying to sync non-existent endpoint
        assert!(response.status_code() != axum::http::StatusCode::OK);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_synchronize_endpoint_as_non_admin_forbidden(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let test_endpoint_id = get_test_endpoint_id(&app, &user).await;

        let response = app
            .post(&format!("/admin/api/v1/endpoints/{test_endpoint_id}/synchronize"))
            .add_header(add_auth_headers(&user).0, add_auth_headers(&user).1)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_standard_user_can_read_endpoints_only(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let standard_user = create_test_user(&pool, Role::StandardUser).await;

        // StandardUser should be able to list endpoints (has ReadAll for Endpoints)
        let response = app
            .get("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .await;

        response.assert_status_ok();

        // StandardUser should be able to get specific endpoint
        let test_endpoint_id = get_test_endpoint_id(&app, &standard_user).await;
        let response = app
            .get(&format!("/admin/api/v1/endpoints/{test_endpoint_id}"))
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .await;

        response.assert_status_ok();

        // StandardUser should NOT be able to create endpoints
        let create_request = json!({
            "name": "Standard User Endpoint",
            "url": "https://api.standarduser.com/v1"
        });

        let response = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .json(&create_request)
            .await;

        response.assert_status_forbidden();

        // StandardUser should NOT be able to update endpoints
        let update = json!({"name": "Updated by Standard User"});
        let response = app
            .patch(&format!("/admin/api/v1/endpoints/{test_endpoint_id}"))
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .json(&update)
            .await;

        response.assert_status_forbidden();

        // StandardUser should NOT be able to delete endpoints
        let response = app
            .delete(&format!("/admin/api/v1/endpoints/{test_endpoint_id}"))
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .await;

        response.assert_status_forbidden();

        // StandardUser should NOT be able to validate endpoints
        let validate_request = json!({
            "type": "new",
            "url": "https://api.test.com/v1"
        });

        let response = app
            .post("/admin/api/v1/endpoints/validate")
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .json(&validate_request)
            .await;

        response.assert_status_forbidden();

        // StandardUser should NOT be able to synchronize endpoints
        let response = app
            .post(&format!("/admin/api/v1/endpoints/{test_endpoint_id}/synchronize"))
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_request_viewer_can_read_endpoints_only(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let request_viewer = create_test_user(&pool, Role::RequestViewer).await;

        // RequestViewer should NOT be able to list endpoints (no Endpoints permissions)
        let response = app
            .get("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .await;

        response.assert_status_forbidden();

        // RequestViewer should NOT be able to get specific endpoint
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let test_endpoint_id = get_test_endpoint_id(&app, &admin_user).await;

        let response = app
            .get(&format!("/admin/api/v1/endpoints/{test_endpoint_id}"))
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .await;

        response.assert_status_forbidden();

        // RequestViewer should NOT be able to create endpoints
        let create_request = json!({
            "name": "Request Viewer Endpoint",
            "url": "https://api.requestviewer.com/v1"
        });

        let response = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .json(&create_request)
            .await;

        response.assert_status_forbidden();

        // RequestViewer should NOT be able to validate, update, delete, or sync endpoints
        let validate_request = json!({
            "type": "new",
            "url": "https://api.test.com/v1"
        });

        let response = app
            .post("/admin/api/v1/endpoints/validate")
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .json(&validate_request)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_multi_role_user_endpoint_permissions(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;

        // User with StandardUser + RequestViewer should be able to read endpoints (from StandardUser)
        let multi_role_user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::RequestViewer]).await;

        let response = app
            .get("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&multi_role_user).0, add_auth_headers(&multi_role_user).1)
            .await;

        response.assert_status_ok();

        // But should NOT be able to modify endpoints (needs PlatformManager)
        let create_request = json!({
            "name": "Multi Role Endpoint",
            "url": "https://api.multirole.com/v1"
        });

        let response = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&multi_role_user).0, add_auth_headers(&multi_role_user).1)
            .json(&create_request)
            .await;

        response.assert_status_forbidden();

        // User with PlatformManager + StandardUser should have full access
        let platform_user = create_test_user_with_roles(&pool, vec![Role::PlatformManager, Role::StandardUser]).await;

        let response = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&platform_user).0, add_auth_headers(&platform_user).1)
            .json(&create_request)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);
        let created_endpoint: InferenceEndpointResponse = response.json();

        // Should be able to update
        let update = json!({"name": "Updated Platform Endpoint"});
        let response = app
            .patch(&format!("/admin/api/v1/endpoints/{}", created_endpoint.id))
            .add_header(add_auth_headers(&platform_user).0, add_auth_headers(&platform_user).1)
            .json(&update)
            .await;

        response.assert_status_ok();

        // Should be able to validate
        let validate_request = json!({
            "type": "existing",
            "endpoint_id": created_endpoint.id
        });

        let response = app
            .post("/admin/api/v1/endpoints/validate")
            .add_header(add_auth_headers(&platform_user).0, add_auth_headers(&platform_user).1)
            .json(&validate_request)
            .await;

        // Might fail due to network, but should not be forbidden
        assert!(response.status_code() != axum::http::StatusCode::FORBIDDEN);

        // Should be able to synchronize
        let response = app
            .post(&format!("/admin/api/v1/endpoints/{}/synchronize", created_endpoint.id))
            .add_header(add_auth_headers(&platform_user).0, add_auth_headers(&platform_user).1)
            .await;

        // Might fail due to network, but should not be forbidden
        assert!(response.status_code() != axum::http::StatusCode::FORBIDDEN);

        // Should be able to delete
        let response = app
            .delete(&format!("/admin/api/v1/endpoints/{}", created_endpoint.id))
            .add_header(add_auth_headers(&platform_user).0, add_auth_headers(&platform_user).1)
            .await;

        response.assert_status(axum::http::StatusCode::NO_CONTENT);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_endpoint_crud_permission_isolation(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let platform_manager1 = create_test_admin_user(&pool, Role::PlatformManager).await;
        let platform_manager2 = create_test_admin_user(&pool, Role::PlatformManager).await;
        let standard_user = create_test_user(&pool, Role::StandardUser).await;

        // Platform Manager 1 creates an endpoint
        let create_request = json!({
            "name": "PM1 Endpoint",
            "url": "https://api.pm1.com/v1",
            "description": "Created by Platform Manager 1"
        });

        let response = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&platform_manager1).0, add_auth_headers(&platform_manager1).1)
            .json(&create_request)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);
        let endpoint: InferenceEndpointResponse = response.json();
        assert_eq!(endpoint.created_by, platform_manager1.id);

        // Platform Manager 2 should be able to modify it (admin permissions are global)
        let update = json!({"name": "Updated by PM2"});
        let response = app
            .patch(&format!("/admin/api/v1/endpoints/{}", endpoint.id))
            .add_header(add_auth_headers(&platform_manager2).0, add_auth_headers(&platform_manager2).1)
            .json(&update)
            .await;

        response.assert_status_ok();

        // Standard User should only be able to read it
        let response = app
            .get(&format!("/admin/api/v1/endpoints/{}", endpoint.id))
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .await;

        response.assert_status_ok();
        let read_endpoint: InferenceEndpointResponse = response.json();
        assert_eq!(read_endpoint.name, "Updated by PM2");

        // Standard User should NOT be able to delete it
        let response = app
            .delete(&format!("/admin/api/v1/endpoints/{}", endpoint.id))
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .await;

        response.assert_status_forbidden();

        // Platform Manager 2 should be able to delete it
        let response = app
            .delete(&format!("/admin/api/v1/endpoints/{}", endpoint.id))
            .add_header(add_auth_headers(&platform_manager2).0, add_auth_headers(&platform_manager2).1)
            .await;

        response.assert_status(axum::http::StatusCode::NO_CONTENT);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_validation_permission_requirements(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_admin_user(&pool, Role::PlatformManager).await;
        let standard_user = create_test_user(&pool, Role::StandardUser).await;
        let request_viewer = create_test_user(&pool, Role::RequestViewer).await;

        // Create an endpoint for testing existing validation
        let create_request = json!({
            "name": "Validation Test Endpoint",
            "url": "https://api.validation.com/v1",
            "api_key": "test-key"
        });

        let response = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .json(&create_request)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);
        let endpoint: InferenceEndpointResponse = response.json();

        // Test new endpoint validation permissions
        let validate_new = json!({
            "type": "new",
            "url": "https://api.test.com/v1",
            "api_key": "test-key"
        });

        // Only PlatformManager should be able to validate
        let response = app
            .post("/admin/api/v1/endpoints/validate")
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .json(&validate_new)
            .await;

        assert!(response.status_code() != axum::http::StatusCode::FORBIDDEN);

        // StandardUser should be forbidden
        let response = app
            .post("/admin/api/v1/endpoints/validate")
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .json(&validate_new)
            .await;

        response.assert_status_forbidden();

        // RequestViewer should be forbidden
        let response = app
            .post("/admin/api/v1/endpoints/validate")
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .json(&validate_new)
            .await;

        response.assert_status_forbidden();

        // Test existing endpoint validation
        let validate_existing = json!({
            "type": "existing",
            "endpoint_id": endpoint.id
        });

        // Only PlatformManager should be able to validate existing endpoints
        let response = app
            .post("/admin/api/v1/endpoints/validate")
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .json(&validate_existing)
            .await;

        assert!(response.status_code() != axum::http::StatusCode::FORBIDDEN);

        // Others should be forbidden
        let response = app
            .post("/admin/api/v1/endpoints/validate")
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .json(&validate_existing)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_synchronization_permission_requirements(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_admin_user(&pool, Role::PlatformManager).await;
        let standard_user = create_test_user(&pool, Role::StandardUser).await;
        let request_viewer = create_test_user(&pool, Role::RequestViewer).await;
        let multi_role_user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::RequestViewer]).await;

        let test_endpoint_id = get_test_endpoint_id(&app, &platform_manager).await;

        // Only PlatformManager should be able to synchronize
        let response = app
            .post(&format!("/admin/api/v1/endpoints/{test_endpoint_id}/synchronize"))
            .add_header(add_auth_headers(&platform_manager).0, add_auth_headers(&platform_manager).1)
            .await;

        assert!(response.status_code() != axum::http::StatusCode::FORBIDDEN);

        // StandardUser should be forbidden
        let response = app
            .post(&format!("/admin/api/v1/endpoints/{test_endpoint_id}/synchronize"))
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .await;

        response.assert_status_forbidden();

        // RequestViewer should be forbidden
        let response = app
            .post(&format!("/admin/api/v1/endpoints/{test_endpoint_id}/synchronize"))
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .await;

        response.assert_status_forbidden();

        // Multi-role user without PlatformManager should be forbidden
        let response = app
            .post(&format!("/admin/api/v1/endpoints/{test_endpoint_id}/synchronize"))
            .add_header(add_auth_headers(&multi_role_user).0, add_auth_headers(&multi_role_user).1)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_standard_user_endpoint_access(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let standard_user = create_test_user(&pool, Role::StandardUser).await;

        // Standard user should be able to read endpoints
        let response = app
            .get("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .await;
        response.assert_status_ok();

        // Standard user should NOT be able to create endpoints
        let create_request = json!({
            "name": "Test Create Permission",
            "url": "https://api.test.com/v1"
        });
        let response = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&standard_user).0, add_auth_headers(&standard_user).1)
            .json(&create_request)
            .await;
        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_request_viewer_endpoint_access(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let request_viewer = create_test_user(&pool, Role::RequestViewer).await;

        // Request viewer should NOT be able to read endpoints
        let response = app
            .get("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .await;
        response.assert_status_forbidden();

        // Request viewer should NOT be able to create endpoints
        let create_request = json!({
            "name": "Test Create Permission",
            "url": "https://api.test.com/v1"
        });
        let response = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&request_viewer).0, add_auth_headers(&request_viewer).1)
            .json(&create_request)
            .await;
        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_multi_role_user_endpoint_access(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let multi_role_user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::RequestViewer]).await;

        // Multi-role user should be able to read (StandardUser permission)
        let response = app
            .get("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&multi_role_user).0, add_auth_headers(&multi_role_user).1)
            .await;
        response.assert_status_ok();

        // Multi-role user should NOT create (no PlatformManager role)
        let create_request = json!({
            "name": "Test Create Permission",
            "url": "https://api.test.com/v1"
        });
        let response = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&multi_role_user).0, add_auth_headers(&multi_role_user).1)
            .json(&create_request)
            .await;
        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_endpoint_access(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let platform_user = create_test_user_with_roles(&pool, vec![Role::PlatformManager, Role::StandardUser]).await;

        // Platform user should be able to read
        let response = app
            .get("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&platform_user).0, add_auth_headers(&platform_user).1)
            .await;
        response.assert_status_ok();

        // Platform user should be able to create
        let create_request = json!({
            "name": "Test Create Permission",
            "url": "https://api.test.com/v1",
            "sync": false
        });
        let response = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&platform_user).0, add_auth_headers(&platform_user).1)
            .json(&create_request)
            .await;
        response.assert_status(axum::http::StatusCode::CREATED);

        // Clean up the endpoint we created
        let endpoint: InferenceEndpointResponse = response.json();
        app.delete(&format!("/admin/api/v1/endpoints/{}", endpoint.id))
            .add_header(add_auth_headers(&platform_user).0, add_auth_headers(&platform_user).1)
            .await
            .assert_status(axum::http::StatusCode::NO_CONTENT);
    }

    // Alias mapping dedupe/uniqueness tests

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_endpoint_with_unique_aliases(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        let create_request = json!({
            "name": "Alias Endpoint",
            "url": "https://api.alias.com/v1",
            "sync": true,
            "alias_mapping": {
                "google/gemma-3-12b-it": "gen-model",
                "openai/gpt-4": "gpt4-prod"
            }
        });

        let response = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&create_request)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);
        let endpoint: InferenceEndpointResponse = response.json();
        assert_eq!(endpoint.name, "Alias Endpoint");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_endpoint_with_duplicate_alias_conflict(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        // First endpoint with alias "shared-alias"
        let create_request1 = json!({
            "name": "Endpoint 1",
            "url": "https://api.dup1.com/v1",
            "sync": true,
            "alias_mapping": {
                "google/gemma-3-12b-it": "shared-alias"
            }
        });
        let resp1 = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&create_request1)
            .await;
        resp1.assert_status(axum::http::StatusCode::CREATED);

        // Second endpoint tries to use same alias
        let create_request2 = json!({
            "name": "Endpoint 2",
            "url": "https://api.dup2.com/v1",
            "sync": true,
            "alias_mapping": {
                "openai/gpt-4": "shared-alias"
            }
        });
        let resp2 = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&create_request2)
            .await;
        resp2.assert_status(axum::http::StatusCode::CONFLICT);
        let body: serde_json::Value = resp2.json();
        assert!(body["message"].as_str().unwrap().contains("Alias conflicts"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_endpoint_with_alias_conflict(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        // Create two endpoints with unique aliases
        let create1 = json!({
            "name": "Endpoint A",
            "url": "https://api.a.com/v1",
            "sync": true,
            "alias_mapping": { "google/gemma-3-12b-it": "alias-a" }
        });
        let resp1 = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&create1)
            .await;
        resp1.assert_status(axum::http::StatusCode::CREATED);
        let _endpoint_a: InferenceEndpointResponse = resp1.json();

        let create2 = json!({
            "name": "Endpoint B",
            "url": "https://api.b.com/v1",
            "sync": true,
            "alias_mapping": { "openai/gpt-4": "alias-b" }
        });
        let resp2 = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&create2)
            .await;
        resp2.assert_status(axum::http::StatusCode::CREATED);
        let endpoint_b: InferenceEndpointResponse = resp2.json();

        // Try to update endpoint B to use alias-a (should conflict)
        let update = json!({
            "alias_mapping": { "openai/gpt-4": "alias-a" }
        });
        let resp_update = app
            .patch(&format!("/admin/api/v1/endpoints/{}", endpoint_b.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&update)
            .await;
        resp_update.assert_status(axum::http::StatusCode::CONFLICT);
        let body: serde_json::Value = resp_update.json();
        assert!(body["message"].as_str().unwrap().contains("Alias conflicts"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_endpoint_with_unique_aliases(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        // Create endpoint with one alias
        let create = json!({
            "name": "Endpoint C",
            "url": "https://api.c.com/v1",
            "sync": true,
            "alias_mapping": { "google/gemma-3-12b-it": "alias-c" }
        });
        let resp = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&create)
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let endpoint: InferenceEndpointResponse = resp.json();

        // Update alias to a new unique value
        let update = json!({
            "alias_mapping": { "google/gemma-3-12b-it": "alias-c2" }
        });
        let resp_update = app
            .patch(&format!("/admin/api/v1/endpoints/{}", endpoint.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&update)
            .await;
        resp_update.assert_status(axum::http::StatusCode::OK);
        let updated: InferenceEndpointResponse = resp_update.json();
        assert_eq!(updated.name, "Endpoint C");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_alias_uniqueness_is_global(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        // Create endpoint with alias "global-alias"
        let create1 = json!({
            "name": "Endpoint Global 1",
            "url": "https://api.global1.com/v1",
            "sync": true,
            "alias_mapping": { "google/gemma-3-12b-it": "global-alias" }
        });
        let resp1 = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&create1)
            .await;
        resp1.assert_status(axum::http::StatusCode::CREATED);

        // Try to create another endpoint with same alias for a different model
        let create2 = json!({
            "name": "Endpoint Global 2",
            "url": "https://api.global2.com/v1",
            "sync": true,
            "alias_mapping": { "google/gemma-3-12b-it": "global-alias" }
        });
        let resp2 = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&create2)
            .await;
        resp2.assert_status(axum::http::StatusCode::CONFLICT);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_alias_can_match_model_name_if_unique(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        // Alias matches model name, but is unique
        let create = json!({
            "name": "Endpoint Self Alias",
            "url": "https://api.selfalias.com/v1",
            "sync": true,
            "alias_mapping": { "google/gemma-3-12b-it": "google/gemma-3-12b-it" }
        });
        let resp = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&create)
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_alias_update_noop_is_ok(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        // Create endpoint with alias
        let create = json!({
            "name": "Endpoint Noop",
            "url": "https://api.noop.com/v1",
            "sync": true,
            "alias_mapping": { "google/gemma-3-12b-it": "noop-alias" }
        });
        let resp = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&create)
            .await;
        resp.assert_status(axum::http::StatusCode::CREATED);
        let endpoint: InferenceEndpointResponse = resp.json();

        // Update with same alias mapping (should be a no-op, but succeed)
        let update = json!({
            "alias_mapping": { "google/gemma-3-12b-it": "noop-alias" }
        });
        let resp_update = app
            .patch(&format!("/admin/api/v1/endpoints/{}", endpoint.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&update)
            .await;
        resp_update.assert_status(axum::http::StatusCode::OK);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_multiple_same_model_different_endpoints_unique_aliases(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        // Endpoint 1 with gemma model, alias "alias-1"
        let create1 = json!({
            "name": "Endpoint 1",
            "url": "https://api.1.com/v1",
            "sync": true,
            "alias_mapping": {
                "google/gemma-3-12b-it": "alias-1",
                "openai/gpt-4": "alias-1b"
            }
        });
        let resp1 = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&create1)
            .await;
        resp1.assert_status(axum::http::StatusCode::CREATED);

        // Endpoint 2 with gemma model, alias "alias-2"
        let create2 = json!({
            "name": "Endpoint 2",
            "url": "https://api.2.com/v1",
            "sync": true,
            "alias_mapping": {
                "google/gemma-3-12b-it": "alias-2",
                "openai/gpt-4": "alias-2b"
            }
        });
        let resp2 = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&create2)
            .await;
        resp2.assert_status(axum::http::StatusCode::CREATED);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_default_alias_conflict_when_no_alias_mapping(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        // First endpoint with gemma model, default alias (model name)
        let create1 = json!({
            "name": "Endpoint 1",
            "url": "https://api.1.com/v1",
            "sync": true
            // no alias_mapping, so alias defaults to model name
        });
        let resp1 = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&create1)
            .await;
        resp1.assert_status(axum::http::StatusCode::CREATED);

        // Second endpoint, also no alias_mapping, so alias defaults to model name and should conflict
        let create2 = json!({
            "name": "Endpoint 2",
            "url": "https://api.2.com/v1",
            "sync": true
            // no alias_mapping
        });
        let resp2 = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&create2)
            .await;
        resp2.assert_status(axum::http::StatusCode::CONFLICT);
        let body: serde_json::Value = resp2.json();
        assert!(body["message"].as_str().unwrap().contains("Alias conflicts"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_endpoint_name_conflict_bounces_before_alias_conflict(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        // First endpoint with name "Endpoint X"
        let create1 = json!({
            "name": "Endpoint X",
            "url": "https://api.x.com/v1",
            "sync": true,
            "alias_mapping": { "google/gemma-3-12b-it": "unique-alias-x" }
        });
        let resp1 = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&create1)
            .await;
        resp1.assert_status(axum::http::StatusCode::CREATED);

        // Second endpoint with same name, but different alias (should fail on name, not alias)
        let create2 = json!({
            "name": "Endpoint X", // same name
            "url": "https://api.y.com/v1",
            "sync": true,
            "alias_mapping": { "google/gemma-3-12b-it": "unique-alias-y" }
        });
        let resp2 = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&create2)
            .await;
        resp2.assert_status(axum::http::StatusCode::CONFLICT);
        let body: serde_json::Value = resp2.json();
        assert!(body["message"].as_str().unwrap().contains("already exists"));
        // Should not mention alias conflicts
        if let Some(conflicts) = body.get("conflicts") {
            assert!(conflicts.is_null() || conflicts.as_array().unwrap().is_empty());
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_endpoint_no_alias_mapping_defaults_to_model_name(pool: PgPool) {
        let (app, _) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;

        let create_request = json!({
            "name": "Default Alias Endpoint",
            "url": "https://api.defaultalias.com/v1",
            "sync": true
            // no alias_mapping
        });

        let response = app
            .post("/admin/api/v1/endpoints")
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .json(&create_request)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);
        let endpoint: InferenceEndpointResponse = response.json();

        // Now fetch deployments for this endpoint (you may need to add an API or DB helper for this)
        // For demonstration, let's assume you have a helper:
        let response = app
            .get(&format!("/admin/api/v1/models?endpoint={}", endpoint.id))
            .add_header(add_auth_headers(&admin_user).0, add_auth_headers(&admin_user).1)
            .await;

        response.assert_status_ok();
        let deployments: Vec<DeployedModelResponse> = response.json();
        assert!(deployments.iter().any(|d| d.alias == "google/gemma-3-12b-it"));
        assert!(deployments.iter().any(|d| d.alias == "openai/gpt-4"));
    }
}
