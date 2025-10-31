use crate::config::{NativeAuthConfig, ProxyHeaderAuthConfig, SecurityConfig};
use crate::db::handlers::inference_endpoints::{InferenceEndpointFilter, InferenceEndpoints};
use crate::db::handlers::repository::Repository;
use crate::errors::Error;
use crate::types::{GroupId, Operation, Permission, Resource, UserId};
use crate::{
    api::models::{
        api_keys::ApiKeyCreate,
        users::{CurrentUser, Role, UserResponse},
    },
    db::{
        handlers::{api_keys::ApiKeys, Deployments, Groups, Users},
        models::{
            api_keys::{ApiKeyCreateDBRequest, ApiKeyDBResponse},
            deployments::{DeploymentCreateDBRequest, DeploymentDBResponse},
            groups::{GroupCreateDBRequest, GroupDBResponse},
            users::UserCreateDBRequest,
        },
    },
};

use axum_test::TestServer;
use sqlx::{PgConnection, PgPool};
use tokio_util::sync::DropGuard;
use uuid::Uuid;

pub async fn create_test_app(pool: PgPool, enable_sync: bool) -> (TestServer, Option<DropGuard>) {
    let config = create_test_config();
    let (router, onwards_config_sync, drop_guard) = crate::setup_app(pool, config, true).await.expect("Failed to setup test app");

    if enable_sync {
        // Start the config sync in background for tests
        tokio::spawn(async move {
            if let Err(e) = onwards_config_sync.start().await {
                eprintln!("Config sync error in test: {e}");
            }
        });

        let server = TestServer::new(router).expect("Failed to create test server");
        (server, Some(drop_guard))
    } else {
        let server = TestServer::new(router).expect("Failed to create test server");
        (server, None)
    }
}

pub fn create_test_config() -> crate::config::Config {
    let database_url = std::env::var("TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://postgres@localhost/test".to_string());

    crate::config::Config {
        database_url: None, // Deprecated field
        database: crate::config::DatabaseConfig::External { url: database_url },
        host: "127.0.0.1".to_string(),
        port: 0,
        admin_email: "admin@test.com".to_string(),
        admin_password: None,
        secret_key: Some("test-secret-key-for-testing-only".to_string()),
        model_sources: vec![crate::config::ModelSource {
            name: "test".to_string(),
            url: "http://localhost:8081".parse().unwrap(),
            api_key: None,
            sync_interval: std::time::Duration::from_secs(60),
        }],
        metadata: crate::config::Metadata::default(),
        auth: crate::config::AuthConfig {
            native: NativeAuthConfig {
                enabled: false,
                ..Default::default()
            },
            proxy_header: ProxyHeaderAuthConfig {
                enabled: true,
                ..Default::default()
            },
            security: SecurityConfig::default(),
        },
        enable_metrics: false,
        enable_request_logging: false,
    }
}

pub async fn create_test_user(pool: &PgPool, role: Role) -> UserResponse {
    let mut conn = pool.acquire().await.expect("Failed to acquire connection");
    let mut users_repo = Users::new(&mut conn);
    let user_id = Uuid::new_v4();
    let username = format!("testuser_{}", user_id.simple());
    let email = format!("{username}@example.com");

    let roles = vec![role];

    let user_create = UserCreateDBRequest {
        username,
        email,
        display_name: Some("Test User".to_string()),
        avatar_url: None,
        is_admin: false,
        roles,
        auth_source: "test".to_string(),
        password_hash: None,
    };

    let user = users_repo.create(&user_create).await.expect("Failed to create test user");
    UserResponse::from(user)
}

pub async fn create_test_admin_user(pool: &PgPool, role: Role) -> UserResponse {
    let mut conn = pool.acquire().await.expect("Failed to acquire connection");
    let mut users_repo = Users::new(&mut conn);
    let user_id = Uuid::new_v4();
    let username = format!("testadmin_{}", user_id.simple());
    let email = format!("{username}@example.com");

    let roles = vec![role];

    let user_create = UserCreateDBRequest {
        username,
        email,
        display_name: Some("Test Admin User".to_string()),
        avatar_url: None,
        is_admin: true,
        roles,
        auth_source: "test".to_string(),
        password_hash: None,
    };

    let user = users_repo.create(&user_create).await.expect("Failed to create test admin user");
    UserResponse::from(user)
}

pub async fn create_test_user_with_roles(pool: &PgPool, roles: Vec<Role>) -> UserResponse {
    let mut conn = pool.acquire().await.expect("Failed to acquire connection");
    let mut users_repo = Users::new(&mut conn);
    let user_id = Uuid::new_v4();
    let username = format!("testuser_{}", user_id.simple());
    let email = format!("{username}@example.com");

    let user_create = UserCreateDBRequest {
        username,
        email,
        display_name: Some("Test Multi-Role User".to_string()),
        avatar_url: None,
        is_admin: false,
        roles,
        auth_source: "test".to_string(),
        password_hash: None,
    };

    let user = users_repo
        .create(&user_create)
        .await
        .expect("Failed to create test user with roles");
    UserResponse::from(user)
}

pub fn add_auth_headers(user: &UserResponse) -> (String, String) {
    (ProxyHeaderAuthConfig::default().header_name, user.email.clone())
}

pub async fn create_test_group(pool: &PgPool) -> GroupDBResponse {
    let mut conn = pool.acquire().await.expect("Failed to acquire connection");
    let system_user = get_system_user(&mut conn).await;

    let mut group_repo = Groups::new(&mut conn);
    let group_create = GroupCreateDBRequest {
        name: format!("test_group_{}", Uuid::new_v4().simple()),
        description: Some("Test group".to_string()),
        created_by: system_user.id,
    };

    group_repo.create(&group_create).await.expect("Failed to create test group")
}

pub async fn get_system_user(pool: &mut PgConnection) -> UserResponse {
    let user_id = Uuid::nil();
    let user = sqlx::query!(
        r#"
        SELECT id, username, email, display_name, avatar_url, is_admin, created_at, updated_at, auth_source
        FROM users
        WHERE users.id = $1
        "#,
        user_id
    )
    .fetch_one(&mut *pool)
    .await
    .expect("Failed to get system user");

    let roles = sqlx::query!("SELECT role as \"role: Role\" FROM user_roles WHERE user_id = $1", user.id)
        .fetch_all(&mut *pool)
        .await
        .expect("Failed to get system user roles");

    let roles: Vec<Role> = roles.into_iter().map(|r| r.role).collect();

    // Convert to UserResponse
    UserResponse {
        id: user.id,
        username: user.username,
        email: user.email,
        display_name: user.display_name,
        avatar_url: user.avatar_url,
        is_admin: user.is_admin,
        roles,
        created_at: user.created_at,
        updated_at: user.updated_at,
        last_login: None,
        auth_source: user.auth_source,
        groups: None, // Groups not included in test users by default
        credit_balance: None,
    }
}

pub async fn add_user_to_group(pool: &PgPool, user_id: UserId, group_id: GroupId) {
    let mut conn = pool.acquire().await.expect("Failed to acquire connection");
    let mut group_repo = Groups::new(&mut conn);
    group_repo
        .add_user_to_group(user_id, group_id)
        .await
        .expect("Failed to add user to group");
}

pub async fn create_test_api_key_for_user(pool: &PgPool, user_id: UserId) -> ApiKeyDBResponse {
    let mut conn = pool.acquire().await.expect("Failed to acquire connection");
    let mut api_key_repo = ApiKeys::new(&mut conn);
    let request = ApiKeyCreateDBRequest::new(
        user_id,
        ApiKeyCreate {
            name: "Test API Key".to_string(),
            description: Some("Test description".to_string()),
            requests_per_second: None,
            burst_size: None,
        },
    );

    api_key_repo.create(&request).await.expect("Failed to create test API key")
}

pub async fn create_test_deployment(pool: &PgPool, created_by: UserId, model_name: &str, alias: &str) -> DeploymentDBResponse {
    // Get a valid endpoint ID
    let mut tx = pool.begin().await.expect("Failed to begin transaction");

    let mut endpoints_repo = InferenceEndpoints::new(&mut tx);
    let filter = InferenceEndpointFilter::new(0, 100);
    let endpoints = endpoints_repo.list(&filter).await.expect("Failed to list endpoints");
    let test_endpoint_id = endpoints
        .into_iter()
        .find(|e| e.name == "test")
        .expect("Test endpoint should exist")
        .id;

    let mut deployment_repo = Deployments::new(&mut tx);
    let request = DeploymentCreateDBRequest::builder()
        .created_by(created_by)
        .model_name(model_name.to_string())
        .alias(alias.to_string())
        .hosted_on(test_endpoint_id)
        .build();

    let response = deployment_repo.create(&request).await.expect("Failed to create test deployment");
    tx.commit().await.expect("Failed to commit transaction");
    response
}

pub async fn add_deployment_to_group(pool: &PgPool, deployment_id: crate::types::DeploymentId, group_id: GroupId, granted_by: UserId) {
    let mut conn = pool.acquire().await.expect("Failed to acquire connection");
    let mut group_repo = Groups::new(&mut conn);
    group_repo
        .add_deployment_to_group(deployment_id, group_id, granted_by)
        .await
        .expect("Failed to add deployment to group");
}

pub async fn get_test_endpoint_id(pool: &PgPool) -> uuid::Uuid {
    let mut conn = pool.acquire().await.expect("Failed to acquire connection");
    let mut endpoints_repo = InferenceEndpoints::new(&mut conn);
    let filter = crate::db::handlers::inference_endpoints::InferenceEndpointFilter::new(0, 100);
    let endpoints = endpoints_repo.list(&filter).await.expect("Failed to list endpoints");
    endpoints.iter().find(|e| e.name == "test").expect("Test endpoint should exist").id
}

pub fn require_admin(user: CurrentUser) -> Result<CurrentUser, Error> {
    if user.is_admin {
        Ok(user)
    } else {
        Err(Error::InsufficientPermissions {
            required: Permission::Allow(Resource::Users, Operation::ReadAll),
            action: Operation::ReadAll,
            resource: "admin resource".to_string(),
        })
    }
}
