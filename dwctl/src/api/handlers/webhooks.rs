//! HTTP handlers for webhook management endpoints.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::Json,
};
use sqlx_pool_router::PoolProvider;
use tracing::instrument;

use crate::{
    AppState,
    api::models::webhooks::{
        UserWebhookPathParams, WebhookCreate, WebhookPathParams, WebhookResponse, WebhookTestResponse, WebhookUpdate,
        WebhookWithSecretResponse,
    },
    auth::permissions,
    db::handlers::Webhooks,
    db::models::webhooks::{WebhookCreateDBRequest, WebhookUpdateDBRequest},
    errors::{Error, Result},
    types::{Operation, Permission, Resource, UserId},
    webhooks::{WebhookEvent, WebhookEventType, signing},
};

/// List all webhooks for a user.
#[utoipa::path(
    get,
    path = "/users/{user_id}/webhooks",
    tag = "webhooks",
    summary = "List webhooks",
    description = "List all webhooks for a user. Users can list their own webhooks; admins can list any user's webhooks.",
    params(
        ("user_id" = uuid::Uuid, Path, description = "User ID"),
    ),
    responses(
        (status = 200, description = "List of webhooks", body = [WebhookResponse]),
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
#[instrument(skip_all)]
pub async fn list_webhooks<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(params): Path<UserWebhookPathParams>,
    current_user: crate::api::models::users::CurrentUser,
) -> Result<Json<Vec<WebhookResponse>>> {
    let target_user_id: UserId = params.user_id;

    // Check permissions: can read all webhooks OR read own webhooks
    let can_read_all = permissions::has_permission(&current_user, Resource::Webhooks, Operation::ReadAll);
    let can_read_own =
        target_user_id == current_user.id && permissions::has_permission(&current_user, Resource::Webhooks, Operation::ReadOwn);

    if !can_read_all && !can_read_own {
        return Err(Error::InsufficientPermissions {
            required: Permission::Any(vec![
                Permission::Allow(Resource::Webhooks, Operation::ReadAll),
                Permission::Allow(Resource::Webhooks, Operation::ReadOwn),
            ]),
            action: Operation::ReadAll,
            resource: format!("webhooks for user {}", target_user_id),
        });
    }

    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Webhooks::new(&mut conn);

    let webhooks = repo.list_by_user(target_user_id).await?;
    let responses: Vec<WebhookResponse> = webhooks.into_iter().map(Into::into).collect();

    Ok(Json(responses))
}

/// Create a new webhook for a user.
#[utoipa::path(
    post,
    path = "/users/{user_id}/webhooks",
    tag = "webhooks",
    summary = "Create webhook",
    description = "Create a new webhook for a user. Returns the secret which is only shown once.",
    params(
        ("user_id" = uuid::Uuid, Path, description = "User ID"),
    ),
    request_body = WebhookCreate,
    responses(
        (status = 201, description = "Webhook created", body = WebhookWithSecretResponse),
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
#[instrument(skip_all)]
pub async fn create_webhook<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(params): Path<UserWebhookPathParams>,
    current_user: crate::api::models::users::CurrentUser,
    Json(request): Json<WebhookCreate>,
) -> Result<(StatusCode, Json<WebhookWithSecretResponse>)> {
    let target_user_id: UserId = params.user_id;

    // Check permissions: can create all webhooks OR create own webhooks
    let can_create_all = permissions::has_permission(&current_user, Resource::Webhooks, Operation::CreateAll);
    let can_create_own =
        target_user_id == current_user.id && permissions::has_permission(&current_user, Resource::Webhooks, Operation::CreateOwn);

    if !can_create_all && !can_create_own {
        return Err(Error::InsufficientPermissions {
            required: Permission::Any(vec![
                Permission::Allow(Resource::Webhooks, Operation::CreateAll),
                Permission::Allow(Resource::Webhooks, Operation::CreateOwn),
            ]),
            action: Operation::CreateAll,
            resource: format!("webhooks for user {}", target_user_id),
        });
    }

    // Validate URL is HTTPS
    if !request.url.starts_with("https://") {
        return Err(Error::BadRequest {
            message: "Webhook URL must use HTTPS".to_string(),
        });
    }

    // Validate event types if provided
    if let Some(ref event_types) = request.event_types {
        for event_type in event_types {
            if event_type.parse::<WebhookEventType>().is_err() {
                return Err(Error::BadRequest {
                    message: format!(
                        "Invalid event type: {}. Valid types are: batch.completed, batch.failed, batch.cancelled",
                        event_type
                    ),
                });
            }
        }
    }

    // Generate secret
    let secret = signing::generate_secret();

    let mut tx = state.db.write().begin().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Webhooks::new(&mut tx);

    let db_request = WebhookCreateDBRequest {
        user_id: target_user_id,
        url: request.url,
        secret,
        event_types: request.event_types,
        description: request.description,
    };

    let webhook = repo.create(&db_request).await?;
    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    Ok((StatusCode::CREATED, Json(webhook.into())))
}

/// Get a specific webhook.
#[utoipa::path(
    get,
    path = "/users/{user_id}/webhooks/{webhook_id}",
    tag = "webhooks",
    summary = "Get webhook",
    description = "Get a specific webhook. Secret is not included in the response.",
    params(
        ("user_id" = uuid::Uuid, Path, description = "User ID"),
        ("webhook_id" = uuid::Uuid, Path, description = "Webhook ID"),
    ),
    responses(
        (status = 200, description = "Webhook details", body = WebhookResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Webhook not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[instrument(skip_all)]
pub async fn get_webhook<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(params): Path<WebhookPathParams>,
    current_user: crate::api::models::users::CurrentUser,
) -> Result<Json<WebhookResponse>> {
    let target_user_id: UserId = params.user_id;

    // Check permissions
    let can_read_all = permissions::has_permission(&current_user, Resource::Webhooks, Operation::ReadAll);
    let can_read_own =
        target_user_id == current_user.id && permissions::has_permission(&current_user, Resource::Webhooks, Operation::ReadOwn);

    if !can_read_all && !can_read_own {
        return Err(Error::InsufficientPermissions {
            required: Permission::Any(vec![
                Permission::Allow(Resource::Webhooks, Operation::ReadAll),
                Permission::Allow(Resource::Webhooks, Operation::ReadOwn),
            ]),
            action: Operation::ReadAll,
            resource: format!("webhook {}", params.webhook_id),
        });
    }

    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Webhooks::new(&mut conn);

    let webhook = repo.get_by_id(params.webhook_id).await?.ok_or_else(|| Error::NotFound {
        resource: "Webhook".to_string(),
        id: params.webhook_id.to_string(),
    })?;

    // Verify the webhook belongs to the specified user
    if webhook.user_id != target_user_id {
        return Err(Error::NotFound {
            resource: "Webhook".to_string(),
            id: params.webhook_id.to_string(),
        });
    }

    Ok(Json(webhook.into()))
}

/// Update a webhook.
#[utoipa::path(
    patch,
    path = "/users/{user_id}/webhooks/{webhook_id}",
    tag = "webhooks",
    summary = "Update webhook",
    description = "Update a webhook's URL, enabled status, event types, or description.",
    params(
        ("user_id" = uuid::Uuid, Path, description = "User ID"),
        ("webhook_id" = uuid::Uuid, Path, description = "Webhook ID"),
    ),
    request_body = WebhookUpdate,
    responses(
        (status = 200, description = "Webhook updated", body = WebhookResponse),
        (status = 400, description = "Bad request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Webhook not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[instrument(skip_all)]
pub async fn update_webhook<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(params): Path<WebhookPathParams>,
    current_user: crate::api::models::users::CurrentUser,
    Json(request): Json<WebhookUpdate>,
) -> Result<Json<WebhookResponse>> {
    let target_user_id: UserId = params.user_id;

    // Check permissions
    let can_update_all = permissions::has_permission(&current_user, Resource::Webhooks, Operation::UpdateAll);
    let can_update_own =
        target_user_id == current_user.id && permissions::has_permission(&current_user, Resource::Webhooks, Operation::UpdateOwn);

    if !can_update_all && !can_update_own {
        return Err(Error::InsufficientPermissions {
            required: Permission::Any(vec![
                Permission::Allow(Resource::Webhooks, Operation::UpdateAll),
                Permission::Allow(Resource::Webhooks, Operation::UpdateOwn),
            ]),
            action: Operation::UpdateAll,
            resource: format!("webhook {}", params.webhook_id),
        });
    }

    // Validate URL if provided
    if let Some(ref url) = request.url
        && !url.starts_with("https://")
    {
        return Err(Error::BadRequest {
            message: "Webhook URL must use HTTPS".to_string(),
        });
    }

    // Validate event types if provided
    if let Some(Some(ref event_types)) = request.event_types {
        for event_type in event_types {
            if event_type.parse::<WebhookEventType>().is_err() {
                return Err(Error::BadRequest {
                    message: format!(
                        "Invalid event type: {}. Valid types are: batch.completed, batch.failed, batch.cancelled",
                        event_type
                    ),
                });
            }
        }
    }

    let mut tx = state.db.write().begin().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Webhooks::new(&mut tx);

    // First verify the webhook exists and belongs to the user
    let existing = repo.get_by_id(params.webhook_id).await?.ok_or_else(|| Error::NotFound {
        resource: "Webhook".to_string(),
        id: params.webhook_id.to_string(),
    })?;

    if existing.user_id != target_user_id {
        return Err(Error::NotFound {
            resource: "Webhook".to_string(),
            id: params.webhook_id.to_string(),
        });
    }

    let db_request = WebhookUpdateDBRequest {
        url: request.url,
        enabled: request.enabled,
        event_types: request.event_types,
        description: request.description,
    };

    let webhook = repo.update(params.webhook_id, &db_request).await?.ok_or_else(|| Error::NotFound {
        resource: "Webhook".to_string(),
        id: params.webhook_id.to_string(),
    })?;

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    Ok(Json(webhook.into()))
}

/// Delete a webhook.
#[utoipa::path(
    delete,
    path = "/users/{user_id}/webhooks/{webhook_id}",
    tag = "webhooks",
    summary = "Delete webhook",
    description = "Delete a webhook.",
    params(
        ("user_id" = uuid::Uuid, Path, description = "User ID"),
        ("webhook_id" = uuid::Uuid, Path, description = "Webhook ID"),
    ),
    responses(
        (status = 204, description = "Webhook deleted"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Webhook not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[instrument(skip_all)]
pub async fn delete_webhook<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(params): Path<WebhookPathParams>,
    current_user: crate::api::models::users::CurrentUser,
) -> Result<StatusCode> {
    let target_user_id: UserId = params.user_id;

    // Check permissions
    let can_delete_all = permissions::has_permission(&current_user, Resource::Webhooks, Operation::DeleteAll);
    let can_delete_own =
        target_user_id == current_user.id && permissions::has_permission(&current_user, Resource::Webhooks, Operation::DeleteOwn);

    if !can_delete_all && !can_delete_own {
        return Err(Error::InsufficientPermissions {
            required: Permission::Any(vec![
                Permission::Allow(Resource::Webhooks, Operation::DeleteAll),
                Permission::Allow(Resource::Webhooks, Operation::DeleteOwn),
            ]),
            action: Operation::DeleteAll,
            resource: format!("webhook {}", params.webhook_id),
        });
    }

    let mut tx = state.db.write().begin().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Webhooks::new(&mut tx);

    // First verify the webhook exists and belongs to the user
    let existing = repo.get_by_id(params.webhook_id).await?.ok_or_else(|| Error::NotFound {
        resource: "Webhook".to_string(),
        id: params.webhook_id.to_string(),
    })?;

    if existing.user_id != target_user_id {
        return Err(Error::NotFound {
            resource: "Webhook".to_string(),
            id: params.webhook_id.to_string(),
        });
    }

    let deleted = repo.delete(params.webhook_id).await?;
    if !deleted {
        return Err(Error::NotFound {
            resource: "Webhook".to_string(),
            id: params.webhook_id.to_string(),
        });
    }

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Rotate a webhook's secret.
#[utoipa::path(
    post,
    path = "/users/{user_id}/webhooks/{webhook_id}/rotate-secret",
    tag = "webhooks",
    summary = "Rotate webhook secret",
    description = "Generate a new secret for a webhook. Returns the new secret which is only shown once.",
    params(
        ("user_id" = uuid::Uuid, Path, description = "User ID"),
        ("webhook_id" = uuid::Uuid, Path, description = "Webhook ID"),
    ),
    responses(
        (status = 200, description = "Secret rotated", body = WebhookWithSecretResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Webhook not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[instrument(skip_all)]
pub async fn rotate_secret<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(params): Path<WebhookPathParams>,
    current_user: crate::api::models::users::CurrentUser,
) -> Result<Json<WebhookWithSecretResponse>> {
    let target_user_id: UserId = params.user_id;

    // Check permissions (use UpdateOwn/UpdateAll for secret rotation)
    let can_update_all = permissions::has_permission(&current_user, Resource::Webhooks, Operation::UpdateAll);
    let can_update_own =
        target_user_id == current_user.id && permissions::has_permission(&current_user, Resource::Webhooks, Operation::UpdateOwn);

    if !can_update_all && !can_update_own {
        return Err(Error::InsufficientPermissions {
            required: Permission::Any(vec![
                Permission::Allow(Resource::Webhooks, Operation::UpdateAll),
                Permission::Allow(Resource::Webhooks, Operation::UpdateOwn),
            ]),
            action: Operation::UpdateAll,
            resource: format!("webhook {}", params.webhook_id),
        });
    }

    let mut tx = state.db.write().begin().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Webhooks::new(&mut tx);

    // First verify the webhook exists and belongs to the user
    let existing = repo.get_by_id(params.webhook_id).await?.ok_or_else(|| Error::NotFound {
        resource: "Webhook".to_string(),
        id: params.webhook_id.to_string(),
    })?;

    if existing.user_id != target_user_id {
        return Err(Error::NotFound {
            resource: "Webhook".to_string(),
            id: params.webhook_id.to_string(),
        });
    }

    let new_secret = signing::generate_secret();
    let webhook = repo
        .rotate_secret(params.webhook_id, new_secret)
        .await?
        .ok_or_else(|| Error::NotFound {
            resource: "Webhook".to_string(),
            id: params.webhook_id.to_string(),
        })?;

    tx.commit().await.map_err(|e| Error::Database(e.into()))?;

    Ok(Json(webhook.into()))
}

/// Send a test event to a webhook.
#[utoipa::path(
    post,
    path = "/users/{user_id}/webhooks/{webhook_id}/test",
    tag = "webhooks",
    summary = "Test webhook",
    description = "Send a test event to verify webhook connectivity.",
    params(
        ("user_id" = uuid::Uuid, Path, description = "User ID"),
        ("webhook_id" = uuid::Uuid, Path, description = "Webhook ID"),
    ),
    responses(
        (status = 200, description = "Test result", body = WebhookTestResponse),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Webhook not found"),
        (status = 500, description = "Internal server error"),
    ),
    security(
        ("BearerAuth" = []),
        ("CookieAuth" = []),
        ("X-Doubleword-User" = [])
    )
)]
#[instrument(skip_all)]
pub async fn test_webhook<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(params): Path<WebhookPathParams>,
    current_user: crate::api::models::users::CurrentUser,
) -> Result<Json<WebhookTestResponse>> {
    use crate::webhooks::events::RequestCounts;

    let target_user_id: UserId = params.user_id;

    // Check permissions (use ReadOwn/ReadAll for testing)
    let can_read_all = permissions::has_permission(&current_user, Resource::Webhooks, Operation::ReadAll);
    let can_read_own =
        target_user_id == current_user.id && permissions::has_permission(&current_user, Resource::Webhooks, Operation::ReadOwn);

    if !can_read_all && !can_read_own {
        return Err(Error::InsufficientPermissions {
            required: Permission::Any(vec![
                Permission::Allow(Resource::Webhooks, Operation::ReadAll),
                Permission::Allow(Resource::Webhooks, Operation::ReadOwn),
            ]),
            action: Operation::ReadAll,
            resource: format!("webhook {}", params.webhook_id),
        });
    }

    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
    let mut repo = Webhooks::new(&mut conn);

    let webhook = repo.get_by_id(params.webhook_id).await?.ok_or_else(|| Error::NotFound {
        resource: "Webhook".to_string(),
        id: params.webhook_id.to_string(),
    })?;

    if webhook.user_id != target_user_id {
        return Err(Error::NotFound {
            resource: "Webhook".to_string(),
            id: params.webhook_id.to_string(),
        });
    }

    // Create a test event
    let test_event = WebhookEvent::batch_terminal(
        WebhookEventType::BatchCompleted,
        uuid::Uuid::nil(),
        RequestCounts {
            total: 10,
            completed: 10,
            failed: 0,
            cancelled: 0,
        },
        None,
        None,
        chrono::Utc::now(),
        chrono::Utc::now(),
    );

    let payload = test_event.to_json().map_err(|e| Error::Internal {
        operation: format!("Failed to serialize test event: {}", e),
    })?;

    // Generate signature
    let msg_id = uuid::Uuid::new_v4().to_string();
    let timestamp = chrono::Utc::now().timestamp();
    let signature = signing::sign_payload(&msg_id, timestamp, &payload, &webhook.secret).ok_or_else(|| Error::Internal {
        operation: "Failed to sign webhook payload".to_string(),
    })?;

    // Send the request
    let client = reqwest::Client::new();
    let start = std::time::Instant::now();

    let result = client
        .post(&webhook.url)
        .header("Content-Type", "application/json")
        .header("webhook-id", &msg_id)
        .header("webhook-timestamp", timestamp.to_string())
        .header("webhook-signature", &signature)
        .body(payload)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await;

    let duration_ms = start.elapsed().as_millis() as u64;

    match result {
        Ok(response) => {
            let status = response.status();
            let success = status.is_success();
            Ok(Json(WebhookTestResponse {
                success,
                status_code: Some(status.as_u16()),
                error: if success { None } else { Some(format!("HTTP {}", status)) },
                duration_ms,
            }))
        }
        Err(e) => Ok(Json(WebhookTestResponse {
            success: false,
            status_code: None,
            error: Some(e.to_string()),
            duration_ms,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::users::Role;
    use crate::test::utils::*;
    use serde_json::json;
    use sqlx::PgPool;

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_webhook(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        let webhook_data = json!({
            "url": "https://example.com/webhook",
            "description": "Test webhook"
        });

        let response = app
            .post(&format!("/admin/api/v1/users/{}/webhooks", user.id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .json(&webhook_data)
            .await;

        response.assert_status(StatusCode::CREATED);
        let created: WebhookWithSecretResponse = response.json();
        assert_eq!(created.url, "https://example.com/webhook");
        assert!(created.secret.starts_with("whsec_"));
        assert!(created.enabled);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_webhook_requires_https(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        let webhook_data = json!({
            "url": "http://example.com/webhook"
        });

        let response = app
            .post(&format!("/admin/api/v1/users/{}/webhooks", user.id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .json(&webhook_data)
            .await;

        response.assert_status_bad_request();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_webhook_invalid_event_type(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        let webhook_data = json!({
            "url": "https://example.com/webhook",
            "event_types": ["invalid.event"]
        });

        let response = app
            .post(&format!("/admin/api/v1/users/{}/webhooks", user.id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .json(&webhook_data)
            .await;

        response.assert_status_bad_request();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_webhooks(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create a webhook first
        let webhook_data = json!({
            "url": "https://example.com/webhook"
        });

        app.post(&format!("/admin/api/v1/users/{}/webhooks", user.id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .json(&webhook_data)
            .await;

        // List webhooks
        let response = app
            .get(&format!("/admin/api/v1/users/{}/webhooks", user.id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let webhooks: Vec<WebhookResponse> = response.json();
        assert_eq!(webhooks.len(), 1);
        assert_eq!(webhooks[0].url, "https://example.com/webhook");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_webhook(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create a webhook
        let webhook_data = json!({
            "url": "https://example.com/webhook"
        });

        let create_response = app
            .post(&format!("/admin/api/v1/users/{}/webhooks", user.id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .json(&webhook_data)
            .await;

        let created: WebhookWithSecretResponse = create_response.json();

        // Update the webhook
        let update_data = json!({
            "url": "https://example.com/new-webhook",
            "enabled": false
        });

        let response = app
            .patch(&format!("/admin/api/v1/users/{}/webhooks/{}", user.id, created.id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .json(&update_data)
            .await;

        response.assert_status_ok();
        let updated: WebhookResponse = response.json();
        assert_eq!(updated.url, "https://example.com/new-webhook");
        assert!(!updated.enabled);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_delete_webhook(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create a webhook
        let webhook_data = json!({
            "url": "https://example.com/webhook"
        });

        let create_response = app
            .post(&format!("/admin/api/v1/users/{}/webhooks", user.id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .json(&webhook_data)
            .await;

        let created: WebhookWithSecretResponse = create_response.json();

        // Delete the webhook
        let response = app
            .delete(&format!("/admin/api/v1/users/{}/webhooks/{}", user.id, created.id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status(StatusCode::NO_CONTENT);

        // Verify it's deleted
        let get_response = app
            .get(&format!("/admin/api/v1/users/{}/webhooks/{}", user.id, created.id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        get_response.assert_status_not_found();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_rotate_secret(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        // Create a webhook
        let webhook_data = json!({
            "url": "https://example.com/webhook"
        });

        let create_response = app
            .post(&format!("/admin/api/v1/users/{}/webhooks", user.id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .json(&webhook_data)
            .await;

        let created: WebhookWithSecretResponse = create_response.json();
        let old_secret = created.secret.clone();

        // Rotate the secret
        let response = app
            .post(&format!("/admin/api/v1/users/{}/webhooks/{}/rotate-secret", user.id, created.id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
        let rotated: WebhookWithSecretResponse = response.json();
        assert_ne!(rotated.secret, old_secret);
        assert!(rotated.secret.starts_with("whsec_"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_cannot_access_other_users_webhooks(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user1 = create_test_user(&pool, Role::StandardUser).await;
        let user2 = create_test_user(&pool, Role::StandardUser).await;

        // User1 creates a webhook
        let webhook_data = json!({
            "url": "https://example.com/webhook"
        });

        let create_response = app
            .post(&format!("/admin/api/v1/users/{}/webhooks", user1.id))
            .add_header(&add_auth_headers(&user1)[0].0, &add_auth_headers(&user1)[0].1)
            .add_header(&add_auth_headers(&user1)[1].0, &add_auth_headers(&user1)[1].1)
            .json(&webhook_data)
            .await;

        let created: WebhookWithSecretResponse = create_response.json();

        // User2 tries to access user1's webhook
        let response = app
            .get(&format!("/admin/api/v1/users/{}/webhooks/{}", user1.id, created.id))
            .add_header(&add_auth_headers(&user2)[0].0, &add_auth_headers(&user2)[0].1)
            .add_header(&add_auth_headers(&user2)[1].0, &add_auth_headers(&user2)[1].1)
            .await;

        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_admin_can_access_other_users_webhooks(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;
        let admin = create_test_admin_user(&pool, Role::PlatformManager).await;

        // User creates a webhook
        let webhook_data = json!({
            "url": "https://example.com/webhook"
        });

        let create_response = app
            .post(&format!("/admin/api/v1/users/{}/webhooks", user.id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .json(&webhook_data)
            .await;

        let created: WebhookWithSecretResponse = create_response.json();

        // Admin can access the webhook
        let response = app
            .get(&format!("/admin/api/v1/users/{}/webhooks/{}", user.id, created.id))
            .add_header(&add_auth_headers(&admin)[0].0, &add_auth_headers(&admin)[0].1)
            .add_header(&add_auth_headers(&admin)[1].0, &add_auth_headers(&admin)[1].1)
            .await;

        response.assert_status_ok();
    }
}
