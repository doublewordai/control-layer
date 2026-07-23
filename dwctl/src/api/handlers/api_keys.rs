//! HTTP handlers for API key management endpoints.

use crate::api::models::api_keys::ListApiKeysQuery;
use crate::{
    AppState,
    api::models::{
        api_keys::{ApiKeyCreate, ApiKeyInfoResponse, ApiKeyResponse, ApiKeyUpdate},
        pagination::PaginatedResponse,
        users::CurrentUser,
    },
    auth::permissions::{
        can_create_all_resources, can_create_own_resource, can_delete_all_resources, can_delete_own_resource, can_read_all_resources,
        can_read_own_resource, can_update_all_resources, can_update_own_resource, is_org_member,
    },
    db::handlers::{Repository, api_keys::ApiKeyFilter, api_keys::ApiKeys},
    db::models::api_keys::{ApiKeyCreateDBRequest, ApiKeyPurpose, ApiKeyUpdateDBRequest},
    errors::{Error, Result},
    types::{ApiKeyId, Operation, Permission, Resource, UserIdOrCurrent},
};
use rust_decimal::Decimal;
use sqlx_pool_router::PoolProvider;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
};
use sqlx::Acquire;

/// Valid spend-cap reset periods (calendar-aligned UTC; see migration 122/123).
const VALID_CAP_INTERVALS: [&str; 3] = ["daily", "weekly", "monthly"];

/// Validate a (spend_limit, spend_limit_interval) pair as submitted via the
/// API. Mirrors the DB CHECK constraints so users get a 400 with a message
/// instead of a 500 from a constraint violation.
fn validate_cap_fields(spend_limit: Option<&Decimal>, spend_limit_interval: Option<&str>) -> Result<()> {
    if let Some(limit) = spend_limit
        && *limit <= Decimal::ZERO
    {
        return Err(Error::BadRequest {
            message: "spend_limit must be greater than zero".to_string(),
        });
    }
    if let Some(interval) = spend_limit_interval {
        if !VALID_CAP_INTERVALS.contains(&interval) {
            return Err(Error::BadRequest {
                message: format!("spend_limit_interval must be one of {VALID_CAP_INTERVALS:?} (calendar-aligned UTC windows)"),
            });
        }
        if spend_limit.is_none() {
            return Err(Error::BadRequest {
                message: "spend_limit_interval requires spend_limit".to_string(),
            });
        }
    }
    Ok(())
}

/// Create an API key for the current user or a specified user.
/// This returns `ApiKeyResponse`, which contains the actual API key.
///
/// This should be the only time that the API key is returned in a response.
#[utoipa::path(
    post,
    path = "/users/{user_id}/api-keys",
    tag = "api_keys",
    summary = "Create API key",
    description = "Create an API key for the current user or a specified user",
    params(
        ("user_id" = String, Path, description = "User ID (UUID) or 'current' for current user"),
    ),
    responses(
        (status = 201, description = "API key created successfully", body = ApiKeyResponse),
        (status = 400, description = "Bad request - invalid API key data"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - can only manage own API keys unless admin"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn create_user_api_key<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(user_id): Path<UserIdOrCurrent>,
    current_user: CurrentUser,
    Json(data): Json<ApiKeyCreate>,
) -> Result<(StatusCode, Json<ApiKeyResponse>)> {
    // Validate input data
    if data.name.trim().is_empty() {
        return Err(Error::BadRequest {
            message: "API key name cannot be empty".to_string(),
        });
    }
    validate_cap_fields(data.spend_limit.as_ref(), data.spend_limit_interval.as_deref())?;

    let target_user_id = match user_id {
        UserIdOrCurrent::Current(_) => current_user.id,
        UserIdOrCurrent::Id(uuid) => uuid,
    };

    // Check permissions: CreateAll, CreateOwn, or org membership
    let can_create_all = can_create_all_resources(&current_user, Resource::ApiKeys);
    let can_create_own = can_create_own_resource(&current_user, Resource::ApiKeys, target_user_id);

    if !can_create_all && !can_create_own {
        let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
        let member = is_org_member(&current_user, target_user_id, &mut conn)
            .await
            .map_err(Error::Database)?;
        if !member {
            return Err(Error::InsufficientPermissions {
                required: Permission::Any(vec![
                    Permission::Allow(Resource::ApiKeys, Operation::CreateAll),
                    Permission::Allow(Resource::ApiKeys, Operation::CreateOwn),
                ]),
                action: Operation::CreateOwn,
                resource: format!("API keys for user {target_user_id}"),
            });
        }
    }

    // Only PlatformManagers can specify member_id to attribute a key to another org member
    if data.member_id.is_some() && !can_create_all {
        return Err(Error::InsufficientPermissions {
            required: Permission::Allow(Resource::ApiKeys, Operation::CreateAll),
            action: Operation::CreateAll,
            resource: "API keys with member_id (requires PlatformManager)".to_string(),
        });
    }

    // Validate purpose: restrict batch/playground to system use only (purpose defaults to Realtime via serde)
    match &data.purpose {
        crate::db::models::api_keys::ApiKeyPurpose::Batch | crate::db::models::api_keys::ApiKeyPurpose::Playground => {
            return Err(Error::BadRequest {
                message:
                    "Cannot manually create API keys with 'batch' or 'playground' purpose. These are reserved for internal system use."
                        .to_string(),
            });
        }
        _ => {}
    }

    let mut pool_conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;

    // Check if target is an organization
    let target_is_org = {
        let mut org_repo = crate::db::handlers::Organizations::new(&mut pool_conn);
        org_repo.exists(target_user_id).await.map_err(Error::Database)?
    };

    // Validate member_id: must be a member of the target org
    if let Some(member_id) = data.member_id {
        if !target_is_org {
            return Err(Error::BadRequest {
                message: "member_id can only be used when creating keys for an organization".to_string(),
            });
        }
        let mut org_repo = crate::db::handlers::Organizations::new(&mut pool_conn);
        let role = org_repo
            .get_user_org_role(member_id, target_user_id)
            .await
            .map_err(Error::Database)?;
        if role.is_none() {
            return Err(Error::BadRequest {
                message: format!("User {member_id} is not a member of organization {target_user_id}"),
            });
        }
    }

    // Determine created_by based on target type:
    // - Organization target: attribute to specified member_id, or current user
    // - Individual target (PM creating on behalf): attribute to the target user
    //   so the key is visible to them
    // - Self: attribute to current user
    let pm_creating_for_other = can_create_all && target_user_id != current_user.id;
    let created_by = if target_is_org {
        data.member_id.unwrap_or(current_user.id)
    } else if pm_creating_for_other {
        target_user_id
    } else {
        current_user.id
    };

    let mut repo = ApiKeys::new(&mut pool_conn);
    let has_cap = data.spend_limit.is_some();
    let db_request = ApiKeyCreateDBRequest::new(target_user_id, created_by, data);

    let api_key = repo.create(&db_request).await?;

    // Capped keys need their cap scope provisioned up front: the hidden batch
    // child (so batch/flex traffic executes inside the scope, and is in
    // onwards' key set before the first request fires) and a zeroed spend
    // window (the cap counts from now).
    if has_cap {
        repo.get_or_create_child_hidden_key(api_key.id).await?;
        repo.reset_spend_window(api_key.id).await?;
    }

    let key_id = api_key.id;
    let spend_states = repo.get_spend_states(&[key_id]).await?;

    // api_key.created webhook deliveries are created by the notification poller
    // via PG LISTEN/NOTIFY on the api_keys table.

    Ok((
        StatusCode::CREATED,
        Json(ApiKeyResponse::from(api_key).with_spend_state(spend_states.get(&key_id))),
    ))
}

/// List the API keys for the current user or a specified user.
/// This should never contain the actual API key value.
#[utoipa::path(
    get,
    path = "/users/{user_id}/api-keys",
    tag = "api_keys",
    summary = "List API keys",
    description = "List API keys for the current user or a specified user",
    params(
        ("user_id" = String, Path, description = "User ID (UUID) or 'current' for current user"),
        ListApiKeysQuery
    ),
    responses(
        (status = 200, description = "Paginated list of API keys", body = PaginatedResponse<ApiKeyInfoResponse>),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - can only view own API keys unless admin"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn list_user_api_keys<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(user_id): Path<UserIdOrCurrent>,
    Query(query): Query<ListApiKeysQuery>,
    // Can't use RequiresPermission here because we need conditional logic for own vs other users
    current_user: CurrentUser,
) -> Result<Json<PaginatedResponse<ApiKeyInfoResponse>>> {
    let target_user_id = match user_id {
        UserIdOrCurrent::Current(_) => current_user.id,
        UserIdOrCurrent::Id(uuid) => uuid,
    };

    // Check permissions: ReadAll, ReadOwn, or org membership
    let can_read_all = can_read_all_resources(&current_user, Resource::ApiKeys);
    let can_read_own = can_read_own_resource(&current_user, Resource::ApiKeys, target_user_id);

    if !can_read_all && !can_read_own {
        let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
        let member = is_org_member(&current_user, target_user_id, &mut conn)
            .await
            .map_err(Error::Database)?;
        if !member {
            return Err(Error::InsufficientPermissions {
                required: Permission::Any(vec![
                    Permission::Allow(Resource::ApiKeys, Operation::ReadAll),
                    Permission::Allow(Resource::ApiKeys, Operation::ReadOwn),
                ]),
                action: Operation::ReadOwn,
                resource: format!("API keys for user {target_user_id}"),
            });
        }
    }

    // PlatformManagers (ReadAll) see all keys for a user; everyone else is scoped to created_by.
    let skip_created_by_filter = can_read_all;

    // Use read replica for this read-only operation
    let mut pool_conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ApiKeys::new(&mut pool_conn);

    // Extract pagination parameters with defaults & validation
    let skip = query.pagination.skip();
    let limit = query.pagination.limit();

    // Scope to keys created by this user, unless they have ReadAll (PlatformManager).
    // PM-created keys on behalf of users have created_by = target_user_id, so the target
    // user can always see them. PMs bypass the filter to see all keys for any user.
    let filter = ApiKeyFilter {
        skip,
        limit,
        user_id: Some(target_user_id),
        created_by: if skip_created_by_filter { None } else { Some(current_user.id) },
    };

    // Get total count and list of items
    let total_count = repo.count(&filter).await?;
    let api_keys = repo.list(&filter).await?;

    // Bulk-attach spend display state (one PK-joined query for the page).
    let ids: Vec<ApiKeyId> = api_keys.iter().map(|k| k.id).collect();
    let spend_states = repo.get_spend_states(&ids).await?;

    let data: Vec<ApiKeyInfoResponse> = api_keys
        .into_iter()
        .map(|k| {
            let state = spend_states.get(&k.id);
            ApiKeyInfoResponse::from(k).with_spend_state(state)
        })
        .collect();

    Ok(Json(PaginatedResponse::new(data, total_count, skip, limit)))
}

/// Get a specific API key for the current user or a specified user.
#[utoipa::path(
    get,
    path = "/users/{user_id}/api-keys/{id}",
    tag = "api_keys",
    summary = "Get API key",
    description = "Get a specific API key for the current user or a specified user",
    params(
        ("user_id" = String, Path, description = "User ID (UUID) or 'current' for current user"),
        ("id" = uuid::Uuid, Path, description = "API key ID to retrieve"),
    ),
    responses(
        (status = 200, description = "API key information", body = ApiKeyInfoResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - can only view own API keys unless admin"),
        (status = 404, description = "API key not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn get_user_api_key<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path((user_id, api_key_id)): Path<(UserIdOrCurrent, ApiKeyId)>,
    // Can't use RequiresPermission here because we need conditional logic for own vs other users
    current_user: CurrentUser,
) -> Result<Json<ApiKeyInfoResponse>> {
    let target_user_id = match user_id {
        UserIdOrCurrent::Current(_) => current_user.id,
        UserIdOrCurrent::Id(uuid) => uuid,
    };

    // Check permissions: ReadAll, ReadOwn, or org membership
    let can_read_all = can_read_all_resources(&current_user, Resource::ApiKeys);
    let can_read_own = can_read_own_resource(&current_user, Resource::ApiKeys, target_user_id);

    if !can_read_all && !can_read_own {
        let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
        let member = is_org_member(&current_user, target_user_id, &mut conn)
            .await
            .map_err(Error::Database)?;
        if !member {
            return Err(Error::InsufficientPermissions {
                required: Permission::Any(vec![
                    Permission::Allow(Resource::ApiKeys, Operation::ReadAll),
                    Permission::Allow(Resource::ApiKeys, Operation::ReadOwn),
                ]),
                action: Operation::ReadOwn,
                resource: format!("API keys for user {target_user_id}"),
            });
        }
    }

    let skip_created_by_filter = can_read_all;

    // Use read replica for this read-only operation
    let mut pool_conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ApiKeys::new(&mut pool_conn);

    // Get the specific API key, verify ownership and created_by visibility
    let api_key = repo
        .get_by_id(api_key_id)
        .await?
        .filter(|key| key.user_id == target_user_id)
        .filter(|key| skip_created_by_filter || key.created_by == current_user.id)
        .ok_or_else(|| Error::NotFound {
            resource: "API key".to_string(),
            id: api_key_id.to_string(),
        })?;

    let key_id = api_key.id;
    let spend_states = repo.get_spend_states(&[key_id]).await?;

    Ok(Json(ApiKeyInfoResponse::from(api_key).with_spend_state(spend_states.get(&key_id))))
}

/// Update a specific API key: metadata, rate limits, and the spending cap.
#[utoipa::path(
    patch,
    path = "/users/{user_id}/api-keys/{id}",
    tag = "api_keys",
    summary = "Update API key",
    description = "Update an API key's name, description, rate limits, or spending cap. \
                   Setting a cap where none existed provisions cap-scope batch/flex execution and starts a fresh spend window; \
                   changing the cap interval or passing reset_window also restarts the window; \
                   passing spend_limit: null removes the cap.",
    request_body = ApiKeyUpdate,
    params(
        ("user_id" = String, Path, description = "User ID (UUID) or 'current' for current user"),
        ("id" = uuid::Uuid, Path, description = "API key ID to update"),
    ),
    responses(
        (status = 200, description = "Updated API key", body = ApiKeyInfoResponse),
        (status = 400, description = "Bad request - invalid update data"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - can only manage own API keys unless admin"),
        (status = 404, description = "API key not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn update_user_api_key<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path((user_id, api_key_id)): Path<(UserIdOrCurrent, ApiKeyId)>,
    // Can't use RequiresPermission here because we need conditional logic for own vs other users
    current_user: CurrentUser,
    Json(data): Json<ApiKeyUpdate>,
) -> Result<Json<ApiKeyInfoResponse>> {
    let target_user_id = match user_id {
        UserIdOrCurrent::Current(_) => current_user.id,
        UserIdOrCurrent::Id(uuid) => uuid,
    };

    // Check permissions: UpdateAll, UpdateOwn, or org membership — the same
    // shape as create/delete.
    let can_update_all = can_update_all_resources(&current_user, Resource::ApiKeys);
    let can_update_own = can_update_own_resource(&current_user, Resource::ApiKeys, target_user_id);

    if !can_update_all && !can_update_own {
        let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
        let member = is_org_member(&current_user, target_user_id, &mut conn)
            .await
            .map_err(Error::Database)?;
        if !member {
            return Err(Error::InsufficientPermissions {
                required: Permission::Any(vec![
                    Permission::Allow(Resource::ApiKeys, Operation::UpdateAll),
                    Permission::Allow(Resource::ApiKeys, Operation::UpdateOwn),
                ]),
                action: Operation::UpdateOwn,
                resource: format!("API keys for user {target_user_id}"),
            });
        }
    }

    let skip_created_by_filter = can_update_all;

    let mut tx = state.db.write().begin().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ApiKeys::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);

    // Fetch and gate the key. System-managed keys (hidden batch/playground and
    // cap-scope children) are never updatable through the API — their
    // lifecycle is derived (children: minted at cap-set, revoked with parent).
    let key = repo
        .get_by_id(api_key_id)
        .await?
        .filter(|key| key.user_id == target_user_id)
        .filter(|key| skip_created_by_filter || key.created_by == current_user.id)
        .filter(|key| key.parent_api_key_id.is_none())
        .filter(|key| matches!(key.purpose, ApiKeyPurpose::Realtime | ApiKeyPurpose::Platform))
        .ok_or_else(|| Error::NotFound {
            resource: "API key".to_string(),
            id: api_key_id.to_string(),
        })?;

    // Resolve the cap tri-state against current values: absent = unchanged,
    // explicit null = clear, value = set.
    let old_limit = key.spend_limit;
    let old_interval = key.spend_limit_interval.clone();
    let new_limit = match data.spend_limit {
        None => old_limit,
        Some(v) => v,
    };
    let new_interval = match data.spend_limit_interval.clone() {
        None => {
            // Clearing the cap implicitly clears the interval (the DB CHECK
            // forbids an interval without a limit).
            if new_limit.is_none() { None } else { old_interval.clone() }
        }
        Some(v) => v,
    };
    validate_cap_fields(new_limit.as_ref(), new_interval.as_deref())?;

    let reset_window = data.reset_window.unwrap_or(false);
    if reset_window && new_limit.is_none() {
        return Err(Error::BadRequest {
            message: "reset_window requires a spending cap".to_string(),
        });
    }

    // Generic metadata/rate-limit fields via the existing repository update.
    if data.name.is_some() || data.description.is_some() || data.requests_per_second.is_some() || data.burst_size.is_some() {
        if let Some(name) = &data.name
            && name.trim().is_empty()
        {
            return Err(Error::BadRequest {
                message: "API key name cannot be empty".to_string(),
            });
        }
        repo.update(
            api_key_id,
            &ApiKeyUpdateDBRequest {
                name: data.name.clone(),
                description: data.description.clone(),
                requests_per_second: data.requests_per_second,
                burst_size: data.burst_size,
            },
        )
        .await?;
    }

    // Cap changes. The window resets when a cap appears where none was
    // (REQUIRED — otherwise the scope inherits spend accumulated before or
    // between caps and can exhaust immediately), when the interval changes,
    // or on the explicit re-arm flag. The lifetime total is never reset.
    let cap_changed = new_limit != old_limit || new_interval != old_interval;
    let newly_capped = old_limit.is_none() && new_limit.is_some();
    let interval_changed = new_limit.is_some() && new_interval != old_interval;

    if cap_changed {
        repo.update_spend_cap(api_key_id, new_limit, new_interval.clone()).await?;
    }
    if newly_capped {
        // Provision the cap scope: hidden batch child for batch/flex coverage.
        // Idempotent — re-capping reuses the existing child.
        repo.get_or_create_child_hidden_key(api_key_id).await?;
    }
    if newly_capped || interval_changed || reset_window {
        repo.reset_spend_window(api_key_id).await?;
    }

    let updated = repo.get_by_id(api_key_id).await?.ok_or_else(|| Error::NotFound {
        resource: "API key".to_string(),
        id: api_key_id.to_string(),
    })?;
    let spend_states = repo.get_spend_states(&[api_key_id]).await?;

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    Ok(Json(
        ApiKeyInfoResponse::from(updated).with_spend_state(spend_states.get(&api_key_id)),
    ))
}

/// Delete a specific API key for the current user or a specified user.
#[utoipa::path(
    delete,
    path = "/users/{user_id}/api-keys/{id}",
    tag = "api_keys",
    summary = "Delete API key",
    description = "Delete a specific API key for the current user or a specified user",
    params(
        ("user_id" = String, Path, description = "User ID (UUID) or 'current' for current user"),
        ("id" = uuid::Uuid, Path, description = "API key ID to delete"),
    ),
    responses(
        (status = 204, description = "API key deleted successfully"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden - can only delete own API keys unless admin"),
        (status = 404, description = "API key not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[tracing::instrument(skip_all)]
pub async fn delete_user_api_key<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path((user_id, api_key_id)): Path<(UserIdOrCurrent, ApiKeyId)>,
    // Can't use RequiresPermission here because we need conditional logic for own vs other users
    current_user: CurrentUser,
) -> Result<StatusCode> {
    let target_user_id = match user_id {
        UserIdOrCurrent::Current(_) => current_user.id,
        UserIdOrCurrent::Id(uuid) => uuid,
    };

    // Check permissions: DeleteAll, DeleteOwn, or org membership
    let can_delete_all = can_delete_all_resources(&current_user, Resource::ApiKeys);
    let can_delete_own = can_delete_own_resource(&current_user, Resource::ApiKeys, target_user_id);

    if !can_delete_all && !can_delete_own {
        let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
        let member = is_org_member(&current_user, target_user_id, &mut conn)
            .await
            .map_err(Error::Database)?;
        if !member {
            return Err(Error::InsufficientPermissions {
                required: Permission::Any(vec![
                    Permission::Allow(Resource::ApiKeys, Operation::DeleteAll),
                    Permission::Allow(Resource::ApiKeys, Operation::DeleteOwn),
                ]),
                action: Operation::DeleteOwn,
                resource: format!("API keys for user {target_user_id}"),
            });
        }
    }

    let skip_created_by_filter = can_delete_all;

    let mut tx = state.db.write().begin().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = ApiKeys::new(tx.acquire().await.map_err(|e| Error::Database(e.into()))?);

    // Check if the API key exists, belongs to the target user, and was created by current user.
    // Cap-scope child keys (parent_api_key_id set) are system-managed and can
    // never be deleted directly — their ids leak via transaction rows, and
    // deleting one would silently route the parent's batch/flex traffic back
    // to the shared (uncapped) hidden key, bypassing the spending cap. They
    // are revoked only via their parent's deletion (repo cascade).
    repo.get_by_id(api_key_id)
        .await?
        .filter(|key| key.user_id == target_user_id)
        .filter(|key| skip_created_by_filter || key.created_by == current_user.id)
        .filter(|key| key.parent_api_key_id.is_none())
        .ok_or_else(|| Error::NotFound {
            resource: "API key".to_string(),
            id: api_key_id.to_string(),
        })?;

    // Now delete the API key
    repo.delete(api_key_id).await?;
    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use crate::api::models::api_keys::{ApiKeyInfoResponse, ApiKeyResponse};
    use crate::api::models::pagination::PaginatedResponse;
    use crate::api::models::users::Role;
    use crate::test::utils::*;
    use serde_json::json;
    use sqlx::PgPool;

    /// PATCH helper: send an update body for a key as a given user.
    async fn patch_key(
        app: &axum_test::TestServer,
        user: &crate::api::models::users::UserResponse,
        key_id: crate::types::ApiKeyId,
        body: serde_json::Value,
    ) -> axum_test::TestResponse {
        let auth = add_auth_headers(user);
        app.patch(&format!("/admin/api/v1/users/current/api-keys/{key_id}"))
            .json(&body)
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await
    }

    fn window_spend_of(resp: &ApiKeyInfoResponse) -> rust_decimal::Decimal {
        resp.spend.expect("capped key should carry a spend value")
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_api_key_with_spend_cap(pool: PgPool) {
        use crate::db::handlers::{Repository as _, api_keys::ApiKeys};

        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let auth = add_auth_headers(&user);

        let response = app
            .post("/admin/api/v1/users/current/api-keys")
            .json(&json!({
                "name": "Capped Key",
                "spend_limit": "50",
                "spend_limit_interval": "daily"
            }))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;
        response.assert_status(axum::http::StatusCode::CREATED);
        let created: ApiKeyResponse = response.json();
        assert_eq!(created.spend_limit, Some(rust_decimal::Decimal::from(50)));
        assert_eq!(created.spend_limit_interval.as_deref(), Some("daily"));
        assert!(created.resets_at.is_some(), "windowed cap advertises its next reset");
        assert_eq!(created.spend, Some(rust_decimal::Decimal::ZERO), "fresh cap starts a zeroed window");

        // Cap scope provisioned: hidden batch child + zeroed checkpoint row.
        let mut conn = pool.acquire().await.unwrap();
        let child: Option<uuid::Uuid> =
            sqlx::query_scalar("SELECT id FROM api_keys WHERE parent_api_key_id = $1 AND purpose = 'batch' AND hidden = true")
                .bind(created.id)
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert!(child.is_some(), "cap-set must mint the batch/flex child");
        let window_spend: rust_decimal::Decimal =
            sqlx::query_scalar("SELECT window_spend FROM api_key_spend_checkpoints WHERE api_key_id = $1")
                .bind(created.id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(window_spend, rust_decimal::Decimal::ZERO);

        // The resolver now routes this key's batch/flex work to the child.
        let (_, resolved) = ApiKeys::new(&mut conn)
            .resolve_batch_execution_key(user.id, user.id, Some(created.id))
            .await
            .unwrap();
        assert_eq!(Some(resolved), child);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_api_key_cap_validation(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let auth = add_auth_headers(&user);
        let post = |body: serde_json::Value| {
            let auth = add_auth_headers(&user);
            let app = &app;
            async move {
                app.post("/admin/api/v1/users/current/api-keys")
                    .json(&body)
                    .add_header(&auth[0].0, &auth[0].1)
                    .add_header(&auth[1].0, &auth[1].1)
                    .await
            }
        };

        // Zero / negative limit.
        post(json!({"name": "k1", "spend_limit": "0"}))
            .await
            .assert_status(axum::http::StatusCode::BAD_REQUEST);
        // Unknown interval.
        post(json!({"name": "k2", "spend_limit": "5", "spend_limit_interval": "hourly"}))
            .await
            .assert_status(axum::http::StatusCode::BAD_REQUEST);
        // Interval without a limit.
        post(json!({"name": "k3", "spend_limit_interval": "daily"}))
            .await
            .assert_status(axum::http::StatusCode::BAD_REQUEST);

        // Control: a valid one-off cap passes.
        let ok = app
            .post("/admin/api/v1/users/current/api-keys")
            .json(&json!({"name": "k4", "spend_limit": "5"}))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;
        ok.assert_status(axum::http::StatusCode::CREATED);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_patch_api_key_spend_cap_matrix(pool: PgPool) {
        use rust_decimal::Decimal;

        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let key = create_test_api_key_for_user(&pool, user.id).await;

        // Set from NULL: mints the child and starts a zeroed window.
        let resp = patch_key(&app, &user, key.id, json!({"spend_limit": "25"})).await;
        resp.assert_status_ok();
        let body: ApiKeyInfoResponse = resp.json();
        assert_eq!(body.spend_limit, Some(Decimal::from(25)));
        assert_eq!(window_spend_of(&body), Decimal::ZERO);
        let child_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE parent_api_key_id = $1")
            .bind(key.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(child_count, 1, "setting a cap mints exactly one child");

        // Seed some counted spend to observe keep-vs-reset behavior.
        sqlx::query("UPDATE api_key_spend_checkpoints SET window_spend = 5, total_spend = 5 WHERE api_key_id = $1")
            .bind(key.id)
            .execute(&pool)
            .await
            .unwrap();

        // Raise the limit: the window is KEPT.
        let resp = patch_key(&app, &user, key.id, json!({"spend_limit": "40"})).await;
        resp.assert_status_ok();
        let body: ApiKeyInfoResponse = resp.json();
        assert_eq!(body.spend_limit, Some(Decimal::from(40)));
        assert_eq!(window_spend_of(&body), Decimal::from(5), "raising the limit keeps counted spend");

        // Change the interval: the window RESETS (lifetime total is kept).
        let resp = patch_key(&app, &user, key.id, json!({"spend_limit_interval": "weekly"})).await;
        resp.assert_status_ok();
        let body: ApiKeyInfoResponse = resp.json();
        assert_eq!(body.spend_limit_interval.as_deref(), Some("weekly"));
        assert_eq!(window_spend_of(&body), Decimal::ZERO, "interval change resets the window");
        assert_eq!(body.total_spend, Some(Decimal::from(5)), "lifetime total survives resets");

        // Explicit re-arm resets the window too.
        sqlx::query("UPDATE api_key_spend_checkpoints SET window_spend = 7 WHERE api_key_id = $1")
            .bind(key.id)
            .execute(&pool)
            .await
            .unwrap();
        let resp = patch_key(&app, &user, key.id, json!({"reset_window": true})).await;
        resp.assert_status_ok();
        let body: ApiKeyInfoResponse = resp.json();
        assert_eq!(window_spend_of(&body), Decimal::ZERO, "reset_window re-arms the cap");

        // Clear the cap: columns NULLed, enforcement stops, the child SURVIVES
        // (it remains the execution key; re-capping reuses it).
        let resp = patch_key(&app, &user, key.id, json!({"spend_limit": null})).await;
        resp.assert_status_ok();
        let body: ApiKeyInfoResponse = resp.json();
        assert_eq!(body.spend_limit, None);
        assert_eq!(body.spend_limit_interval, None, "clearing the cap clears the interval");
        let child_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE parent_api_key_id = $1 AND is_deleted = false")
            .bind(key.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(child_count, 1, "cap removal must not revoke the child");

        // Re-enable: the window resets rather than inheriting old spend
        // (decision #8 — the contract on EnrichedRecord::cap_scope_root).
        sqlx::query("UPDATE api_key_spend_checkpoints SET window_spend = 99 WHERE api_key_id = $1")
            .bind(key.id)
            .execute(&pool)
            .await
            .unwrap();
        let resp = patch_key(&app, &user, key.id, json!({"spend_limit": "10"})).await;
        resp.assert_status_ok();
        let body: ApiKeyInfoResponse = resp.json();
        assert_eq!(window_spend_of(&body), Decimal::ZERO, "re-capping must not inherit prior spend");

        // reset_window without a cap is a 400.
        patch_key(&app, &user, key.id, json!({"spend_limit": null}))
            .await
            .assert_status_ok();
        patch_key(&app, &user, key.id, json!({"reset_window": true}))
            .await
            .assert_status(axum::http::StatusCode::BAD_REQUEST);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_patch_api_key_permissions_and_system_keys(pool: PgPool) {
        use crate::db::handlers::api_keys::ApiKeys;
        use crate::db::models::api_keys::ApiKeyPurpose;

        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let owner = create_test_user(&pool, Role::StandardUser).await;
        let other = create_test_user(&pool, Role::StandardUser).await;
        let admin = create_test_admin_user(&pool, Role::PlatformManager).await;
        let key = create_test_api_key_for_user(&pool, owner.id).await;

        // A stranger cannot update someone else's key.
        let auth = add_auth_headers(&other);
        app.patch(&format!("/admin/api/v1/users/{}/api-keys/{}", owner.id, key.id))
            .json(&json!({"spend_limit": "5"}))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await
            .assert_status(axum::http::StatusCode::FORBIDDEN);

        // A PlatformManager can.
        let auth = add_auth_headers(&admin);
        app.patch(&format!("/admin/api/v1/users/{}/api-keys/{}", owner.id, key.id))
            .json(&json!({"spend_limit": "5"}))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await
            .assert_status_ok();

        // System-managed keys are never PATCHable: the cap-scope child and the
        // shared hidden batch key both 404 even for their own user.
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = ApiKeys::new(&mut conn);
        let (_, child_id) = repo.get_or_create_child_hidden_key(key.id).await.unwrap();
        let (_, shared_id) = repo
            .get_or_create_hidden_key_with_id(owner.id, ApiKeyPurpose::Batch, owner.id)
            .await
            .unwrap();
        drop(conn);
        patch_key(&app, &owner, child_id, json!({"name": "nope"}))
            .await
            .assert_status(axum::http::StatusCode::NOT_FOUND);
        patch_key(&app, &owner, shared_id, json!({"name": "nope"}))
            .await
            .assert_status(axum::http::StatusCode::NOT_FOUND);
    }

    /// End-to-end acceptance path: a cap set through the real API, once
    /// exhausted, excludes the whole scope from the onwards key set on the
    /// next reload, and the proxy path answers with the explicit 402.
    #[sqlx::test]
    #[test_log::test]
    async fn test_capped_key_end_to_end_402(pool: PgPool) {
        use crate::config::RateLimitTiersConfig;
        use crate::db::handlers::{Credits, Tariffs};
        use crate::db::models::credits::{CreditTransactionCreateDBRequest, CreditTransactionType};
        use crate::db::models::tariffs::TariffCreateDBRequest;
        use onwards::auth::ConstantTimeString;

        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Model access + a PAID tariff (the cap gate, like the balance gate,
        // only bites on paid models) + healthy balance (so the enricher's
        // balance arm doesn't mask the cap arm).
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;
        let endpoint_id = create_test_endpoint(&pool, "cap-e2e-endpoint", user.id).await;
        let deployment_id = create_test_model(&pool, "cap-e2e-model-name", "cap-e2e-model", endpoint_id, user.id).await;
        add_deployment_to_group(&pool, deployment_id, group.id, user.id).await;
        let mut conn = pool.acquire().await.unwrap();
        Tariffs::new(&mut conn)
            .create(&TariffCreateDBRequest {
                deployed_model_id: deployment_id,
                name: "cap-e2e-tariff".to_string(),
                api_key_purpose: Some(crate::db::models::api_keys::ApiKeyPurpose::Realtime),
                input_price_per_token: rust_decimal::Decimal::new(1, 5),
                output_price_per_token: rust_decimal::Decimal::new(3, 5),
                valid_from: None,
                completion_window: None,
            })
            .await
            .unwrap();
        Credits::new(&mut conn)
            .create_transaction(&CreditTransactionCreateDBRequest {
                user_id: user.id,
                transaction_type: CreditTransactionType::AdminGrant,
                amount: rust_decimal::Decimal::from(100),
                source_id: uuid::Uuid::new_v4().to_string(),
                description: Some("credits".to_string()),
                fusillade_batch_id: None,
                api_key_id: None,
            })
            .await
            .unwrap();
        drop(conn);

        // Create the capped key through the real API.
        let auth = add_auth_headers(&user);
        let created: ApiKeyResponse = app
            .post("/admin/api/v1/users/current/api-keys")
            .json(&json!({"name": "e2e-capped", "spend_limit": "0.01"}))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await
            .json();
        let child_secret: String = sqlx::query_scalar("SELECT secret FROM api_keys WHERE parent_api_key_id = $1")
            .bind(created.id)
            .fetch_one(&pool)
            .await
            .unwrap();

        // Under the cap: both scope keys are in the paid pool.
        let tiers = RateLimitTiersConfig::default();
        let targets = crate::sync::onwards_config::load_targets_from_db(&pool, &[], false, &tiers)
            .await
            .unwrap();
        let has_key = |targets: &onwards::target::Targets, secret: &str| {
            let expected = ConstantTimeString::from(secret.to_string());
            targets
                .targets
                .get("cap-e2e-model")
                .is_some_and(|p| p.value().keys().is_some_and(|keys| keys.iter().any(|c| c == &expected)))
        };
        assert!(has_key(&targets, &created.key));
        assert!(has_key(&targets, &child_secret));

        // Exhaust the scope (as the batcher fold would) and reload: the whole
        // scope is yanked.
        sqlx::query("UPDATE api_key_spend_checkpoints SET window_spend = 0.02, total_spend = 0.02 WHERE api_key_id = $1")
            .bind(created.id)
            .execute(&pool)
            .await
            .unwrap();
        let targets = crate::sync::onwards_config::load_targets_from_db(&pool, &[], false, &tiers)
            .await
            .unwrap();
        assert!(!has_key(&targets, &created.key), "exhausted root must leave the paid pool");
        assert!(!has_key(&targets, &child_secret), "the child is yanked with its root");

        // And the proxy path answers the yanked key's 403 with the explicit
        // 402 (enrichment middleware, as wired on the /ai router).
        let router = axum::Router::new()
            .route(
                "/ai/v1/chat/completions",
                axum::routing::post(|| async {
                    axum::response::Response::builder()
                        .status(axum::http::StatusCode::FORBIDDEN)
                        .body(axum::body::Body::from("Forbidden"))
                        .unwrap()
                }),
            )
            .layer(axum::middleware::from_fn_with_state(
                pool.clone(),
                crate::error_enrichment::error_enrichment_middleware,
            ));
        let proxy = axum_test::TestServer::new(router).unwrap();
        let response = proxy
            .post("/ai/v1/chat/completions")
            .add_header("authorization", &format!("Bearer {}", created.key))
            .json(&json!({"model": "cap-e2e-model", "messages": [{"role": "user", "content": "hi"}]}))
            .await;
        response.assert_status(axum::http::StatusCode::PAYMENT_REQUIRED);
        let body = response.text();
        assert!(body.contains("spend_cap_exceeded"), "expected explicit cap code, got: {body}");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_api_key_for_self(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let api_key_data = json!({
            "name": "Test API Key",
            "description": "A test API key",
            "purpose": "realtime"
        });

        let response = app
            .post("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .json(&api_key_data)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);
        let api_key: ApiKeyResponse = response.json();
        assert_eq!(api_key.name, "Test API Key");
        assert_eq!(api_key.description, Some("A test API key".to_string()));
        assert!(api_key.key.starts_with("sk-"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_api_key_for_other_user_as_admin(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, regular_user.id, group.id).await;

        let api_key_data = json!({
            "name": "Admin Created Key",
            "description": "Created by admin for user",
            "purpose": "realtime"
        });

        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", regular_user.id))
            .add_header(&add_auth_headers(&admin_user)[0].0, &add_auth_headers(&admin_user)[0].1)
            .add_header(&add_auth_headers(&admin_user)[1].0, &add_auth_headers(&admin_user)[1].1)
            .json(&api_key_data)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);
        let api_key: ApiKeyResponse = response.json();
        assert_eq!(api_key.name, "Admin Created Key");
        assert!(api_key.key.starts_with("sk-"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_api_key_for_other_user_as_non_admin_forbidden(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        let api_key_data = json!({
            "name": "Forbidden Key",
            "description": "This should not work",
            "purpose": "realtime"
        });

        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", user2.id))
            .add_header(&add_auth_headers(&user1)[0].0, &add_auth_headers(&user1)[0].1)
            .add_header(&add_auth_headers(&user1)[1].0, &add_auth_headers(&user1)[1].1)
            .json(&api_key_data)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_user_api_keys(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;
        let api_key = create_test_api_key_for_user(&pool, user.id).await;

        let response = app
            .get("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(paginated.data.len(), 1);
        assert_eq!(paginated.total_count, 1);
        assert_eq!(paginated.data[0].name, api_key.name);
    }

    // Add new pagination test for the handler
    #[sqlx::test]
    #[test_log::test]
    async fn test_list_user_api_keys_with_pagination_query_params(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create multiple API keys
        for i in 1..=5 {
            let api_key_data = json!({
                "name": format!("Test API Key {}", i),
                "description": format!("Description for key {}", i),
                "purpose": "realtime"
            });

            app.post("/admin/api/v1/users/current/api-keys")
                .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
                .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
                .json(&api_key_data)
                .await
                .assert_status(axum::http::StatusCode::CREATED);
        }

        // Test with pagination parameters
        let response = app
            .get("/admin/api/v1/users/current/api-keys?skip=1&limit=2")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(paginated.data.len(), 2, "Should return exactly 2 items with limit=2");
        assert_eq!(paginated.total_count, 5, "Total count should be 5");
        assert_eq!(paginated.skip, 1);
        assert_eq!(paginated.limit, 2);

        // Test with no pagination parameters (should use defaults)
        let response = app
            .get("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(paginated.data.len(), 5, "Should return all items with default pagination");
        assert_eq!(paginated.total_count, 5);

        // Test with large limit (should be capped)
        let response = app
            .get("/admin/api/v1/users/current/api-keys?limit=9999")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(paginated.data.len(), 5, "Should return all items even with large limit");
        assert_eq!(paginated.total_count, 5);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_user_api_key_for_self(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;
        let api_key = create_test_api_key_for_user(&pool, user.id).await;

        let response = app
            .delete(&format!("/admin/api/v1/users/current/api-keys/{}", api_key.id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status(axum::http::StatusCode::NO_CONTENT);

        // Verify the API key was deleted by trying to list them
        let list_response = app
            .get("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        list_response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = list_response.json();
        assert_eq!(paginated.data.len(), 0);
        assert_eq!(paginated.total_count, 0);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_rejects_cap_scope_child_key(pool: PgPool) {
        use crate::db::handlers::api_keys::ApiKeys;
        use crate::db::handlers::repository::Repository;

        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let api_key = create_test_api_key_for_user(&pool, user.id).await;

        // Mint a cap-scope child for the visible key.
        let child_id = {
            let mut conn = pool.acquire().await.unwrap();
            let (_, child_id) = ApiKeys::new(&mut conn).get_or_create_child_hidden_key(api_key.id).await.unwrap();
            child_id
        };

        // Children are system-managed: direct deletion is rejected (404), even
        // though the child belongs to and was created by this user.
        let response = app
            .delete(&format!("/admin/api/v1/users/current/api-keys/{child_id}"))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        response.assert_status(axum::http::StatusCode::NOT_FOUND);

        // The child is still alive (and would still resolve for execution).
        let mut conn = pool.acquire().await.unwrap();
        assert!(ApiKeys::new(&mut conn).get_by_id(child_id).await.unwrap().is_some());

        // Deleting the parent is the only path that revokes it.
        let response = app
            .delete(&format!("/admin/api/v1/users/current/api-keys/{}", api_key.id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        response.assert_status(axum::http::StatusCode::NO_CONTENT);
        assert!(ApiKeys::new(&mut conn).get_by_id(child_id).await.unwrap().is_none());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_user_api_key_for_other_user_as_admin(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, regular_user.id, group.id).await;
        let api_key = create_test_api_key_for_user(&pool, regular_user.id).await;

        let response = app
            .delete(&format!("/admin/api/v1/users/{}/api-keys/{}", regular_user.id, api_key.id))
            .add_header(&add_auth_headers(&admin_user)[0].0, &add_auth_headers(&admin_user)[0].1)
            .add_header(&add_auth_headers(&admin_user)[1].0, &add_auth_headers(&admin_user)[1].1)
            .await;

        response.assert_status(axum::http::StatusCode::NO_CONTENT);

        // Verify the API key was deleted
        let list_response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", regular_user.id))
            .add_header(&add_auth_headers(&admin_user)[0].0, &add_auth_headers(&admin_user)[0].1)
            .add_header(&add_auth_headers(&admin_user)[1].0, &add_auth_headers(&admin_user)[1].1)
            .await;

        list_response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = list_response.json();
        assert_eq!(paginated.data.len(), 0);
        assert_eq!(paginated.total_count, 0);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_user_api_key_for_other_user_as_non_admin_forbidden(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user2.id, group.id).await;
        let api_key = create_test_api_key_for_user(&pool, user2.id).await;

        let response = app
            .delete(&format!("/admin/api/v1/users/{}/api-keys/{}", user2.id, api_key.id))
            .add_header(&add_auth_headers(&user1)[0].0, &add_auth_headers(&user1)[0].1)
            .add_header(&add_auth_headers(&user1)[1].0, &add_auth_headers(&user1)[1].1)
            .await;

        response.assert_status_forbidden();

        // Verify the API key still exists
        let list_response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", user2.id))
            .add_header(&add_auth_headers(&user2)[0].0, &add_auth_headers(&user2)[0].1)
            .add_header(&add_auth_headers(&user2)[1].0, &add_auth_headers(&user2)[1].1)
            .await;

        list_response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = list_response.json();
        assert_eq!(paginated.data.len(), 1);
        assert_eq!(paginated.total_count, 1);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_nonexistent_api_key_returns_not_found(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;
        let fake_api_key_id = uuid::Uuid::new_v4();

        let response = app
            .delete(&format!("/admin/api/v1/users/current/api-keys/{fake_api_key_id}"))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_not_found();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_api_key_belonging_to_different_user_returns_not_found(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user1.id, group.id).await;
        add_user_to_group(&pool, user2.id, group.id).await;

        // Create API key for user2
        let api_key = create_test_api_key_for_user(&pool, user2.id).await;

        // Try to delete user2's API key as user1 (using current endpoint)
        let response = app
            .delete(&format!("/admin/api/v1/users/current/api-keys/{}", api_key.id))
            .add_header(&add_auth_headers(&user1)[0].0, &add_auth_headers(&user1)[0].1)
            .add_header(&add_auth_headers(&user1)[1].0, &add_auth_headers(&user1)[1].1)
            .await;

        response.assert_status_not_found();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_api_keys_for_other_user_as_admin(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, regular_user.id, group.id).await;

        // Create multiple API keys for the regular user
        let api_key1 = create_test_api_key_for_user(&pool, regular_user.id).await;
        let api_key2 = create_test_api_key_for_user(&pool, regular_user.id).await;

        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", regular_user.id))
            .add_header(&add_auth_headers(&admin_user)[0].0, &add_auth_headers(&admin_user)[0].1)
            .add_header(&add_auth_headers(&admin_user)[1].0, &add_auth_headers(&admin_user)[1].1)
            .await;

        response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(paginated.data.len(), 2);
        assert_eq!(paginated.total_count, 2);

        // Verify we got the correct API keys
        let returned_ids: Vec<_> = paginated.data.iter().map(|k| k.id).collect();
        assert!(returned_ids.contains(&api_key1.id));
        assert!(returned_ids.contains(&api_key2.id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_api_keys_for_other_user_as_non_admin_forbidden(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user2.id, group.id).await;
        create_test_api_key_for_user(&pool, user2.id).await;

        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", user2.id))
            .add_header(&add_auth_headers(&user1)[0].0, &add_auth_headers(&user1)[0].1)
            .add_header(&add_auth_headers(&user1)[1].0, &add_auth_headers(&user1)[1].1)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_api_key_for_other_user_as_admin(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, regular_user.id, group.id).await;
        let api_key = create_test_api_key_for_user(&pool, regular_user.id).await;

        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys/{}", regular_user.id, api_key.id))
            .add_header(&add_auth_headers(&admin_user)[0].0, &add_auth_headers(&admin_user)[0].1)
            .add_header(&add_auth_headers(&admin_user)[1].0, &add_auth_headers(&admin_user)[1].1)
            .await;

        response.assert_status_ok();
        let returned_key: ApiKeyInfoResponse = response.json();
        assert_eq!(returned_key.id, api_key.id);
        assert_eq!(returned_key.name, api_key.name);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_api_key_for_other_user_as_non_admin_forbidden(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user2.id, group.id).await;
        let api_key = create_test_api_key_for_user(&pool, user2.id).await;

        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys/{}", user2.id, api_key.id))
            .add_header(&add_auth_headers(&user1)[0].0, &add_auth_headers(&user1)[0].1)
            .add_header(&add_auth_headers(&user1)[1].0, &add_auth_headers(&user1)[1].1)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_request_viewer_api_key_permissions(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        // Note: create_test_user automatically adds StandardUser role
        // So request_viewer actually has [RequestViewer, StandardUser] roles
        let request_viewer = create_test_user(&pool, Role::RequestViewer).await;
        let other_user = create_test_user(&pool, Role::StandardUser).await;

        // RequestViewer (with StandardUser) CAN create API keys for themselves
        let api_key_data = json!({
            "name": "RequestViewer Key",
            "description": "Should work - StandardUser can manage own keys",
            "purpose": "realtime"
        });

        let response = app
            .post("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&request_viewer)[0].0, &add_auth_headers(&request_viewer)[0].1)
            .add_header(&add_auth_headers(&request_viewer)[1].0, &add_auth_headers(&request_viewer)[1].1)
            .json(&api_key_data)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);

        // RequestViewer (with StandardUser) CAN list their own API keys
        let response = app
            .get("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&request_viewer)[0].0, &add_auth_headers(&request_viewer)[0].1)
            .add_header(&add_auth_headers(&request_viewer)[1].0, &add_auth_headers(&request_viewer)[1].1)
            .await;

        response.assert_status_ok();

        // RequestViewer should NOT be able to list other users' API keys (no PlatformManager role)
        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", other_user.id))
            .add_header(&add_auth_headers(&request_viewer)[0].0, &add_auth_headers(&request_viewer)[0].1)
            .add_header(&add_auth_headers(&request_viewer)[1].0, &add_auth_headers(&request_viewer)[1].1)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_multi_role_user_api_key_permissions(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;

        // Create a user with both StandardUser and RequestViewer roles
        let multi_role_user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::RequestViewer]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, multi_role_user.id, group.id).await;

        // Should be able to create API keys (from StandardUser role)
        let api_key_data = json!({
            "name": "Multi Role Key",
            "description": "Should work due to StandardUser role",
            "purpose": "realtime"
        });

        let response = app
            .post("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&multi_role_user)[0].0, &add_auth_headers(&multi_role_user)[0].1)
            .add_header(&add_auth_headers(&multi_role_user)[1].0, &add_auth_headers(&multi_role_user)[1].1)
            .json(&api_key_data)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);

        // Should be able to list their own API keys (from StandardUser role)
        let response = app
            .get("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&multi_role_user)[0].0, &add_auth_headers(&multi_role_user)[0].1)
            .add_header(&add_auth_headers(&multi_role_user)[1].0, &add_auth_headers(&multi_role_user)[1].1)
            .await;

        response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(paginated.data.len(), 1);
        assert_eq!(paginated.total_count, 1);
        assert_eq!(paginated.data[0].name, "Multi Role Key");

        // Should be able to get specific API key
        let api_key_id = paginated.data[0].id;
        let response = app
            .get(&format!("/admin/api/v1/users/current/api-keys/{api_key_id}"))
            .add_header(&add_auth_headers(&multi_role_user)[0].0, &add_auth_headers(&multi_role_user)[0].1)
            .add_header(&add_auth_headers(&multi_role_user)[1].0, &add_auth_headers(&multi_role_user)[1].1)
            .await;

        response.assert_status_ok();

        // Should be able to delete their own API keys
        let response = app
            .delete(&format!("/admin/api/v1/users/current/api-keys/{api_key_id}"))
            .add_header(&add_auth_headers(&multi_role_user)[0].0, &add_auth_headers(&multi_role_user)[0].1)
            .add_header(&add_auth_headers(&multi_role_user)[1].0, &add_auth_headers(&multi_role_user)[1].1)
            .await;

        response.assert_status(axum::http::StatusCode::NO_CONTENT);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_full_api_key_access(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let platform_manager = create_test_user(&pool, Role::PlatformManager).await;
        let standard_user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, standard_user.id, group.id).await;

        // Platform manager should be able to create API keys for other users
        let api_key_data = json!({
            "name": "Manager Created Key",
            "description": "Created by platform manager",
            "purpose": "realtime"
        });

        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", standard_user.id))
            .add_header(&add_auth_headers(&platform_manager)[0].0, &add_auth_headers(&platform_manager)[0].1)
            .add_header(&add_auth_headers(&platform_manager)[1].0, &add_auth_headers(&platform_manager)[1].1)
            .json(&api_key_data)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);

        // Platform manager should be able to list all users' API keys
        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", standard_user.id))
            .add_header(&add_auth_headers(&platform_manager)[0].0, &add_auth_headers(&platform_manager)[0].1)
            .add_header(&add_auth_headers(&platform_manager)[1].0, &add_auth_headers(&platform_manager)[1].1)
            .await;

        response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(paginated.data.len(), 1);
        assert_eq!(paginated.total_count, 1);

        // Platform manager should be able to get specific API keys for other users
        let api_key_id = paginated.data[0].id;
        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys/{}", standard_user.id, api_key_id))
            .add_header(&add_auth_headers(&platform_manager)[0].0, &add_auth_headers(&platform_manager)[0].1)
            .add_header(&add_auth_headers(&platform_manager)[1].0, &add_auth_headers(&platform_manager)[1].1)
            .await;

        response.assert_status_ok();

        // The target user can also see the key created on their behalf
        let response = app
            .get("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&standard_user)[0].0, &add_auth_headers(&standard_user)[0].1)
            .add_header(&add_auth_headers(&standard_user)[1].0, &add_auth_headers(&standard_user)[1].1)
            .await;

        response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(paginated.data.len(), 1);
        assert_eq!(paginated.data[0].name, "Manager Created Key");

        // Platform manager should be able to delete other users' API keys
        let response = app
            .delete(&format!("/admin/api/v1/users/{}/api-keys/{}", standard_user.id, api_key_id))
            .add_header(&add_auth_headers(&platform_manager)[0].0, &add_auth_headers(&platform_manager)[0].1)
            .add_header(&add_auth_headers(&platform_manager)[1].0, &add_auth_headers(&platform_manager)[1].1)
            .await;

        response.assert_status(axum::http::StatusCode::NO_CONTENT);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_api_key_isolation_between_users(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user1.id, group.id).await;
        add_user_to_group(&pool, user2.id, group.id).await;

        // Create API keys for both users
        let api_key1 = create_test_api_key_for_user(&pool, user1.id).await;
        let api_key2 = create_test_api_key_for_user(&pool, user2.id).await;

        // User1 should only see their own API keys
        let response = app
            .get("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&user1)[0].0, &add_auth_headers(&user1)[0].1)
            .add_header(&add_auth_headers(&user1)[1].0, &add_auth_headers(&user1)[1].1)
            .await;

        response.assert_status_ok();
        let user1_paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(user1_paginated.data.len(), 1);
        assert_eq!(user1_paginated.total_count, 1);
        assert_eq!(user1_paginated.data[0].id, api_key1.id);

        // User2 should only see their own API keys
        let response = app
            .get("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&user2)[0].0, &add_auth_headers(&user2)[0].1)
            .add_header(&add_auth_headers(&user2)[1].0, &add_auth_headers(&user2)[1].1)
            .await;

        response.assert_status_ok();
        let user2_paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(user2_paginated.data.len(), 1);
        assert_eq!(user2_paginated.total_count, 1);
        assert_eq!(user2_paginated.data[0].id, api_key2.id);

        // User1 should not be able to access user2's specific API key
        let response = app
            .get(&format!("/admin/api/v1/users/current/api-keys/{}", api_key2.id))
            .add_header(&add_auth_headers(&user1)[0].0, &add_auth_headers(&user1)[0].1)
            .add_header(&add_auth_headers(&user1)[1].0, &add_auth_headers(&user1)[1].1)
            .await;

        response.assert_status_not_found(); // 404 because the key doesn't belong to user1
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_error_messages_are_user_friendly(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // Try to access another user's API keys - should get user-friendly error
        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", user2.id))
            .add_header(&add_auth_headers(&user1)[0].0, &add_auth_headers(&user1)[0].1)
            .add_header(&add_auth_headers(&user1)[1].0, &add_auth_headers(&user1)[1].1)
            .await;

        response.assert_status_forbidden();
        let body = response.text();

        // Should show generic "Read" not "ReadAll" or "ReadOwn"
        assert!(body.contains("Insufficient permissions to Read"));
        assert!(!body.contains("ReadAll"));
        assert!(!body.contains("ReadOwn"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_get_specific_api_key_for_self(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;
        let api_key = create_test_api_key_for_user(&pool, user.id).await;

        let response = app
            .get(&format!("/admin/api/v1/users/current/api-keys/{}", api_key.id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let returned_key: ApiKeyInfoResponse = response.json();
        assert_eq!(returned_key.id, api_key.id);
        assert_eq!(returned_key.name, api_key.name);
        assert_eq!(returned_key.description, api_key.description);
        // ApiKeyInfoResponse intentionally does not have a key field (security feature)
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_api_key_creation_returns_key_value_only_once(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create API key - should return the actual key value
        let api_key_data = json!({
            "name": "Test Key for Security",
            "description": "Testing key exposure",
            "purpose": "realtime"
        });

        let create_response = app
            .post("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .json(&api_key_data)
            .await;

        create_response.assert_status(axum::http::StatusCode::CREATED);
        let created_key: ApiKeyResponse = create_response.json();

        // Should have the actual key value
        assert!(created_key.key.starts_with("sk-"));
        assert!(created_key.key.len() > 10);

        // List API keys - should NOT return key values (uses ApiKeyInfoResponse)
        let list_response = app
            .get("/admin/api/v1/users/current/api-keys")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        list_response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = list_response.json();
        assert_eq!(paginated.data.len(), 1);
        assert_eq!(paginated.total_count, 1);

        // ApiKeyInfoResponse doesn't have a key field - this is the security feature

        // Get specific API key - should NOT return key value (uses ApiKeyInfoResponse)
        let get_response = app
            .get(&format!("/admin/api/v1/users/current/api-keys/{}", created_key.id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        get_response.assert_status_ok();
        let retrieved_key: ApiKeyInfoResponse = get_response.json();

        // ApiKeyInfoResponse doesn't have a key field - this is the security feature
        assert_eq!(retrieved_key.id, created_key.id);
        assert_eq!(retrieved_key.name, created_key.name);
    }

    // ── Organization API key tests ────────────────────────────────────────

    #[sqlx::test]
    #[test_log::test]
    async fn test_org_member_can_create_api_key(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let bob = create_test_user(&pool, Role::StandardUser).await;
        let org = create_test_org(&pool, alice.id).await;
        add_org_member(&pool, org.id, bob.id, "member").await;

        // Bob (member, not admin) creates a key for the org
        let api_key_data = json!({
            "name": "Bob Org Key",
            "description": "Created by member",
            "purpose": "realtime"
        });

        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&bob)[0].0, &add_auth_headers(&bob)[0].1)
            .add_header(&add_auth_headers(&bob)[1].0, &add_auth_headers(&bob)[1].1)
            .json(&api_key_data)
            .await;

        response.assert_status(axum::http::StatusCode::CREATED);
        let api_key: ApiKeyResponse = response.json();
        assert_eq!(api_key.name, "Bob Org Key");
        assert!(api_key.key.starts_with("sk-"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_non_member_cannot_create_org_api_key(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let outsider = create_test_user(&pool, Role::StandardUser).await;
        let org = create_test_org(&pool, alice.id).await;

        // Outsider (not a member) tries to create a key for the org
        let api_key_data = json!({
            "name": "Outsider Key",
            "purpose": "realtime"
        });

        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&outsider)[0].0, &add_auth_headers(&outsider)[0].1)
            .add_header(&add_auth_headers(&outsider)[1].0, &add_auth_headers(&outsider)[1].1)
            .json(&api_key_data)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_org_member_list_only_own_keys(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let bob = create_test_user(&pool, Role::StandardUser).await;
        let org = create_test_org(&pool, alice.id).await;
        add_org_member(&pool, org.id, bob.id, "member").await;

        // Alice (owner) creates a key for the org
        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&alice)[0].0, &add_auth_headers(&alice)[0].1)
            .add_header(&add_auth_headers(&alice)[1].0, &add_auth_headers(&alice)[1].1)
            .json(&json!({"name": "Alice Key", "purpose": "realtime"}))
            .await;
        response.assert_status(axum::http::StatusCode::CREATED);

        // Bob (member) creates a key for the org
        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&bob)[0].0, &add_auth_headers(&bob)[0].1)
            .add_header(&add_auth_headers(&bob)[1].0, &add_auth_headers(&bob)[1].1)
            .json(&json!({"name": "Bob Key", "purpose": "realtime"}))
            .await;
        response.assert_status(axum::http::StatusCode::CREATED);

        // Alice lists org keys → should only see her own key
        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&alice)[0].0, &add_auth_headers(&alice)[0].1)
            .add_header(&add_auth_headers(&alice)[1].0, &add_auth_headers(&alice)[1].1)
            .await;
        response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(paginated.data.len(), 1);
        assert_eq!(paginated.data[0].name, "Alice Key");

        // Bob lists org keys → should only see his own key
        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&bob)[0].0, &add_auth_headers(&bob)[0].1)
            .add_header(&add_auth_headers(&bob)[1].0, &add_auth_headers(&bob)[1].1)
            .await;
        response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(paginated.data.len(), 1);
        assert_eq!(paginated.data[0].name, "Bob Key");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_org_member_cannot_get_other_members_key(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let bob = create_test_user(&pool, Role::StandardUser).await;
        let org = create_test_org(&pool, alice.id).await;
        add_org_member(&pool, org.id, bob.id, "member").await;

        // Bob creates a key
        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&bob)[0].0, &add_auth_headers(&bob)[0].1)
            .add_header(&add_auth_headers(&bob)[1].0, &add_auth_headers(&bob)[1].1)
            .json(&json!({"name": "Bob Key", "purpose": "realtime"}))
            .await;
        response.assert_status(axum::http::StatusCode::CREATED);
        let bob_key: ApiKeyResponse = response.json();

        // Alice tries to get Bob's key by ID → should get 404
        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys/{}", org.id, bob_key.id))
            .add_header(&add_auth_headers(&alice)[0].0, &add_auth_headers(&alice)[0].1)
            .add_header(&add_auth_headers(&alice)[1].0, &add_auth_headers(&alice)[1].1)
            .await;
        response.assert_status_not_found();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_org_member_cannot_delete_other_members_key(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let bob = create_test_user(&pool, Role::StandardUser).await;
        let org = create_test_org(&pool, alice.id).await;
        add_org_member(&pool, org.id, bob.id, "member").await;

        // Bob creates a key
        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&bob)[0].0, &add_auth_headers(&bob)[0].1)
            .add_header(&add_auth_headers(&bob)[1].0, &add_auth_headers(&bob)[1].1)
            .json(&json!({"name": "Bob Key", "purpose": "realtime"}))
            .await;
        response.assert_status(axum::http::StatusCode::CREATED);
        let bob_key: ApiKeyResponse = response.json();

        // Alice tries to delete Bob's key → should get 404
        let response = app
            .delete(&format!("/admin/api/v1/users/{}/api-keys/{}", org.id, bob_key.id))
            .add_header(&add_auth_headers(&alice)[0].0, &add_auth_headers(&alice)[0].1)
            .add_header(&add_auth_headers(&alice)[1].0, &add_auth_headers(&alice)[1].1)
            .await;
        response.assert_status_not_found();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_org_member_can_delete_own_key(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let bob = create_test_user(&pool, Role::StandardUser).await;
        let org = create_test_org(&pool, alice.id).await;
        add_org_member(&pool, org.id, bob.id, "member").await;

        // Bob creates a key
        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&bob)[0].0, &add_auth_headers(&bob)[0].1)
            .add_header(&add_auth_headers(&bob)[1].0, &add_auth_headers(&bob)[1].1)
            .json(&json!({"name": "Bob Key", "purpose": "realtime"}))
            .await;
        response.assert_status(axum::http::StatusCode::CREATED);
        let bob_key: ApiKeyResponse = response.json();

        // Bob deletes his own key → should succeed
        let response = app
            .delete(&format!("/admin/api/v1/users/{}/api-keys/{}", org.id, bob_key.id))
            .add_header(&add_auth_headers(&bob)[0].0, &add_auth_headers(&bob)[0].1)
            .add_header(&add_auth_headers(&bob)[1].0, &add_auth_headers(&bob)[1].1)
            .await;
        response.assert_status(axum::http::StatusCode::NO_CONTENT);

        // Verify key is gone from Bob's list
        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&bob)[0].0, &add_auth_headers(&bob)[0].1)
            .add_header(&add_auth_headers(&bob)[1].0, &add_auth_headers(&bob)[1].1)
            .await;
        response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(paginated.data.len(), 0);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_platform_manager_sees_all_org_keys(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let bob = create_test_user(&pool, Role::StandardUser).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let org = create_test_org(&pool, alice.id).await;
        add_org_member(&pool, org.id, bob.id, "member").await;

        // Alice creates a key
        app.post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&alice)[0].0, &add_auth_headers(&alice)[0].1)
            .add_header(&add_auth_headers(&alice)[1].0, &add_auth_headers(&alice)[1].1)
            .json(&json!({"name": "Alice Key", "purpose": "realtime"}))
            .await
            .assert_status(axum::http::StatusCode::CREATED);

        // Bob creates a key
        app.post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&bob)[0].0, &add_auth_headers(&bob)[0].1)
            .add_header(&add_auth_headers(&bob)[1].0, &add_auth_headers(&bob)[1].1)
            .json(&json!({"name": "Bob Key", "purpose": "realtime"}))
            .await
            .assert_status(axum::http::StatusCode::CREATED);

        // PlatformManager lists org keys → should see all keys (bypasses created_by filter)
        let response = app
            .get(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&pm)[0].0, &add_auth_headers(&pm)[0].1)
            .add_header(&add_auth_headers(&pm)[1].0, &add_auth_headers(&pm)[1].1)
            .await;
        response.assert_status_ok();
        let paginated: PaginatedResponse<ApiKeyInfoResponse> = response.json();
        assert_eq!(paginated.data.len(), 2);
    }

    // ── created_by attribution tests ──────────────────────────────────────

    #[sqlx::test]
    #[test_log::test]
    async fn test_org_member_key_created_by_is_member_not_org(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let org = create_test_org(&pool, alice.id).await;

        // Alice (org owner) creates a key for the org
        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&alice)[0].0, &add_auth_headers(&alice)[0].1)
            .add_header(&add_auth_headers(&alice)[1].0, &add_auth_headers(&alice)[1].1)
            .json(&json!({"name": "Alice Org Key", "purpose": "realtime"}))
            .await;
        response.assert_status(axum::http::StatusCode::CREATED);
        let key: ApiKeyResponse = response.json();

        // created_by should be Alice, not the org
        assert_eq!(key.created_by, alice.id, "created_by should be the member, not the org");
        assert_eq!(key.user_id, org.id, "user_id should be the org");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_pm_org_member_key_created_by_is_pm_not_org(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let org = create_test_org(&pool, pm.id).await;

        // PM who is also an org owner creates a key for the org
        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&pm)[0].0, &add_auth_headers(&pm)[0].1)
            .add_header(&add_auth_headers(&pm)[1].0, &add_auth_headers(&pm)[1].1)
            .json(&json!({"name": "PM Org Key", "purpose": "realtime"}))
            .await;
        response.assert_status(axum::http::StatusCode::CREATED);
        let key: ApiKeyResponse = response.json();

        // created_by should be the PM, not the org — this is the original bug regression test
        assert_eq!(key.created_by, pm.id, "created_by should be the PM, not the org");
        assert_eq!(key.user_id, org.id, "user_id should be the org");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_pm_creates_org_key_with_member_id(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let org = create_test_org(&pool, alice.id).await;

        // PM creates a key for the org attributed to Alice via member_id
        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&pm)[0].0, &add_auth_headers(&pm)[0].1)
            .add_header(&add_auth_headers(&pm)[1].0, &add_auth_headers(&pm)[1].1)
            .json(&json!({"name": "Attributed Key", "purpose": "realtime", "member_id": alice.id}))
            .await;
        response.assert_status(axum::http::StatusCode::CREATED);
        let key: ApiKeyResponse = response.json();

        // created_by should be Alice (the specified member), not the PM
        assert_eq!(key.created_by, alice.id, "created_by should be the specified member_id");
        assert_eq!(key.user_id, org.id, "user_id should be the org");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_pm_member_id_rejected_for_non_org_target(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, alice.id, group.id).await;

        // PM tries to use member_id when creating a key for an individual user
        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", alice.id))
            .add_header(&add_auth_headers(&pm)[0].0, &add_auth_headers(&pm)[0].1)
            .add_header(&add_auth_headers(&pm)[1].0, &add_auth_headers(&pm)[1].1)
            .json(&json!({"name": "Bad Key", "purpose": "realtime", "member_id": pm.id}))
            .await;
        response.assert_status_bad_request();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_pm_member_id_rejected_for_non_member(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let outsider = create_test_user(&pool, Role::StandardUser).await;
        let org = create_test_org(&pool, alice.id).await;

        // PM tries to attribute key to a user who is not an org member
        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&pm)[0].0, &add_auth_headers(&pm)[0].1)
            .add_header(&add_auth_headers(&pm)[1].0, &add_auth_headers(&pm)[1].1)
            .json(&json!({"name": "Bad Key", "purpose": "realtime", "member_id": outsider.id}))
            .await;
        response.assert_status_bad_request();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_non_pm_cannot_use_member_id(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let bob = create_test_user(&pool, Role::StandardUser).await;
        let org = create_test_org(&pool, alice.id).await;
        add_org_member(&pool, org.id, bob.id, "member").await;

        // Alice (org owner, not a PM) tries to use member_id
        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", org.id))
            .add_header(&add_auth_headers(&alice)[0].0, &add_auth_headers(&alice)[0].1)
            .add_header(&add_auth_headers(&alice)[1].0, &add_auth_headers(&alice)[1].1)
            .json(&json!({"name": "Bad Key", "purpose": "realtime", "member_id": bob.id}))
            .await;
        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_pm_creates_key_for_individual_user_created_by_is_target(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let pm = create_test_admin_user(&pool, Role::PlatformManager).await;
        let alice = create_test_user(&pool, Role::StandardUser).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, alice.id, group.id).await;

        // PM creates a personal key for Alice (not an org)
        let response = app
            .post(&format!("/admin/api/v1/users/{}/api-keys", alice.id))
            .add_header(&add_auth_headers(&pm)[0].0, &add_auth_headers(&pm)[0].1)
            .add_header(&add_auth_headers(&pm)[1].0, &add_auth_headers(&pm)[1].1)
            .json(&json!({"name": "PM For Alice", "purpose": "realtime"}))
            .await;
        response.assert_status(axum::http::StatusCode::CREATED);
        let key: ApiKeyResponse = response.json();

        // created_by should be Alice (the target individual) so she can see it
        assert_eq!(key.created_by, alice.id, "created_by should be the target user for individual keys");
        assert_eq!(key.user_id, alice.id, "user_id should be the target user");
    }
}
