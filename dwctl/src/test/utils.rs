//! Test utilities for integration testing
use crate::config::{
    BatchConfig, DaemonConfig, DaemonEnabled, FilesConfig, LeaderElectionConfig, NativeAuthConfig, OnwardsSyncConfig, PasswordConfig,
    PoolSettings, ProbeSchedulerConfig, ProxyHeaderAuthConfig, SecurityConfig,
};
use crate::db::handlers::inference_endpoints::{InferenceEndpointFilter, InferenceEndpoints};
use crate::db::handlers::repository::Repository;
use crate::db::models::api_keys::ApiKeyPurpose;
use crate::errors::Error;
use crate::types::{GroupId, Operation, Permission, Resource, UserId};
use crate::{
    api::models::{
        api_keys::ApiKeyCreate,
        users::{CurrentUser, Role, UserResponse},
    },
    db::{
        handlers::{Deployments, Groups, Users, api_keys::ApiKeys},
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
use sqlx_pool_router::TestDbPools;
use uuid::Uuid;

/// Create an AppState with TestDbPools for proper read/write replica testing
/// Use this in tests that need to manually construct AppState instead of using create_test_app
async fn create_test_app_state(pool: PgPool) -> crate::AppState<TestDbPools> {
    create_test_app_state_with_config(pool, create_test_config()).await
}

/// Create an AppState with TestDbPools and custom config
pub async fn create_test_app_state_with_config(pool: PgPool, config: crate::config::Config) -> crate::AppState<TestDbPools> {
    let test_pools = TestDbPools::new(pool.clone()).await.expect("Failed to create TestDbPools");
    let fusillade_pools = TestDbPools::new(pool.clone())
        .await
        .expect("Failed to create fusillade TestDbPools");

    let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(fusillade_pools));

    crate::AppState::builder()
        .db(test_pools)
        .config(config)
        .request_manager(request_manager)
        .build()
}

pub async fn create_test_app(pool: PgPool, _enable_sync: bool) -> (TestServer, crate::BackgroundServices) {
    let config = create_test_config();

    let app = crate::Application::new_with_pool(config, Some(pool))
        .await
        .expect("Failed to create application");

    // Convert to test server (sync is always enabled in new())
    app.into_test_server()
}

pub async fn create_test_app_with_config(
    pool: PgPool,
    config: crate::config::Config,
    _enable_sync: bool,
) -> (TestServer, crate::BackgroundServices) {
    let app = crate::Application::new_with_pool(config, Some(pool))
        .await
        .expect("Failed to create application");

    // Convert to test server (sync is always enabled in new())
    app.into_test_server()
}

pub fn create_test_config() -> crate::config::Config {
    // Use temp directory for test emails
    let temp_dir = std::env::temp_dir().join(format!("dwctl-test-emails-{}", std::process::id()));

    crate::config::Config {
        database_url: None,
        database_replica_url: None,
        database: crate::config::DatabaseConfig::External {
            url: "Something".to_string(), // Will get overriden by env var
            replica_url: None,
            pool: PoolSettings {
                max_connections: 4,
                min_connections: 1,
                ..Default::default()
            },
            replica_pool: None,
            fusillade: crate::config::ComponentDb::Schema {
                name: "fusillade".to_string(),
                pool: PoolSettings {
                    max_connections: 4,
                    min_connections: 0,
                    ..Default::default()
                },
                replica_pool: None,
            },
            outlet: crate::config::ComponentDb::Schema {
                name: "outlet".to_string(),
                pool: PoolSettings {
                    max_connections: 4,
                    min_connections: 0,
                    ..Default::default()
                },
                replica_pool: None,
            },
        },
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
            default_models: None,
        }],
        metadata: crate::config::Metadata::default(),
        payment: None,
        auth: crate::config::AuthConfig {
            native: NativeAuthConfig {
                enabled: false,
                email: crate::config::EmailConfig {
                    transport: crate::config::EmailTransportConfig::File {
                        path: temp_dir.to_string_lossy().to_string(),
                    },
                    ..Default::default()
                },
                password: PasswordConfig {
                    min_length: 8,
                    max_length: 64,
                    // Ultra-weak params for fast testing (DO NOT USE IN PRODUCTION)
                    argon2_memory_kib: 128, // 128 KB (vs 19 MB production)
                    argon2_iterations: 1,   // 1 iteration (vs 2 production)
                    argon2_parallelism: 1,  // 1 thread
                },
                ..Default::default()
            },
            proxy_header: ProxyHeaderAuthConfig {
                enabled: true,
                ..Default::default()
            },
            security: SecurityConfig::default(),
            default_user_roles: vec![crate::api::models::users::Role::StandardUser],
        },
        enable_metrics: false,
        enable_request_logging: false,
        enable_analytics: true,
        enable_otel_export: false,
        credits: crate::config::CreditsConfig::default(),
        batches: BatchConfig {
            enabled: true,
            files: FilesConfig {
                max_file_size: 1000 * 1024 * 1024, //1GB
                ..Default::default()
            },
            ..Default::default()
        },
        background_services: crate::config::BackgroundServicesConfig {
            onwards_sync: OnwardsSyncConfig { enabled: false },
            probe_scheduler: ProbeSchedulerConfig { enabled: false },
            batch_daemon: DaemonConfig {
                enabled: DaemonEnabled::Never,
                ..Default::default()
            },
            leader_election: LeaderElectionConfig { enabled: false },
            ..Default::default()
        },
        sample_files: crate::sample_files::SampleFilesConfig::default(),
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
        username: username.clone(),
        email,
        display_name: Some("Test User".to_string()),
        avatar_url: None,
        is_admin: false,
        roles,
        auth_source: "test".to_string(),
        password_hash: None,
        external_user_id: Some(username.clone()),
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
        username: username.clone(),
        email,
        display_name: Some("Test Admin User".to_string()),
        avatar_url: None,
        is_admin: true,
        roles,
        auth_source: "test".to_string(),
        password_hash: None,
        external_user_id: Some(username.clone()),
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
        username: username.clone(),
        email,
        display_name: Some("Test Multi-Role User".to_string()),
        avatar_url: None,
        is_admin: false,
        roles,
        auth_source: "test".to_string(),
        password_hash: None,
        external_user_id: Some(username.clone()),
    };

    let user = users_repo
        .create(&user_create)
        .await
        .expect("Failed to create test user with roles");
    UserResponse::from(user)
}

pub fn add_auth_headers(user: &UserResponse) -> Vec<(String, String)> {
    let config = ProxyHeaderAuthConfig::default();
    let external_user_id = user.external_user_id.as_ref().unwrap_or(&user.username);
    vec![
        (config.header_name, external_user_id.clone()),
        (config.email_header_name, user.email.clone()),
    ]
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
        external_user_id: None,
        groups: None, // Groups not included in test users by default
        credit_balance: None,
        has_payment_provider_id: false,
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
            name: format!("Test API Key {}", Uuid::new_v4().simple()),
            description: Some("Test description".to_string()),
            purpose: ApiKeyPurpose::Realtime,
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

pub async fn create_test_endpoint(pool: &PgPool, name: &str, created_by: UserId) -> uuid::Uuid {
    let endpoint_id = uuid::Uuid::new_v4();
    sqlx::query!(
        r#"
        INSERT INTO inference_endpoints (id, name, url, api_key, created_by)
        VALUES ($1, $2, 'http://localhost:8080', NULL, $3)
        "#,
        endpoint_id,
        name,
        created_by
    )
    .execute(pool)
    .await
    .expect("Failed to create test endpoint");
    endpoint_id
}

pub async fn create_test_model(pool: &PgPool, model_name: &str, alias: &str, endpoint_id: uuid::Uuid, created_by: UserId) -> uuid::Uuid {
    let deployment_id = uuid::Uuid::new_v4();
    sqlx::query!(
        r#"
        INSERT INTO deployed_models (id, model_name, alias, hosted_on, created_by, deleted)
        VALUES ($1, $2, $3, $4, $5, false)
        "#,
        deployment_id,
        model_name,
        alias,
        endpoint_id,
        created_by
    )
    .execute(pool)
    .await
    .expect("Failed to create test model");
    deployment_id
}
