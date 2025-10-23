use crate::api::models::inference_endpoints::OpenAIModel;
use crate::db::handlers::deployments::DeploymentFilter;
use crate::db::handlers::repository::Repository;
use crate::db::handlers::{Deployments, InferenceEndpoints};
use crate::db::models::deployments::{DeploymentCreateDBRequest, DeploymentDBResponse, DeploymentUpdateDBRequest, ModelStatus};
use crate::db::models::inference_endpoints::InferenceEndpointDBResponse;
use crate::errors::AliasConflict;
use crate::sync::deployments::fetch_models::{FetchModels, FetchModelsReqwest, SyncConfig};
use crate::types::{DeploymentId, InferenceEndpointId, UserId};
use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::HashMap;
use std::collections::HashSet;
use tracing::{debug, instrument, warn};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("Alias conflicts detected")]
    AliasConflicts { conflicts: Vec<crate::errors::AliasConflict> },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct EndpointSyncResponse {
    /// Endpoint that was synchronized
    #[schema(value_type = String, format = "uuid")]
    pub endpoint_id: InferenceEndpointId,
    /// Number of changes made during sync
    pub changes_made: usize,
    /// Number of new models created
    pub new_models_created: usize,
    /// Number of models reactivated
    pub models_reactivated: usize,
    /// Number of models deactivated
    pub models_deactivated: usize,
    /// Number of models deleted (filtered out)
    pub models_deleted: usize,
    /// Total number of models fetched from endpoint
    pub total_models_fetched: usize,
    /// Number of models after applying filter
    pub filtered_models_count: usize,
    /// Sync timestamp
    pub synced_at: chrono::DateTime<chrono::Utc>,
}

/// Synchronize deployments for a specific inference endpoint
#[instrument]
pub async fn synchronize_endpoint(endpoint_id: InferenceEndpointId, pool: PgPool) -> Result<EndpointSyncResponse> {
    let mut tx = pool.begin().await?;
    let endpoint_info;
    // Automatically synchronize the endpoint after creating
    {
        let mut endpoints_repo = InferenceEndpoints::new(&mut tx);

        // Get endpoint info
        endpoint_info = endpoints_repo
            .get_by_id(endpoint_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Endpoint not found: {}", endpoint_id))?;
    }

    // Create sync config from endpoint
    let sync_config = SyncConfig::from_endpoint(&endpoint_info);

    // Create fetcher
    let fetcher = FetchModelsReqwest::new(sync_config);

    // Perform the sync
    let sync_result;
    {
        let mut deployments_repo = Deployments::new(&mut tx);
        sync_result = sync_endpoint_models(endpoint_info, &mut deployments_repo, fetcher).await
    }

    tx.commit()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to commit sync transaction: {}", e))?;
    sync_result
}

/// Synchronizes models for an endpoint by fetching and comparing with existing deployments
#[instrument(skip(deployments_repo, fetch_models))]
pub async fn sync_endpoint_models<D, F>(
    endpoint_info: InferenceEndpointDBResponse,
    deployments_repo: &mut D,
    fetch_models: F,
) -> Result<EndpointSyncResponse>
where
    D: Repository<
        CreateRequest = DeploymentCreateDBRequest,
        UpdateRequest = DeploymentUpdateDBRequest,
        Response = DeploymentDBResponse,
        Id = DeploymentId,
        Filter = DeploymentFilter,
    >,
    F: FetchModels,
{
    // Get fetched + existing models
    let fetched_models = fetch_models.fetch().await?;
    let existing_models = get_existing_models(deployments_repo, endpoint_info.id).await?;

    // Names are used for deduplication - i.e. we can't have Qwen/Qwen3-VL twice in the same endpoint.
    let existing_model_names: HashSet<String> = existing_models.iter().map(|m| m.model_name.clone()).collect();
    let fetched_model_names: HashSet<String> = fetched_models.data.iter().map(|m| m.id.clone()).collect();

    let mut changes_made = 0;
    let mut new_models_created = 0;
    let mut models_reactivated = 0;
    let mut models_deactivated = 0;
    let mut models_deleted = 0;
    let sync_time = Utc::now();

    // Filter models based on endpoint's model_filter if specified
    let models_to_sync: Vec<_> = if let Some(model_filter) = &endpoint_info.model_filter {
        // Only sync models that are in the filter
        fetched_models
            .data
            .iter()
            .filter(|model| model_filter.contains(&model.id))
            .collect()
    } else {
        // No filter specified, sync all models
        fetched_models.data.iter().collect()
    };

    debug!(
        "Endpoint {} model filter: {:?}, syncing {} of {} fetched models",
        endpoint_info.name,
        endpoint_info.model_filter,
        models_to_sync.len(),
        fetched_models.data.len()
    );

    // Use system user ID (nil UUID) for creating deployments
    let system_user_id = Uuid::nil();

    // Create new models that don't exist yet.
    for model in &models_to_sync {
        if !existing_model_names.contains(&model.id) {
            match create_deployment(deployments_repo, model, &endpoint_info, system_user_id).await {
                Ok(_) => {
                    debug!("Created new deployment for model: {}", model.id);
                    new_models_created += 1;
                    changes_made += 1;
                }
                Err(e) => {
                    warn!("Failed to create deployment for model {}: {}", model.id, e);
                }
            }
        }
    }

    // Update status for existing models using proper status transitions
    for existing_model in &existing_models {
        let existing_model_present = fetched_model_names.contains(&existing_model.model_name);

        // Skip deleted models entirely - respect user deletions
        if existing_model.deleted {
            continue;
        }

        // Check if model should be filtered out based on endpoint's model_filter
        // If there's a filter and this model isn't in it, delete it
        if let Some(model_filter) = &endpoint_info.model_filter {
            if !model_filter.contains(&existing_model.model_name) {
                if let Err(e) = deployments_repo.delete(existing_model.id).await {
                    warn!("Failed to delete filtered-out model {}: {}", existing_model.model_name, e);
                } else {
                    debug!("Deleted model {} (removed from filter)", existing_model.model_name);
                    models_deleted += 1;
                    changes_made += 1;
                }
                continue;
            }
        }

        // Now handle models that are in the filter
        match (&existing_model.status, existing_model_present) {
            // Model is active and present in API - just update sync time
            (ModelStatus::Active, true) => {
                let update = DeploymentUpdateDBRequest::status_update(None, sync_time);
                if let Err(e) = deployments_repo.update(existing_model.id, &update).await {
                    warn!("Failed to update sync time for active model {}: {}", existing_model.model_name, e);
                }
            }

            // Model is inactive but now present in API - reactivate it
            (ModelStatus::Inactive, true) => {
                let update = DeploymentUpdateDBRequest::status_update(Some(ModelStatus::Active), sync_time);
                if let Err(e) = deployments_repo.update(existing_model.id, &update).await {
                    warn!("Failed to reactivate model {}: {}", existing_model.model_name, e);
                } else {
                    debug!("Reactivated model {} (returned to API)", existing_model.model_name);
                    models_reactivated += 1;
                    changes_made += 1;
                }
            }

            // Model is active but missing from API - mark inactive
            (ModelStatus::Active, false) => {
                let update = DeploymentUpdateDBRequest::status_update(Some(ModelStatus::Inactive), sync_time);
                if let Err(e) = deployments_repo.update(existing_model.id, &update).await {
                    warn!("Failed to deactivate model {}: {}", existing_model.model_name, e);
                } else {
                    debug!("Deactivated model {} (missing from API)", existing_model.model_name);
                    models_deactivated += 1;
                    changes_made += 1;
                }
            }

            // Model is inactive and still missing from API - update sync time
            (ModelStatus::Inactive, false) => {
                let update = DeploymentUpdateDBRequest::status_update(None, sync_time);
                if let Err(e) = deployments_repo.update(existing_model.id, &update).await {
                    warn!("Failed to update sync time for inactive model {}: {}", existing_model.model_name, e);
                }
            }
        }
    }

    debug!(
        "Sync completed: {} new models created, {} reactivated, {} deactivated, {} deleted, {} total changes",
        new_models_created, models_reactivated, models_deactivated, models_deleted, changes_made
    );

    Ok(EndpointSyncResponse {
        endpoint_id: endpoint_info.id,
        changes_made,
        new_models_created,
        models_reactivated,
        models_deactivated,
        models_deleted,
        total_models_fetched: fetched_models.data.len(),
        filtered_models_count: models_to_sync.len(),
        synced_at: sync_time,
    })
}

/// Sync function for endpoint creation with optional alias mapping
#[instrument(skip(deployments_repo, fetch_models))]
pub async fn sync_endpoint_models_with_aliases<D, F>(
    endpoint_info: InferenceEndpointDBResponse,
    deployments_repo: &mut D,
    fetch_models: F,
    alias_mapping: &Option<HashMap<String, String>>,
) -> std::result::Result<EndpointSyncResponse, SyncError>
where
    D: Repository<
        CreateRequest = DeploymentCreateDBRequest,
        UpdateRequest = DeploymentUpdateDBRequest,
        Response = DeploymentDBResponse,
        Id = DeploymentId,
        Filter = DeploymentFilter,
    >,
    F: FetchModels,
{
    // Get fetched models
    let fetched_models = fetch_models.fetch().await?;
    let existing_models = get_existing_models(deployments_repo, endpoint_info.id)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to get existing models: {}", e))?;

    let existing_model_names: HashSet<String> = existing_models.iter().map(|m| m.model_name.clone()).collect();
    let mut changes_made = 0;
    let mut new_models_created = 0;
    let sync_time = Utc::now();

    // Filter models based on endpoint's model_filter if specified
    let models_to_sync: Vec<_> = if let Some(model_filter) = &endpoint_info.model_filter {
        fetched_models
            .data
            .iter()
            .filter(|model| model_filter.contains(&model.id))
            .collect()
    } else {
        fetched_models.data.iter().collect()
    };

    debug!(
        "Endpoint {} syncing {} of {} fetched models with alias mapping: {:?}",
        endpoint_info.name,
        models_to_sync.len(),
        fetched_models.data.len(),
        alias_mapping
    );

    // Use system user ID for creating deployments
    let system_user_id = Uuid::nil();

    // Create new models with custom aliases if provided
    for model in &models_to_sync {
        if !existing_model_names.contains(&model.id) {
            // Determine the alias for this model
            let alias = alias_mapping
                .as_ref()
                .and_then(|mapping| mapping.get(&model.id))
                .cloned()
                .unwrap_or_else(|| model.id.clone());

            match create_deployment_with_alias(deployments_repo, model, &endpoint_info, system_user_id, alias.clone()).await {
                Ok(_) => {
                    debug!("Created new deployment for model: {} with alias: {}", model.id, alias);
                    new_models_created += 1;
                    changes_made += 1;
                }
                Err(e) => {
                    // Check if this is an alias conflict and extract structured info
                    if let Some(conflict) = extract_alias_conflict_from_error(&e, &model.id, &alias) {
                        warn!("Alias conflict for model '{}' with alias '{}': {}", model.id, alias, e);
                        // Return immediately with just this one conflict
                        return Err(SyncError::AliasConflicts { conflicts: vec![conflict] });
                    } else {
                        // For non-conflict errors, fail fast
                        return Err(SyncError::Other(e));
                    }
                }
            }
        }
    }

    // Check for duplicate aliases in the alias mapping (or default aliases)
    let mut seen_aliases = std::collections::HashMap::new();
    let mut conflicts = Vec::new();
    for model in &models_to_sync {
        let alias = alias_mapping
            .as_ref()
            .and_then(|mapping| mapping.get(&model.id))
            .cloned()
            .unwrap_or_else(|| model.id.clone());
        if let Some(existing_model) = seen_aliases.insert(alias.clone(), model.id.clone()) {
            // Found a duplicate alias in this batch
            conflicts.push(AliasConflict {
                model_name: model.id.clone(),
                attempted_alias: alias.clone(),
            });
            conflicts.push(AliasConflict {
                model_name: existing_model,
                attempted_alias: alias.clone(),
            });
        }
    }
    if !conflicts.is_empty() {
        return Err(SyncError::AliasConflicts { conflicts });
    }

    debug!(
        "Sync completed: {} new models created, {} total changes",
        new_models_created, changes_made
    );

    Ok(EndpointSyncResponse {
        endpoint_id: endpoint_info.id,
        changes_made,
        new_models_created,
        models_reactivated: 0,
        models_deactivated: 0,
        models_deleted: 0,
        total_models_fetched: fetched_models.data.len(),
        filtered_models_count: models_to_sync.len(),
        synced_at: sync_time,
    })
}

/// Extract alias conflict information from database error - specific to deployment creation
fn extract_alias_conflict_from_error(
    error: &anyhow::Error,
    model_name: &str,
    attempted_alias: &str,
) -> Option<crate::errors::AliasConflict> {
    if let Some(crate::db::errors::DbError::UniqueViolation {
        constraint,
        conflicting_value,
        ..
    }) = error.downcast_ref::<crate::db::errors::DbError>()
    {
        if constraint.as_deref() == Some("deployed_models_alias_unique") {
            return Some(crate::errors::AliasConflict {
                model_name: model_name.to_string(),
                attempted_alias: conflicting_value.clone().unwrap_or_else(|| attempted_alias.to_string()),
            });
        }
    }
    None
}

/// Create deployment with custom alias - now returns structured error info
async fn create_deployment_with_alias<D>(
    deployments_repo: &mut D,
    model: &OpenAIModel,
    endpoint_info: &InferenceEndpointDBResponse,
    created_by: UserId,
    alias: String,
) -> Result<()>
where
    D: Repository<CreateRequest = DeploymentCreateDBRequest, Response = DeploymentDBResponse>,
{
    let db_request = DeploymentCreateDBRequest::builder()
        .created_by(created_by)
        .model_name(model.id.clone())
        .alias(alias.clone())
        .hosted_on(endpoint_info.id)
        .build();

    match deployments_repo.create(&db_request).await {
        Ok(_) => {
            debug!(
                "Created deployment for model: {} with alias: {} on endpoint: {}",
                model.id, alias, endpoint_info.name
            );
            Ok(())
        }
        Err(e) => {
            // Convert DbError to anyhow::Error so we can extract conflict info later
            Err(anyhow::Error::from(e))
        }
    }
}

// Update the get_existing_models function to handle the Result conversion
async fn get_existing_models<D>(
    deployments_repo: &mut D,
    endpoint_id: InferenceEndpointId,
) -> crate::db::errors::Result<Vec<DeploymentDBResponse>>
where
    D: Repository<Response = DeploymentDBResponse, Id = DeploymentId, Filter = DeploymentFilter>,
{
    // Fetch all models for this endpoint, including soft-deleted ones for sync purposes
    let filter = DeploymentFilter::new(0, i64::MAX).with_endpoint(endpoint_id);
    deployments_repo.list(&filter).await
}

async fn create_deployment<D>(
    deployments_repo: &mut D,
    model: &OpenAIModel,
    endpoint_info: &InferenceEndpointDBResponse,
    created_by: UserId,
) -> Result<()>
where
    D: Repository<CreateRequest = DeploymentCreateDBRequest, Response = DeploymentDBResponse>,
{
    let db_request = DeploymentCreateDBRequest::builder()
        .created_by(created_by)
        .model_name(model.id.clone())
        .alias(model.id.clone()) // Use model ID as alias by default
        .hosted_on(endpoint_info.id)
        .build();

    deployments_repo.create(&db_request).await?;
    Ok(())
}

/// Update deployment aliases and create/remove deployments based on model filter
pub async fn update_endpoint_aliases<D>(
    endpoint: InferenceEndpointDBResponse,
    deployments_repo: &mut D,
    alias_mapping: &HashMap<String, String>,
) -> Result<EndpointSyncResponse, SyncError>
where
    D: Repository<
        CreateRequest = DeploymentCreateDBRequest,
        UpdateRequest = DeploymentUpdateDBRequest,
        Response = DeploymentDBResponse,
        Id = DeploymentId,
        Filter = DeploymentFilter,
    >,
{
    let mut changes_made = 0;
    let mut new_models_created = 0;
    let mut models_deleted = 0;

    // Get current deployments for this endpoint
    let current_deployments = deployments_repo
        .list(&DeploymentFilter::new(0, 1000).with_endpoint(endpoint.id))
        .await
        .map_err(|e| SyncError::Other(e.into()))?;

    // Build a map of existing deployments by model_name for quick lookup
    let existing_deployments_map: HashMap<String, &DeploymentDBResponse> =
        current_deployments.iter().map(|d| (d.model_name.clone(), d)).collect();

    // Get the models that should be deployed based on the endpoint's model_filter
    let models_to_deploy: HashSet<String> = if let Some(model_filter) = &endpoint.model_filter {
        model_filter.iter().cloned().collect()
    } else {
        // If no filter, we should keep all existing models and not create new ones
        // This is different from initial sync where we might want to sync all available models
        existing_deployments_map.keys().cloned().collect()
    };

    // 1. Update aliases for existing deployments that should remain
    for deployment in &current_deployments {
        if models_to_deploy.contains(&deployment.model_name) && !deployment.deleted {
            if let Some(new_alias) = alias_mapping.get(&deployment.model_name) {
                let trimmed_alias = new_alias.trim();
                if deployment.alias != trimmed_alias {
                    // Get fresh conflict check data for this update
                    let conflict_check = deployments_repo
                        .list(&DeploymentFilter::new(0, 1000))
                        .await
                        .map_err(|e| SyncError::Other(e.into()))?;

                    // Check for alias conflicts before updating (excluding this deployment)
                    if conflict_check.iter().any(|d| d.alias == *trimmed_alias && d.id != deployment.id) {
                        return Err(SyncError::AliasConflicts {
                            conflicts: vec![AliasConflict {
                                model_name: deployment.model_name.clone(),
                                attempted_alias: trimmed_alias.to_string(),
                            }],
                        });
                    }

                    // Update the deployment alias
                    let update_request = DeploymentUpdateDBRequest::alias_update(trimmed_alias.to_string());

                    deployments_repo
                        .update(deployment.id, &update_request)
                        .await
                        .map_err(|e| SyncError::Other(e.into()))?;

                    changes_made += 1;
                    tracing::info!(
                        "Updated deployment {} alias from '{}' to '{}'",
                        deployment.id,
                        deployment.alias,
                        trimmed_alias
                    );
                }
            }
        }
    }

    // 2. Create new deployments for models in the filter that don't exist yet
    let system_user_id = uuid::Uuid::nil();

    for model_name in &models_to_deploy {
        if !existing_deployments_map.contains_key(model_name) {
            // This model needs to be deployed
            let alias = alias_mapping
                .get(model_name)
                .cloned()
                .unwrap_or_else(|| model_name.clone())
                .trim() // ADD THIS LINE - always trim the alias
                .to_string();

            // Check for alias conflicts before creating
            let conflict_check = deployments_repo
                .list(&DeploymentFilter::new(0, 1000))
                .await
                .map_err(|e| SyncError::Other(e.into()))?;

            if conflict_check.iter().any(|d| d.alias == alias) {
                return Err(SyncError::AliasConflicts {
                    conflicts: vec![AliasConflict {
                        model_name: model_name.clone(),
                        attempted_alias: alias,
                    }],
                });
            }

            // Create the new deployment
            let db_request = DeploymentCreateDBRequest::builder()
                .created_by(system_user_id)
                .model_name(model_name.clone())
                .alias(alias.clone())
                .hosted_on(endpoint.id)
                .build();

            match deployments_repo.create(&db_request).await {
                Ok(_) => {
                    new_models_created += 1;
                    changes_made += 1;
                    tracing::info!(
                        "Created new deployment for model '{}' with alias '{}' on endpoint {}",
                        model_name,
                        alias,
                        endpoint.id
                    );
                }
                Err(e) => {
                    tracing::error!("Failed to create deployment for model '{}': {}", model_name, e);
                    return Err(SyncError::Other(e.into()));
                }
            }
        }
    }

    // 3. Remove deployments for models that are no longer in the filter
    // Only do this if we have a model_filter (if None, keep all existing)
    if endpoint.model_filter.is_some() {
        for deployment in &current_deployments {
            if !models_to_deploy.contains(&deployment.model_name) && !deployment.deleted {
                // This deployment should be removed
                match deployments_repo.delete(deployment.id).await {
                    Ok(true) => {
                        models_deleted += 1;
                        changes_made += 1;
                        tracing::info!("Deleted deployment for model '{}' (removed from filter)", deployment.model_name);
                    }
                    Ok(false) => {
                        tracing::warn!("Deployment {} not found for deletion", deployment.id);
                    }
                    Err(e) => {
                        tracing::error!("Failed to delete deployment {}: {}", deployment.id, e);
                        return Err(SyncError::Other(e.into()));
                    }
                }
            }
        }
    }

    Ok(EndpointSyncResponse {
        endpoint_id: endpoint.id,
        changes_made,
        new_models_created,
        models_reactivated: 0,
        models_deactivated: 0,
        models_deleted,
        total_models_fetched: models_to_deploy.len(),
        filtered_models_count: models_to_deploy.len(),
        synced_at: Utc::now(),
    })
}

#[cfg(test)]
mod tests {
    use crate::{
        api::models::inference_endpoints::{OpenAIModel, OpenAIModelsResponse},
        db::{
            errors::Result,
            handlers::{deployments::DeploymentFilter, InferenceEndpoints, Repository},
            models::{
                deployments::{DeploymentCreateDBRequest, DeploymentDBResponse, DeploymentUpdateDBRequest, ModelStatus},
                inference_endpoints::InferenceEndpointDBResponse,
            },
        },
        sync::{
            deployments::fetch_models::FetchModels, endpoint_sync::sync_endpoint_models, endpoint_sync::sync_endpoint_models_with_aliases,
        },
        DeploymentId, UserId,
    };
    use anyhow::anyhow;
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use tokio::sync::RwLock;

    #[derive(Debug, Clone)]
    struct MockDeployment {
        id: DeploymentId,
        model_name: String,
        alias: String,
        created_by: UserId,
        status: ModelStatus,
        last_sync: Option<DateTime<Utc>>,
    }

    impl From<MockDeployment> for DeploymentDBResponse {
        fn from(mock: MockDeployment) -> Self {
            DeploymentDBResponse {
                id: mock.id,
                model_name: mock.model_name,
                alias: mock.alias,
                created_by: mock.created_by,
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
                model_type: None,
                capabilities: None,
                description: None,
                hosted_on: InferenceEndpoints::default_endpoint_id(),
                status: mock.status,
                last_sync: mock.last_sync,
                deleted: false,
                requests_per_second: None,
                burst_size: None,
                pricing: None,
            }
        }
    }

    #[derive(Debug, Clone)]
    struct MockDeploymentsRepo {
        deployments: Arc<RwLock<HashMap<DeploymentId, MockDeployment>>>,
    }

    impl MockDeploymentsRepo {
        fn new() -> Self {
            Self {
                deployments: Arc::new(RwLock::new(HashMap::new())),
            }
        }

        async fn add_deployment(&self, model_name: String, alias: String) -> DeploymentId {
            let id = uuid::Uuid::new_v4();
            let deployment = MockDeployment {
                id,
                model_name,
                alias,
                created_by: uuid::Uuid::nil(),
                status: ModelStatus::Active,
                last_sync: None,
            };
            self.deployments.write().await.insert(id, deployment);
            id
        }

        fn mock_coalesce(request: DeploymentUpdateDBRequest, mut response: DeploymentDBResponse) -> DeploymentDBResponse {
            if let Some(model_name) = &request.model_name {
                response.model_name = model_name.clone();
            }
            if let Some(alias) = &request.alias {
                response.alias = alias.clone();
            }
            if let Some(description) = &request.description {
                response.description = description.clone();
            }
            if let Some(model_type) = &request.model_type {
                response.model_type = model_type.clone();
            }
            if let Some(capabilities) = &request.capabilities {
                response.capabilities = capabilities.clone();
            }
            if let Some(status) = &request.status {
                response.status = status.clone();
            }
            if let Some(last_sync) = &request.last_sync {
                response.last_sync = *last_sync;
            }
            if let Some(deleted) = &request.deleted {
                response.deleted = *deleted;
            }
            response.updated_at = chrono::Utc::now();
            response
        }
    }

    #[async_trait]
    impl Repository for MockDeploymentsRepo {
        type CreateRequest = DeploymentCreateDBRequest;
        type UpdateRequest = DeploymentUpdateDBRequest;
        type Response = DeploymentDBResponse;
        type Id = DeploymentId;
        type Filter = DeploymentFilter;

        async fn create(&mut self, request: &Self::CreateRequest) -> Result<Self::Response> {
            let deployments = self.deployments.read().await;
            if let Some(_existing) = deployments.values().find(|d| d.alias == request.alias) {
                return Err(crate::db::errors::DbError::UniqueViolation {
                    constraint: Some("deployed_models_alias_unique".to_string()),
                    table: Some("deployed_models".to_string()),
                    message: format!("Alias '{}' already exists", request.alias),
                    conflicting_value: Some(request.alias.clone()),
                });
            }
            drop(deployments);

            let id = uuid::Uuid::new_v4();
            let deployment = MockDeployment {
                id,
                model_name: request.model_name.clone(),
                alias: request.alias.clone(),
                created_by: request.created_by,
                status: ModelStatus::Active,
                last_sync: None,
            };
            let response = DeploymentDBResponse::from(deployment.clone());
            self.deployments.write().await.insert(id, deployment);
            Ok(response)
        }

        async fn get_by_id(&mut self, id: Self::Id) -> Result<Option<Self::Response>> {
            let deployments = self.deployments.read().await;
            Ok(deployments.get(&id).map(|d| DeploymentDBResponse::from(d.clone())))
        }

        async fn get_bulk(&mut self, ids: Vec<Self::Id>) -> Result<HashMap<Self::Id, Self::Response>> {
            let deployments = self.deployments.read().await;
            let mut result = HashMap::new();
            for id in ids {
                if let Some(deployment) = deployments.get(&id) {
                    result.insert(id, DeploymentDBResponse::from(deployment.clone()));
                }
            }
            Ok(result)
        }

        async fn delete(&mut self, id: Self::Id) -> Result<bool> {
            let mut deployments = self.deployments.write().await;
            Ok(deployments.remove(&id).is_some())
        }

        async fn update(&mut self, id: Self::Id, request: &Self::UpdateRequest) -> Result<Self::Response> {
            let mut deployments = self.deployments.write().await;

            if let Some(deployment) = deployments.get(&id) {
                let current_response = DeploymentDBResponse::from(deployment.clone());
                let updated_response = Self::mock_coalesce(request.clone(), current_response);

                let updated_deployment = MockDeployment {
                    id: deployment.id,
                    model_name: updated_response.model_name.clone(),
                    alias: updated_response.alias.clone(),
                    created_by: deployment.created_by,
                    status: updated_response.status.clone(),
                    last_sync: updated_response.last_sync,
                };

                deployments.insert(id, updated_deployment);
                Ok(updated_response)
            } else {
                Err(anyhow::anyhow!("Deployment not found").into())
            }
        }

        async fn list(&mut self, filter: &Self::Filter) -> Result<Vec<Self::Response>> {
            let deployments = self.deployments.read().await;
            let mut results: Vec<DeploymentDBResponse> = deployments.values().map(|d| DeploymentDBResponse::from(d.clone())).collect();

            // Apply endpoint filter if specified
            if let Some(endpoint_id) = filter.endpoint_id {
                results.retain(|d| d.hosted_on == endpoint_id);
            }

            // Apply status filter
            if let Some(statuses) = &filter.statuses {
                results.retain(|d| statuses.contains(&d.status));
            }

            // Apply pagination
            let start = filter.skip as usize;
            let end = if filter.limit == i64::MAX {
                results.len()
            } else {
                (start + filter.limit as usize).min(results.len())
            };

            if start < results.len() {
                results = results[start..end].to_vec();
            } else {
                results = vec![];
            }

            Ok(results)
        }
    }

    fn create_test_endpoint() -> InferenceEndpointDBResponse {
        InferenceEndpointDBResponse {
            id: InferenceEndpoints::default_endpoint_id(),
            name: "Test Endpoint".to_string(),
            description: Some("Test endpoint for unit tests".to_string()),
            url: "http://localhost:8080".parse().unwrap(),
            api_key: Some("test-api-key".to_string()),
            model_filter: None, // No filter by default - sync all models
            created_by: uuid::Uuid::nil(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[derive(Debug, Clone)]
    struct MockFetchModels {
        models: Arc<Mutex<Option<OpenAIModelsResponse>>>,
        error: Arc<Mutex<Option<String>>>,
    }

    impl MockFetchModels {
        fn new() -> Self {
            Self {
                models: Arc::new(Mutex::new(None)),
                error: Arc::new(Mutex::new(None)),
            }
        }

        fn set_models(&self, models: Vec<OpenAIModel>) {
            let response = OpenAIModelsResponse {
                object: "list".to_string(),
                data: models,
            };
            *self.models.lock().unwrap() = Some(response);
        }
    }

    #[async_trait]
    impl FetchModels for MockFetchModels {
        async fn fetch(&self) -> anyhow::Result<OpenAIModelsResponse> {
            if let Some(error) = self.error.lock().unwrap().as_ref() {
                return Err(anyhow!(error.clone()));
            }

            self.models.lock().unwrap().clone().ok_or_else(|| anyhow!("No models configured"))
        }
    }

    fn create_test_model(id: &str) -> OpenAIModel {
        OpenAIModel {
            id: id.to_string(),
            object: "model".to_string(),
            created: Some(1234567890),
            owned_by: "test".to_string(),
        }
    }

    #[tokio::test]
    async fn test_sync_models_adds_new_models() {
        let mut repo = MockDeploymentsRepo::new();
        let fetch_models = MockFetchModels::new();

        // Configure fetch_models to return new models
        let models = vec![create_test_model("gpt-3.5-turbo"), create_test_model("gpt-4")];
        fetch_models.set_models(models);

        let endpoint_info = create_test_endpoint();

        // Run sync - should add 2 new models
        let result = sync_endpoint_models(endpoint_info, &mut repo, fetch_models).await.unwrap();
        assert_eq!(result.changes_made, 2);
        assert_eq!(result.new_models_created, 2);
        assert_eq!(result.models_reactivated, 0);
        assert_eq!(result.models_deactivated, 0);

        // Verify models were added to repository
        let deployments = repo.list(&DeploymentFilter::new(0, 10)).await.unwrap();
        assert_eq!(deployments.len(), 2);

        let model_names: std::collections::HashSet<String> = deployments.iter().map(|d| d.model_name.clone()).collect();
        assert!(model_names.contains("gpt-3.5-turbo"));
        assert!(model_names.contains("gpt-4"));
    }

    #[tokio::test]
    async fn test_sync_models_marks_missing_models_inactive() {
        let mut repo = MockDeploymentsRepo::new();
        let fetch_models = MockFetchModels::new();

        // Add existing models to repository
        repo.add_deployment("old-model-1".to_string(), "old-model-1".to_string()).await;
        repo.add_deployment("old-model-2".to_string(), "old-model-2".to_string()).await;

        // Configure fetch_models to return empty list (all models should be marked inactive)
        fetch_models.set_models(vec![]);

        let endpoint_info = create_test_endpoint();

        // Run sync - should mark 2 models inactive
        let result = sync_endpoint_models(endpoint_info, &mut repo, fetch_models).await.unwrap();
        assert_eq!(result.changes_made, 2);
        assert_eq!(result.new_models_created, 0);
        assert_eq!(result.models_reactivated, 0);
        assert_eq!(result.models_deactivated, 2);

        // Verify models remain in repository but are marked inactive
        let deployments = repo.list(&DeploymentFilter::new(0, 10)).await.unwrap();
        assert_eq!(deployments.len(), 2); // Models still exist, not deleted

        // Check that all models are marked inactive
        for deployment in &deployments {
            assert_eq!(deployment.status, ModelStatus::Inactive);
            assert!(deployment.last_sync.is_some()); // Should have sync timestamp
        }
    }

    #[tokio::test]
    async fn test_sync_models_mixed_add_delete() {
        let mut repo = MockDeploymentsRepo::new();
        let fetch_models = MockFetchModels::new();

        // Add existing models to repository
        repo.add_deployment("keep-model".to_string(), "keep-model".to_string()).await;
        repo.add_deployment("delete-model".to_string(), "delete-model".to_string()).await;

        // Configure fetch_models to return one existing model and one new model
        let models = vec![
            create_test_model("keep-model"), // This should stay
            create_test_model("new-model"),  // This should be added
        ];
        fetch_models.set_models(models);

        let endpoint_info = create_test_endpoint();

        // Run sync - should add 1 new model and mark 1 model inactive
        let result = sync_endpoint_models(endpoint_info, &mut repo, fetch_models).await.unwrap();
        assert_eq!(result.changes_made, 2); // 1 added + 1 marked inactive
        assert_eq!(result.new_models_created, 1);
        assert_eq!(result.models_deactivated, 1);

        // Verify final state - all models remain in database with status tracking
        let deployments = repo.list(&DeploymentFilter::new(0, 10)).await.unwrap();
        assert_eq!(deployments.len(), 3); // All models remain, none deleted

        let model_names: std::collections::HashSet<String> = deployments.iter().map(|d| d.model_name.clone()).collect();
        assert!(model_names.contains("keep-model"));
        assert!(model_names.contains("new-model"));
        assert!(model_names.contains("delete-model")); // Model still exists but should be inactive

        // Check status of each model
        for deployment in &deployments {
            match deployment.model_name.as_str() {
                "keep-model" => assert_eq!(deployment.status, ModelStatus::Active),
                "new-model" => assert_eq!(deployment.status, ModelStatus::Active),
                "delete-model" => assert_eq!(deployment.status, ModelStatus::Inactive),
                _ => panic!("Unexpected model: {}", deployment.model_name),
            }
        }
    }

    #[tokio::test]
    async fn test_sync_models_no_changes() {
        let mut repo = MockDeploymentsRepo::new();
        let fetch_models = MockFetchModels::new();

        // Add existing model to repository
        repo.add_deployment("existing-model".to_string(), "existing-model".to_string())
            .await;

        // Configure fetch_models to return the same model
        let models = vec![create_test_model("existing-model")];
        fetch_models.set_models(models);

        let endpoint_info = create_test_endpoint();

        // Run sync - no changes should occur
        let result = sync_endpoint_models(endpoint_info, &mut repo, fetch_models).await.unwrap();
        assert_eq!(result.changes_made, 0);
        assert_eq!(result.new_models_created, 0);
        assert_eq!(result.models_reactivated, 0);
        assert_eq!(result.models_deactivated, 0);

        // Verify model is still there
        let deployments = repo.list(&DeploymentFilter::new(0, 10)).await.unwrap();
        assert_eq!(deployments.len(), 1);
        assert_eq!(deployments[0].model_name, "existing-model");
    }

    #[tokio::test]
    async fn test_create_deployment_with_duplicate_alias_fails() {
        let mut repo = MockDeploymentsRepo::new();
        let fetch_models = MockFetchModels::new();

        // Set up two models
        let models = vec![create_test_model("google/gemma-3-12b-it"), create_test_model("openai/gpt-4")];
        fetch_models.set_models(models);

        let endpoint_info = create_test_endpoint();

        // First sync with unique aliases should succeed
        let mut alias_mapping = HashMap::new();
        alias_mapping.insert("google/gemma-3-12b-it".to_string(), "alias-1".to_string());
        alias_mapping.insert("openai/gpt-4".to_string(), "alias-2".to_string());

        let result = crate::sync::endpoint_sync::sync_endpoint_models_with_aliases(
            endpoint_info.clone(),
            &mut repo,
            fetch_models.clone(),
            &Some(alias_mapping),
        )
        .await;
        assert!(result.is_ok());

        // Second sync with a duplicate alias should fail
        let mut alias_mapping_conflict = HashMap::new();
        alias_mapping_conflict.insert("google/gemma-3-12b-it".to_string(), "shared-alias".to_string());
        alias_mapping_conflict.insert("openai/gpt-4".to_string(), "shared-alias".to_string());

        let result = crate::sync::endpoint_sync::sync_endpoint_models_with_aliases(
            endpoint_info.clone(),
            &mut repo,
            fetch_models.clone(),
            &Some(alias_mapping_conflict),
        )
        .await;
        assert!(matches!(result, Err(crate::sync::endpoint_sync::SyncError::AliasConflicts { .. })));
    }

    #[tokio::test]
    async fn test_create_deployment_with_default_alias_conflict() {
        let mut repo = MockDeploymentsRepo::new();
        let fetch_models = MockFetchModels::new();

        // Set up a model
        let models = vec![create_test_model("google/gemma-3-12b-it")];
        fetch_models.set_models(models);

        let endpoint_info = create_test_endpoint();

        // First sync with no alias mapping (alias defaults to model name)
        let result =
            crate::sync::endpoint_sync::sync_endpoint_models_with_aliases(endpoint_info.clone(), &mut repo, fetch_models.clone(), &None)
                .await;
        assert!(result.is_ok());

        // Try to sync again with the same model and no alias mapping (should conflict)
        let result =
            crate::sync::endpoint_sync::sync_endpoint_models_with_aliases(endpoint_info.clone(), &mut repo, fetch_models.clone(), &None)
                .await;
        // Should not error, as it's the same endpoint and model (idempotent)
        assert!(result.is_ok());

        // Now, simulate a second endpoint trying to use the same alias (model name)
        let endpoint_info2 = InferenceEndpointDBResponse {
            id: uuid::Uuid::new_v4(),
            ..create_test_endpoint()
        };
        let result =
            crate::sync::endpoint_sync::sync_endpoint_models_with_aliases(endpoint_info2, &mut repo, fetch_models.clone(), &None).await;
        assert!(matches!(result, Err(crate::sync::endpoint_sync::SyncError::AliasConflicts { .. })));
    }

    #[tokio::test]
    async fn test_update_endpoint_aliases_duplicate_fails() {
        let mut repo = MockDeploymentsRepo::new();

        // Add two deployments with unique aliases
        let _id1 = repo
            .add_deployment("google/gemma-3-12b-it".to_string(), "alias-1".to_string())
            .await;
        let _id2 = repo.add_deployment("openai/gpt-4".to_string(), "alias-2".to_string()).await;

        let endpoint_info = create_test_endpoint();

        // Try to update both to the same alias
        let mut alias_mapping = HashMap::new();
        alias_mapping.insert("google/gemma-3-12b-it".to_string(), "shared-alias".to_string());
        alias_mapping.insert("openai/gpt-4".to_string(), "shared-alias".to_string());

        let result = crate::sync::endpoint_sync::update_endpoint_aliases(endpoint_info.clone(), &mut repo, &alias_mapping).await;
        assert!(matches!(result, Err(crate::sync::endpoint_sync::SyncError::AliasConflicts { .. })));
    }

    #[tokio::test]
    async fn test_update_endpoint_aliases_unique_ok() {
        let mut repo = MockDeploymentsRepo::new();

        // Add two deployments with unique aliases
        let _id1 = repo
            .add_deployment("google/gemma-3-12b-it".to_string(), "alias-1".to_string())
            .await;
        let _id2 = repo.add_deployment("openai/gpt-4".to_string(), "alias-2".to_string()).await;

        let endpoint_info = create_test_endpoint();

        // Update aliases to new unique values
        let mut alias_mapping = HashMap::new();
        alias_mapping.insert("google/gemma-3-12b-it".to_string(), "alias-3".to_string());
        alias_mapping.insert("openai/gpt-4".to_string(), "alias-4".to_string());

        let result = crate::sync::endpoint_sync::update_endpoint_aliases(endpoint_info.clone(), &mut repo, &alias_mapping).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_update_endpoint_aliases_default_alias_conflict() {
        let mut repo = MockDeploymentsRepo::new();

        // Add a deployment with alias = model name
        let _id1 = repo
            .add_deployment("google/gemma-3-12b-it".to_string(), "google/gemma-3-12b-it".to_string())
            .await;

        let endpoint_info = create_test_endpoint();

        // Try to update another deployment to use the same alias
        let _id2 = repo.add_deployment("openai/gpt-4".to_string(), "alias-2".to_string()).await;

        let mut alias_mapping = HashMap::new();
        alias_mapping.insert("openai/gpt-4".to_string(), "google/gemma-3-12b-it".to_string());

        let result = crate::sync::endpoint_sync::update_endpoint_aliases(endpoint_info.clone(), &mut repo, &alias_mapping).await;
        assert!(matches!(result, Err(crate::sync::endpoint_sync::SyncError::AliasConflicts { .. })));
    }

    #[tokio::test]
    async fn test_sync_endpoint_models_with_no_alias_mapping_defaults_to_model_name() {
        let mut repo = MockDeploymentsRepo::new();
        let fetch_models = MockFetchModels::new();

        // Configure fetch_models to return two models
        let models = vec![create_test_model("google/gemma-3-12b-it"), create_test_model("openai/gpt-4")];
        fetch_models.set_models(models);

        let endpoint_info = create_test_endpoint();

        // Run sync with no alias mapping
        let result =
            crate::sync::endpoint_sync::sync_endpoint_models_with_aliases(endpoint_info.clone(), &mut repo, fetch_models.clone(), &None)
                .await;
        assert!(result.is_ok());

        // Verify deployments were created with alias == model_name
        let deployments = repo.list(&DeploymentFilter::new(0, 10)).await.unwrap();
        let mut found_gemma = false;
        let mut found_gpt4 = false;
        for d in deployments {
            if d.model_name == "google/gemma-3-12b-it" {
                assert_eq!(d.alias, "google/gemma-3-12b-it");
                found_gemma = true;
            }
            if d.model_name == "openai/gpt-4" {
                assert_eq!(d.alias, "openai/gpt-4");
                found_gpt4 = true;
            }
        }
        assert!(found_gemma && found_gpt4);
    }

    #[tokio::test]
    async fn test_sync_endpoint_models_with_alias_mapping() {
        let mut repo = MockDeploymentsRepo::new();
        let fetch_models = MockFetchModels::new();

        // Configure fetch_models to return two models
        let models = vec![create_test_model("google/gemma-3-12b-it"), create_test_model("openai/gpt-4")];
        fetch_models.set_models(models);

        let _endpoint_info = create_test_endpoint();

        // First endpoint sync (should succeed)
        let endpoint_info1 = create_test_endpoint();
        let result = sync_endpoint_models_with_aliases(endpoint_info1.clone(), &mut repo, fetch_models.clone(), &None).await;
        assert!(result.is_ok());

        // Second endpoint sync (should fail due to alias conflict)
        let endpoint_info2 = InferenceEndpointDBResponse {
            id: uuid::Uuid::new_v4(),
            ..create_test_endpoint()
        };
        let result = sync_endpoint_models_with_aliases(endpoint_info2, &mut repo, fetch_models.clone(), &None).await;
        assert!(matches!(result, Err(crate::sync::endpoint_sync::SyncError::AliasConflicts { .. })));
    }
}
