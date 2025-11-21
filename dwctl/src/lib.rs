//! # dwctl: Control Layer for AI Model Management
//!
//! `dwctl` is a comprehensive control plane for managing AI model deployments, access control,
//! and request routing. It provides a RESTful API for managing users, groups, deployments,
//! and access policies, along with an OpenAI-compatible proxy for routing AI requests.
//!
//! ## Overview
//!
//! `dwctl` acts as a centralized control plane sitting between AI model consumers and multiple
//! AI inference endpoints. Organizations deploying AI models face challenges around multi-tenancy,
//! cost tracking, and managing access to diverse model deployments. This crate addresses these
//! challenges by providing a unified control layer that handles authentication, authorization,
//! request routing, and observability.
//!
//! The system is designed for platforms that need to expose AI capabilities to multiple users or
//! teams while maintaining isolation, tracking usage, and ensuring high availability. It's
//! particularly suited for organizations running their own inference infrastructure or aggregating
//! multiple AI providers behind a single interface.
//!
//! ### What It Does
//!
//! At its core, `dwctl` receives requests from clients using the OpenAI-compatible API format,
//! authenticates the user, checks their permissions against the requested model, routes the
//! request to the appropriate inference endpoint, and optionally logs the request/response for
//! audit and analytics. It manages a credit system for usage tracking and rate limiting, monitors
//! endpoint health to remove failing backends from rotation, and supports batch processing for
//! asynchronous workloads.
//!
//! ## Architecture
//!
//! The application is built on [Axum](https://github.com/tokio-rs/axum) for the HTTP layer and
//! uses PostgreSQL for all persistence needs. It can operate with either an embedded PostgreSQL
//! instance (useful for development) or an external PostgreSQL database (recommended for production).
//!
//! ### Request Flow
//!
//! The application handles two distinct request flows depending on the endpoint accessed.
//!
//! #### AI Proxy Requests (`/ai/v1/*`)
//!
//! When a client makes a request to `/ai/v1/chat/completions`, the request is handled by the
//! [onwards] routing layer. The system maintains a synchronized cache of valid API keys for each
//! model deployment—only keys with sufficient credits and appropriate group access are included.
//! This cache is continuously updated via PostgreSQL LISTEN/NOTIFY whenever database state changes.
//! onwards validates the incoming API key against this cache, maps the model alias to an inference
//! endpoint, and forwards the request. Optional middleware powered by [outlet] and [outlet-postgres]
//! can log request and response data to PostgreSQL for auditing, and credits are deducted based on
//! token usage.
//!
//! #### Management API Requests (`/admin/api/v1/*`)
//!
//! Requests to the management API follow a traditional web application flow. The request first
//! passes through authentication middleware that validates credentials through multiple methods:
//! session cookies (for browser clients), trusted proxy headers (for SSO integration), or API keys
//! with "platform" purpose. The authentication system tries these methods in priority order, falling
//! back to the next if one is unavailable or invalid. Once authenticated, the request reaches the
//! appropriate handler which performs authorization checks based on the user's roles and permissions.
//! Handlers interact with the database through repository interfaces to perform CRUD operations on
//! resources like users, groups, deployments, and endpoints. Changes to deployments or API keys
//! trigger PostgreSQL NOTIFY events that update the [onwards] routing cache in real-time.
//!
//! [onwards]: https://github.com/doublewordai/onwards
//! [outlet]: https://github.com/doublewordai/outlet
//! [outlet-postgres]: https://github.com/doublewordai/outlet-postgres
//!
//! ### Core Components
//!
//! The **API layer** ([`api`]) exposes two main surfaces: a management API for administrators at
//! `/admin/api/v1/*` and an OpenAI-compatible proxy at `/ai/v1/*`. The management API uses RESTful
//! conventions for CRUD operations on users, groups, deployments, and other resources, while the
//! proxy API mimics OpenAI's interface to maximize compatibility with existing clients.
//!
//! The **authentication layer** ([`auth`]) handles session-based authentication for the management
//! API and can integrate with SSO proxy implementations for federated authentication. It includes
//! permission checking logic and role-based access control for administrative operations. The AI
//! proxy endpoints use a separate authentication mechanism where valid API keys are synced to
//! [onwards].
//!
//! The **database layer** ([`db`]) uses the repository pattern to abstract data access. Each entity
//! (users, groups, deployments, etc.) has a corresponding repository that handles queries and
//! mutations. The schema uses PostgreSQL features like advisory locks for leader election and
//! LISTEN/NOTIFY for real-time configuration updates.
//!
//! **Background services** run alongside the HTTP server and include a health probe scheduler that
//! periodically checks inference endpoint availability, a batch processing daemon powered by
//! fusillade for async job execution, and an [onwards] configuration sync process that watches for
//! database changes and updates the routing layer in real-time.
//!
//! ## Quick Start
//!
//! ```no_run
//! use clap::Parser;
//! use dwctl::{Application, Config};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     // Parse CLI arguments and load configuration
//!     let args = dwctl::config::Args::parse();
//!     let config = Config::load(&args)?;
//!
//!     // Initialize telemetry (structured logging and optional OpenTelemetry)
//!     dwctl::telemetry::init_telemetry(config.enable_otel_export)?;
//!
//!     // Create and start the application
//!     let app = Application::new(config).await?;
//!
//!     // Run with graceful shutdown on Ctrl+C
//!     app.serve(async {
//!         tokio::signal::ctrl_c().await.expect("Failed to listen for Ctrl+C");
//!     }).await?;
//!
//!     Ok(())
//! }
//! ```
//!
//! ## Database Setup
//!
//! The application requires a PostgreSQL database and automatically runs migrations on startup:
//!
//! ```no_run
//! # use sqlx::PgPool;
//! # async fn example(pool: PgPool) -> Result<(), sqlx::Error> {
//! // Run migrations
//! dwctl::migrator().run(&pool).await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Configuration
//!
//! See the [`config`] module for configuration options.
//!
// TODO: This file has gotten way too big. We need to refactor it into smaller modules.
// The constructors in test_utils should be unified with the actual constructors: right now they're
// actually the best lib way to construct things, which is bad.
pub mod api;
pub mod auth;
pub mod config;
mod crypto;
pub mod db;
mod email;
pub mod errors;
mod leader_election;
mod metrics;
mod openapi;
mod payment_providers;
mod probes;
mod request_logging;
mod static_assets;
mod sync;
pub mod telemetry;
mod types;
use crate::config::CorsOrigin;

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

use crate::{
    api::models::users::Role,
    auth::password,
    db::handlers::{Repository, Users},
    db::models::users::UserCreateDBRequest,
    metrics::GenAiMetrics,
    openapi::ApiDoc,
    request_logging::serializers::{parse_ai_request, AnalyticsResponseSerializer},
};
use auth::middleware::admin_ai_proxy_middleware;
use axum::extract::DefaultBodyLimit;
use axum::http::HeaderValue;
use axum::{http, middleware::from_fn_with_state, routing::{delete, get, patch, post}, Router, ServiceExt};
use axum_prometheus::PrometheusMetricLayer;
use bon::Builder;
pub use config::Config;
use outlet::{RequestLoggerConfig, RequestLoggerLayer};
use outlet_postgres::PostgresHandler;
use request_logging::{AiRequest, AiResponse};
use sqlx::{Executor, PgPool};
use std::sync::Arc;
use tokio::net::TcpListener;
use tower::Layer;
use tower_http::{
    cors::CorsLayer,
    trace::{DefaultMakeSpan, DefaultOnRequest, DefaultOnResponse, TraceLayer},
};
use tracing::{debug, info, instrument, Level};
use utoipa::OpenApi;
use utoipa_rapidoc::RapiDoc;
use uuid::Uuid;

pub use types::{ApiKeyId, DeploymentId, GroupId, InferenceEndpointId, UserId};

/// Application state shared across all request handlers.
///
/// This struct contains all the shared resources needed by the API handlers,
/// including database connections, configuration, and background service managers.
///
/// # Fields
///
/// - `db`: Main PostgreSQL connection pool for application data
/// - `config`: Application configuration loaded from environment/files
/// - `outlet_db`: Optional connection pool for request logging (when enabled)
/// - `metrics_recorder`: Optional Prometheus metrics recorder (when enabled)
/// - `is_leader`: Whether this instance is the elected leader (for distributed deployments)
/// - `request_manager`: Fusillade batch request manager for async processing
///
/// # Example
///
/// ```ignore
/// let state = AppState::builder()
///     .db(pool)
///     .config(config)
///     .request_manager(request_manager)
///     .build();
/// ```
#[derive(Clone, Builder)]
pub struct AppState {
    pub db: PgPool,
    pub config: Config,
    pub outlet_db: Option<PgPool>,
    pub metrics_recorder: Option<GenAiMetrics>,
    #[builder(default = false)]
    pub is_leader: bool,
    pub request_manager: Arc<fusillade::PostgresRequestManager<fusillade::ReqwestHttpClient>>,
}

/// Get the dwctl database migrator
pub fn migrator() -> sqlx::migrate::Migrator {
    sqlx::migrate!("./migrations")
}

/// Create the initial admin user if it doesn't exist.
///
/// This function is idempotent - it will create a new admin user if one doesn't exist,
/// or update the password if the user already exists. This is typically called during
/// application startup to ensure there's always an admin user available.
///
/// # Arguments
///
/// - `email`: Email address for the admin user (also used as username)
/// - `password`: Optional password. If `None`, the user will have no password set
/// - `db`: PostgreSQL connection pool
///
/// # Returns
///
/// Returns the user ID of the created or existing admin user.
///
/// # Errors
///
/// Returns an error if database operations fail.
///
/// # Example
///
/// ```no_run
/// # use dwctl::create_initial_admin_user;
/// # use sqlx::PgPool;
/// # async fn example(pool: PgPool) -> Result<(), sqlx::Error> {
/// let user_id = create_initial_admin_user(
///     "admin@example.com",
///     Some("secure_password"),
///     &pool
/// ).await?;
/// # Ok(())
/// # }
/// ```
#[instrument(skip_all)]
pub async fn create_initial_admin_user(email: &str, password: Option<&str>, db: &PgPool) -> Result<UserId, sqlx::Error> {
    // Hash password if provided
    let password_hash = if let Some(pwd) = password {
        Some(password::hash_string(pwd).map_err(|e| sqlx::Error::Encode(format!("Failed to hash admin password: {e}").into()))?)
    } else {
        None
    };

    // Use a transaction to ensure atomicity
    let mut tx = db.begin().await?;
    let mut user_repo = Users::new(&mut tx);

    // Check if user already exists
    if let Some(existing_user) = user_repo
        .get_user_by_email(email)
        .await
        .map_err(|e| sqlx::Error::Protocol(format!("Failed to check existing user: {e}")))?
    {
        // User exists - update password if provided
        if let Some(password_hash) = password_hash {
            // Update password using raw SQL since we don't have a password update method
            sqlx::query!("UPDATE users SET password_hash = $1 WHERE email = $2", password_hash, email)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        return Ok(existing_user.id);
    }

    // Create new admin user
    let user_create = UserCreateDBRequest {
        username: email.to_string(),
        email: email.to_string(),
        display_name: None,
        avatar_url: None,
        is_admin: true,
        roles: vec![Role::PlatformManager],
        auth_source: "system".to_string(),
        password_hash,
    };

    let created_user = user_repo
        .create(&user_create)
        .await
        .map_err(|e| sqlx::Error::Protocol(format!("Failed to create admin user: {e}")))?;

    tx.commit().await?;
    Ok(created_user.id)
}

/// Seed the database with initial configuration (run only once).
///
/// This function initializes the database with inference endpoints from model sources
/// and generates a new system API key. It's idempotent - subsequent calls will skip
/// seeding to preserve manual changes.
///
/// The function checks the `endpoints_seeded` flag in `system_config` to determine
/// if seeding has already occurred. This prevents overwriting user modifications.
///
/// # Arguments
///
/// - `sources`: Slice of model source configurations to seed as inference endpoints
/// - `db`: PostgreSQL connection pool
///
/// # Returns
///
/// Returns `Ok(())` if seeding succeeds or was already completed.
///
/// # Errors
///
/// Returns an error if database operations fail.
#[instrument(skip_all)]
pub async fn seed_database(sources: &[config::ModelSource], db: &PgPool) -> Result<(), anyhow::Error> {
    // Use a transaction to ensure atomicity
    let mut tx = db.begin().await?;

    // Check if database has already been seeded to prevent overwriting manual changes
    let seeded = sqlx::query_scalar!("SELECT value FROM system_config WHERE key = 'endpoints_seeded'")
        .fetch_optional(&mut *tx)
        .await?;

    if let Some(true) = seeded {
        info!("Database already seeded, skipping seeding operations");
        tx.commit().await?;
        return Ok(());
    }

    info!("Seeding database with initial configuration");

    // Seed endpoints from model sources
    let system_user_id = Uuid::nil();
    for source in sources {
        // Insert endpoint if it doesn't already exist (first-time seeding only)
        sqlx::query!(
            "INSERT INTO inference_endpoints (name, description, url, created_by)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (name) DO NOTHING",
            source.name,
            None::<String>, // System-created endpoints don't have descriptions
            source.url.as_str(),
            system_user_id,
        )
        .execute(&mut *tx)
        .await?;
    }

    // Update the system API key secret with a new secure value
    let system_api_key_id = Uuid::nil();
    let new_secret = crypto::generate_api_key();
    sqlx::query!("UPDATE api_keys SET secret = $1 WHERE id = $2", new_secret, system_api_key_id)
        .execute(&mut *tx)
        .await?;

    // Mark database as seeded to prevent future overwrites
    sqlx::query!(
        "UPDATE system_config SET value = true, updated_at = NOW()
         WHERE key = 'endpoints_seeded'"
    )
    .execute(&mut *tx)
    .await?;

    // Commit the transaction - either everything succeeds or nothing changes
    tx.commit().await?;

    debug!("Database seeded successfully");

    Ok(())
}

/// Setup database connections, run migrations, and initialize data
/// Returns: (embedded_db, main_pool, fusillade_pool, outlet_pool)
async fn setup_database(config: &Config) -> anyhow::Result<(Option<db::embedded::EmbeddedDatabase>, PgPool, PgPool, Option<PgPool>)> {
    // Database connection - handle both embedded and external
    let (_embedded_db, database_url) = match &config.database {
        config::DatabaseConfig::Embedded { .. } => {
            let persistent = config.database.embedded_persistent();
            info!("Starting with embedded database (persistent: {})", persistent);
            if !persistent {
                info!("persistent=false: database will be ephemeral and data will be lost on shutdown");
            }
            #[cfg(feature = "embedded-db")]
            {
                let data_dir = config.database.embedded_data_dir();
                let embedded_db = db::embedded::EmbeddedDatabase::start(data_dir, persistent).await?;
                let url = embedded_db.connection_string().to_string();
                (Some(embedded_db), url)
            }
            #[cfg(not(feature = "embedded-db"))]
            {
                anyhow::bail!(
                    "Embedded database is configured but the feature is not enabled. \
                     Rebuild with --features embedded-db to use embedded database."
                );
            }
        }
        config::DatabaseConfig::External { url } => {
            info!("Using external database");
            (None::<db::embedded::EmbeddedDatabase>, url.clone())
        }
    };

    let pool = PgPool::connect(&database_url).await?;
    migrator().run(&pool).await?;

    // Setup fusillade schema and pool
    let fusillade_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(20)
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                // Set search path to fusillade schema for all connections in this pool
                conn.execute("SET search_path = 'fusillade'").await?;
                Ok(())
            })
        })
        .connect(&database_url)
        .await?;

    fusillade_pool.execute("CREATE SCHEMA IF NOT EXISTS fusillade").await?;
    fusillade::migrator().run(&fusillade_pool).await?;

    // Setup outlet schema and pool if request logging is enabled
    let outlet_pool = if config.enable_request_logging {
        let outlet_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(5) // Smaller pool for logging
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    // Set search path to outlet schema for all connections in this pool
                    conn.execute("SET search_path = 'outlet'").await?;
                    Ok(())
                })
            })
            .connect(&database_url)
            .await?;

        outlet_pool.execute("CREATE SCHEMA IF NOT EXISTS outlet").await?;
        outlet_postgres::migrator().run(&outlet_pool).await?;

        Some(outlet_pool)
    } else {
        None
    };

    // Create initial admin user if it doesn't exist
    create_initial_admin_user(&config.admin_email, config.admin_password.as_deref(), &pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create initial admin user: {}", e))?;

    // Seed database with initial configuration (only runs once)
    seed_database(&config.model_sources, &pool).await?;

    Ok((_embedded_db, pool, fusillade_pool, outlet_pool))
}

/// Create CORS layer from configuration
fn create_cors_layer(config: &Config) -> anyhow::Result<CorsLayer> {
    let mut origins = Vec::new();
    for origin in &config.auth.security.cors.allowed_origins {
        let header_value = match origin {
            CorsOrigin::Wildcard => "*".parse::<HeaderValue>()?,
            CorsOrigin::Url(url) => url.as_str().parse::<HeaderValue>()?,
        };
        origins.push(header_value);
    }

    let mut cors = CorsLayer::new()
        .allow_origin(origins)
        .allow_credentials(config.auth.security.cors.allow_credentials)
        .expose_headers(vec![http::header::LOCATION]);

    if let Some(max_age) = config.auth.security.cors.max_age {
        cors = cors.max_age(std::time::Duration::from_secs(max_age));
    }

    Ok(cors)
}

/// Build the main application router with all endpoints and middleware.
///
/// This function constructs the complete Axum router with:
/// - Authentication routes (login, registration, password reset)
/// - Admin API routes (user/group/deployment management)
/// - AI proxy routes (OpenAI-compatible endpoints)
/// - Static asset serving and SPA fallback
/// - Optional request logging via outlet
/// - Optional Prometheus metrics
/// - CORS configuration
/// - Tracing middleware
///
/// # Arguments
///
/// - `state`: Mutable application state (metrics recorder may be initialized here)
/// - `onwards_router`: Pre-configured router for AI request proxying
///
/// # Returns
///
/// Returns the fully configured router ready to be served.
///
/// # Errors
///
/// Returns an error if CORS configuration is invalid or metrics initialization fails.
#[instrument(skip_all)]
pub async fn build_router(state: &mut AppState, onwards_router: Router) -> anyhow::Result<Router> {
    // Setup request logging if enabled
    let outlet_layer = if let Some(outlet_pool) = state.outlet_db.as_ref() {
        // Initialize GenAI metrics BEFORE creating analytics serializer if metrics enabled
        if state.config.enable_metrics {
            let gen_ai_registry = prometheus::Registry::new();
            let gen_ai_metrics =
                GenAiMetrics::new(&gen_ai_registry).map_err(|e| anyhow::anyhow!("Failed to create GenAI metrics: {}", e))?;
            state.metrics_recorder = Some(gen_ai_metrics);
        }

        let analytics_serializer = AnalyticsResponseSerializer::new(
            state.db.clone(),
            uuid::Uuid::new_v4(),
            state.config.clone(),
            state.metrics_recorder.clone(),
        );

        let postgres_handler = PostgresHandler::<AiRequest, AiResponse>::from_pool(outlet_pool.clone())
            .await
            .expect("Failed to create PostgresHandler for request logging")
            .with_request_serializer(parse_ai_request)
            .with_response_serializer(analytics_serializer.create_serializer());

        let outlet_config = RequestLoggerConfig {
            capture_request_body: true,
            capture_response_body: true,
            path_filter: None, // No path filter needed - applied directly to ai_router
        };

        Some(RequestLoggerLayer::new(outlet_config, postgres_handler))
    } else {
        None
    };
    // Authentication routes (at root level, can be masked when deployed behind SSO proxy)
    let auth_routes = Router::new()
        .route(
            "/authentication/register",
            get(api::handlers::auth::get_registration_info).post(api::handlers::auth::register),
        )
        .route(
            "/authentication/login",
            get(api::handlers::auth::get_login_info).post(api::handlers::auth::login),
        )
        .route("/authentication/logout", post(api::handlers::auth::logout))
        .route("/authentication/password-resets", post(api::handlers::auth::request_password_reset))
        .route(
            "/authentication/password-resets/{token_id}/confirm",
            post(api::handlers::auth::confirm_password_reset),
        )
        .route("/authentication/password-change", post(api::handlers::auth::change_password))
        .with_state(state.clone());

    // API routes
    let api_routes = Router::new()
        .route("/config", get(api::handlers::config::get_config))
        // User management (admin only for collection operations)
        .route("/users", get(api::handlers::users::list_users))
        .route("/users", post(api::handlers::users::create_user))
        .route("/users/{id}", get(api::handlers::users::get_user))
        .route("/users/{id}", patch(api::handlers::users::update_user))
        .route("/users/{id}", delete(api::handlers::users::delete_user))
        // API Keys as user sub-resources
        .route("/users/{user_id}/api-keys", get(api::handlers::api_keys::list_user_api_keys))
        .route("/users/{user_id}/api-keys", post(api::handlers::api_keys::create_user_api_key))
        .route("/users/{user_id}/api-keys/{id}", get(api::handlers::api_keys::get_user_api_key))
        .route(
            "/users/{user_id}/api-keys/{id}",
            delete(api::handlers::api_keys::delete_user_api_key),
        )
        // User-group relationships
        .route("/users/{user_id}/groups", get(api::handlers::groups::get_user_groups))
        .route("/users/{user_id}/groups/{group_id}", post(api::handlers::groups::add_group_to_user))
        .route(
            "/users/{user_id}/groups/{group_id}",
            delete(api::handlers::groups::remove_group_from_user),
        )
        // Transaction management (RESTful credit transactions)
        .route("/transactions", post(api::handlers::transactions::create_transaction))
        .route("/transactions/{transaction_id}", get(api::handlers::transactions::get_transaction))
        .route("/transactions", get(api::handlers::transactions::list_transactions))
        // Payment processing
        .route("/payments", post(api::handlers::payments::create_payment))
        .route("/payments/{id}", patch(api::handlers::payments::process_payment))
        // Inference endpoints management (admin only for write operations)
        .route("/endpoints", get(api::handlers::inference_endpoints::list_inference_endpoints))
        .route("/endpoints", post(api::handlers::inference_endpoints::create_inference_endpoint))
        .route(
            "/endpoints/validate",
            post(api::handlers::inference_endpoints::validate_inference_endpoint),
        )
        .route("/endpoints/{id}", get(api::handlers::inference_endpoints::get_inference_endpoint))
        .route(
            "/endpoints/{id}",
            patch(api::handlers::inference_endpoints::update_inference_endpoint),
        )
        .route(
            "/endpoints/{id}",
            delete(api::handlers::inference_endpoints::delete_inference_endpoint),
        )
        .route(
            "/endpoints/{id}/synchronize",
            post(api::handlers::inference_endpoints::synchronize_endpoint),
        )
        // Models endpoints
        .route("/models", get(api::handlers::deployments::list_deployed_models))
        .route("/models", post(api::handlers::deployments::create_deployed_model))
        .route("/models/{id}", get(api::handlers::deployments::get_deployed_model))
        .route("/models/{id}", patch(api::handlers::deployments::update_deployed_model))
        .route("/models/{id}", delete(api::handlers::deployments::delete_deployed_model))
        // Groups management
        .route("/groups", get(api::handlers::groups::list_groups))
        .route("/groups", post(api::handlers::groups::create_group))
        .route("/groups/{id}", get(api::handlers::groups::get_group))
        .route("/groups/{id}", patch(api::handlers::groups::update_group))
        .route("/groups/{id}", delete(api::handlers::groups::delete_group))
        // Group-user relationships
        .route("/groups/{group_id}/users", get(api::handlers::groups::get_group_users))
        .route("/groups/{group_id}/users/{user_id}", post(api::handlers::groups::add_user_to_group))
        .route(
            "/groups/{group_id}/users/{user_id}",
            delete(api::handlers::groups::remove_user_from_group),
        )
        // Group-model relationships
        .route("/groups/{group_id}/models", get(api::handlers::groups::get_group_deployments))
        .route(
            "/groups/{group_id}/models/{deployment_id}",
            post(api::handlers::groups::add_deployment_to_group),
        )
        .route(
            "/groups/{group_id}/models/{deployment_id}",
            delete(api::handlers::groups::remove_deployment_from_group),
        )
        .route("/models/{deployment_id}/groups", get(api::handlers::groups::get_deployment_groups))
        .route("/requests", get(api::handlers::requests::list_requests))
        .route("/requests/aggregate", get(api::handlers::requests::aggregate_requests))
        .route("/requests/aggregate-by-user", get(api::handlers::requests::aggregate_by_user))
        // Probes management
        .route("/probes", get(api::handlers::probes::list_probes))
        .route("/probes", post(api::handlers::probes::create_probe))
        .route("/probes/test/{deployment_id}", post(api::handlers::probes::test_probe))
        .route("/probes/{id}", get(api::handlers::probes::get_probe))
        .route("/probes/{id}", patch(api::handlers::probes::update_probe))
        .route("/probes/{id}", delete(api::handlers::probes::delete_probe))
        .route("/probes/{id}/activate", patch(api::handlers::probes::activate_probe))
        .route("/probes/{id}/deactivate", patch(api::handlers::probes::deactivate_probe))
        .route("/probes/{id}/execute", post(api::handlers::probes::execute_probe))
        .route("/probes/{id}/results", get(api::handlers::probes::get_probe_results))
        .route("/probes/{id}/statistics", get(api::handlers::probes::get_statistics));

    let api_routes_with_state = api_routes.with_state(state.clone());

    // Batches API routes (files + batches) - conditionally enabled under /ai/v1
    let batches_routes = if state.config.batches.enabled {
        // File upload route with custom body limit (other routes use default)
        let file_upload_limit = state.config.batches.files.max_file_size;
        let file_router = Router::new().route(
            "/files",
            post(api::handlers::files::upload_file).layer(DefaultBodyLimit::max(file_upload_limit as usize)),
        );

        Some(
            Router::new()
                // Files management - merge file upload route with custom body limit
                .merge(file_router)
                .route("/files", get(api::handlers::files::list_files))
                .route("/files/{file_id}", get(api::handlers::files::get_file))
                .route("/files/{file_id}/content", get(api::handlers::files::get_file_content))
                .route("/files/{file_id}", delete(api::handlers::files::delete_file))
                // Batches management
                .route("/batches", post(api::handlers::batches::create_batch))
                .route("/batches", get(api::handlers::batches::list_batches))
                .route("/batches/{batch_id}", get(api::handlers::batches::get_batch))
                .route("/batches/{batch_id}/cancel", post(api::handlers::batches::cancel_batch))
                // Daemon monitoring
                .route("/daemons", get(api::handlers::daemons::list_daemons))
                .with_state(state.clone()),
        )
    } else {
        None
    };

    // Serve embedded static assets, falling back to SPA for unmatched routes
    let fallback = get(api::handlers::static_assets::serve_embedded_asset).fallback(get(api::handlers::static_assets::spa_fallback));

    // Apply request logging layer only to onwards router
    let onwards_router = if let Some(outlet_layer) = outlet_layer.clone() {
        onwards_router.layer(outlet_layer)
    } else {
        onwards_router
    };

    // Build the app with admin API and onwards proxy nested. serve the (restricted) openai spec.
    // Batches routes are merged with onwards router under /ai/v1 (batches match first)
    let ai_router = if let Some(batches) = batches_routes {
        batches.merge(onwards_router)
    } else {
        onwards_router
    };

    let router = Router::new()
        .route("/healthz", get(|| async { "OK" }))
        .route(
            "/openai-openapi.yaml",
            get(|| async {
                const OPENAPI_SPEC: &str = include_str!("openai-openapi.yaml");
                (axum::http::StatusCode::OK, [("content-type", "application/yaml")], OPENAPI_SPEC)
            }),
        )
        // Webhook routes (external services, not part of client API docs)
        .route("/webhooks/payments", post(api::handlers::payments::webhook_handler))
        .with_state(state.clone())
        .merge(auth_routes)
        .nest("/ai/v1", ai_router)
        .nest("/admin/api/v1", api_routes_with_state)
        .merge(RapiDoc::with_openapi("/api-docs/openapi.json", ApiDoc::openapi()).path("/admin/docs"))
        .merge(RapiDoc::new("/openai-openapi.yaml").path("/ai/docs"))
        .fallback_service(fallback);

    // Create CORS layer from config
    let cors_layer = create_cors_layer(&state.config)?;

    // Apply CORS to main router (request logging already applied to onwards_router above)
    let mut router = router.layer(cors_layer);

    // Add Prometheus metrics if enabled
    if state.config.enable_metrics {
        let (prometheus_layer, metric_handle) = PrometheusMetricLayer::pair();

        // Get the GenAI registry from the metrics recorder (already initialized earlier)
        let gen_ai_registry = if let Some(ref recorder) = state.metrics_recorder {
            recorder.registry().clone()
        } else {
            // Fallback: create empty registry if somehow metrics recorder wasn't initialized
            prometheus::Registry::new()
        };

        // Add metrics endpoint that combines both axum-prometheus and GenAI metrics
        router = router
            .route(
                "/internal/metrics",
                get(|| async move {
                    use prometheus::{Encoder, TextEncoder};

                    // Get axum-prometheus metrics
                    let mut axum_metrics = metric_handle.render();

                    // Get GenAI metrics
                    let encoder = TextEncoder::new();
                    let gen_ai_families = gen_ai_registry.gather();
                    let mut gen_ai_buffer = vec![];
                    encoder.encode(&gen_ai_families, &mut gen_ai_buffer).unwrap();

                    // Combine both
                    axum_metrics.push_str(&String::from_utf8_lossy(&gen_ai_buffer));
                    axum_metrics
                }),
            )
            .layer(prometheus_layer);
    }

    // Add tracing layer
    let router = router.layer(
        TraceLayer::new_for_http()
            .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
            .on_request(DefaultOnRequest::new().level(Level::INFO))
            .on_response(DefaultOnResponse::new().level(Level::INFO)),
    );

    Ok(router)
}

/// Container for background services and their lifecycle management.
///
/// This struct encapsulates all background tasks that run alongside the HTTP server,
/// including:
/// - Fusillade batch request processing daemon
/// - Probe scheduler for health monitoring
/// - Onwards configuration synchronization
/// - Leader election coordination (in distributed mode)
///
/// # Graceful Shutdown
///
/// The struct provides a [`shutdown`](BackgroundServices::shutdown) method to gracefully
/// stop all background tasks. When dropped, the `drop_guard` will automatically cancel
/// the shutdown token, signaling all tasks to stop.
pub struct BackgroundServices {
    request_manager: Arc<fusillade::PostgresRequestManager<fusillade::ReqwestHttpClient>>,
    is_leader: bool,
    onwards_targets: onwards::target::Targets,
    background_tasks: Vec<tokio::task::JoinHandle<()>>,
    shutdown_token: tokio_util::sync::CancellationToken,
    // Pub so that we can disarm it if we want to
    pub drop_guard: Option<tokio_util::sync::DropGuard>,
}

impl BackgroundServices {
    /// Gracefully shutdown all background tasks
    pub async fn shutdown(self) {
        // Signal all background tasks to shutdown
        self.shutdown_token.cancel();

        // Wait for all background tasks to complete
        for handle in self.background_tasks {
            let _ = handle.await;
        }
    }
}

/// Setup background services (probe scheduler, batch daemon, leader election, onwards integration)
async fn setup_background_services(
    pool: PgPool,
    fusillade_pool: PgPool,
    config: Config,
    shutdown_token: tokio_util::sync::CancellationToken,
) -> anyhow::Result<BackgroundServices> {
    let drop_guard = shutdown_token.clone().drop_guard();
    // Track all background task handles for graceful shutdown
    let mut background_tasks = Vec::new();

    // Create shared model capacity limits map for daemon coordination
    // This is populated by onwards config sync and read by fusillade daemon
    let model_capacity_limits = Arc::new(dashmap::DashMap::new());

    // Start onwards integration for proxying AI requests
    let (onwards_config_sync, initial_targets, onwards_stream) =
        sync::onwards_config::OnwardsConfigSync::new_with_daemon_limits(pool.clone(), Some(model_capacity_limits.clone())).await?;

    // Clone targets for the update task
    let targets_for_updates = initial_targets.clone();

    // Start target updates (infallible task, handle internally)
    let handle = tokio::spawn(async move {
        let _ = targets_for_updates.receive_updates(onwards_stream).await;
    });
    background_tasks.push(handle);

    // Start the onwards configuration listener
    let onwards_shutdown = shutdown_token.clone();
    let handle = tokio::spawn(async move {
        info!("Starting onwards configuration listener");
        if let Err(e) = onwards_config_sync.start(Default::default(), onwards_shutdown).await {
            tracing::error!("Onwards configuration listener error: {}", e);
        }
    });
    background_tasks.push(handle);
    // Leader election lock ID: 0x44574354_50524F42 (DWCT_PROB in hex for "dwctl probes")
    const LEADER_LOCK_ID: i64 = 0x4457_4354_5052_4F42_i64;

    let probe_scheduler = probes::ProbeScheduler::new(pool.clone(), config.clone());

    // Initialize the fusillade request manager (for batch processing)
    let request_manager = Arc::new(
        fusillade::PostgresRequestManager::new(fusillade_pool)
            .with_config(
                config
                    .batches
                    .daemon
                    .to_fusillade_config_with_limits(Some(model_capacity_limits.clone())),
            )
            .with_download_buffer_size(config.batches.files.download_buffer_size),
    );

    let is_leader: bool;

    if !config.leader_election.enabled {
        info!("Launching without leader election: running as leader");
        // Skip leader election - just become leader immediately
        is_leader = true;
        probe_scheduler.initialize(shutdown_token.clone()).await?;

        // Start the scheduler daemon in the background
        let daemon_scheduler = probe_scheduler.clone();
        let daemon_shutdown = shutdown_token.clone();
        let handle = tokio::spawn(async move {
            // Use LISTEN/NOTIFY in production, but disable in tests to avoid hangs
            let use_listen_notify = !cfg!(test);
            daemon_scheduler.run_daemon(daemon_shutdown, use_listen_notify, 300).await;
            // Fallback sync every 5 minutes
        });
        background_tasks.push(handle);

        // Start the fusillade batch processing daemon based on config
        use crate::config::DaemonEnabled;
        use fusillade::DaemonExecutor;
        match config.batches.daemon.enabled {
            DaemonEnabled::Always | DaemonEnabled::Leader => {
                let daemon_handle = request_manager.clone().run(shutdown_token.clone())?;
                // Wrap the handle to convert Result<()> to ()
                let handle = tokio::spawn(async move {
                    let _ = daemon_handle.await;
                });
                background_tasks.push(handle);
                info!("Skipping leader election - running as leader with probe scheduler and fusillade daemon");
            }
            DaemonEnabled::Never => {
                info!("Skipping leader election - running as leader with probe scheduler (fusillade daemon disabled)");
            }
        }
    } else {
        // Normal leader election
        is_leader = false;
        info!("Starting leader election - will attempt to acquire leadership");

        // If daemon is set to "Always", start it immediately regardless of leader election
        use crate::config::DaemonEnabled;
        if config.batches.daemon.enabled == DaemonEnabled::Always {
            use fusillade::DaemonExecutor;
            let daemon_handle = request_manager.clone().run(shutdown_token.clone())?;
            // Wrap the handle to convert Result<()> to ()
            let handle = tokio::spawn(async move {
                let _ = daemon_handle.await;
            });
            background_tasks.push(handle);
            info!("Fusillade batch daemon started (configured to always run)");
        }

        let is_leader_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Spawn leader election background task
        let leader_election_pool = pool.clone();
        let leader_election_scheduler_gain = probe_scheduler.clone();
        let leader_election_scheduler_lose = probe_scheduler.clone();
        let leader_election_request_manager_gain = request_manager.clone();
        let leader_election_config = config.clone();
        let leader_election_flag = is_leader_flag.clone();

        // Store daemon handle for cleanup on leadership loss
        let daemon_handle: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<fusillade::Result<()>>>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        let daemon_handle_gain = daemon_handle.clone();
        let daemon_handle_lose = daemon_handle.clone();

        // Store leadership session shutdown token for cleanup on leadership loss
        let leadership_shutdown: Arc<tokio::sync::Mutex<Option<tokio_util::sync::CancellationToken>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        let leadership_shutdown_gain = leadership_shutdown.clone();
        let leadership_shutdown_lose = leadership_shutdown.clone();

        let leader_election_shutdown = shutdown_token.clone();
        let handle = tokio::spawn(async move {
            leader_election::leader_election_task(
                leader_election_pool,
                leader_election_config,
                leader_election_flag,
                LEADER_LOCK_ID,
                leader_election_shutdown,
                move |_pool, config| {
                    // This closure is run when a replica becomes the leader
                    let scheduler = leader_election_scheduler_gain.clone();
                    let request_manager = leader_election_request_manager_gain.clone();
                    let daemon_handle = daemon_handle_gain.clone();
                    let leadership_shutdown = leadership_shutdown_gain.clone();
                    async move {
                        // Wait for the server to be fully up before starting probes
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                        // Create a new cancellation token for this leadership session
                        let session_token = tokio_util::sync::CancellationToken::new();
                        *leadership_shutdown.lock().await = Some(session_token.clone());

                        scheduler
                            .initialize(session_token.clone())
                            .await
                            .map_err(|e| anyhow::anyhow!("Failed to initialize probe scheduler: {}", e))?;

                        // Start the probe scheduler daemon in the background
                        let daemon_scheduler = scheduler.clone();
                        let daemon_session_token = session_token.clone();
                        tokio::spawn(async move {
                            let use_listen_notify = !cfg!(test);
                            daemon_scheduler.run_daemon(daemon_session_token, use_listen_notify, 300).await;
                        });

                        // Start the fusillade batch processing daemon based on config
                        use crate::config::DaemonEnabled;
                        use fusillade::DaemonExecutor;
                        match config.batches.daemon.enabled {
                            DaemonEnabled::Always | DaemonEnabled::Leader => {
                                let handle = request_manager
                                    .run(session_token.clone())
                                    .map_err(|e| anyhow::anyhow!("Failed to start fusillade daemon: {}", e))?;

                                // Store the handle so we can abort it when losing leadership
                                *daemon_handle.lock().await = Some(handle);

                                tracing::info!("Fusillade batch daemon started on elected leader");
                            }
                            DaemonEnabled::Never => {
                                tracing::info!("Fusillade batch daemon disabled by configuration");
                            }
                        }

                        Ok(())
                    }
                },
                move |_pool, _config| {
                    // This closure is run when a replica stops being the leader
                    let scheduler = leader_election_scheduler_lose.clone();
                    let daemon_handle = daemon_handle_lose.clone();
                    let leadership_shutdown = leadership_shutdown_lose.clone();
                    async move {
                        // Cancel the leadership session token first, which will stop all background tasks gracefully
                        if let Some(token) = leadership_shutdown.lock().await.take() {
                            token.cancel();
                        }

                        // Now stop all schedulers (this will be faster since they're already shutting down)
                        scheduler
                            .stop_all()
                            .await
                            .map_err(|e| anyhow::anyhow!("Failed to stop probe scheduler: {}", e))?;

                        // Stop the fusillade daemon
                        if let Some(handle) = daemon_handle.lock().await.take() {
                            handle.abort();
                            tracing::info!("Fusillade batch daemon stopped (lost leadership)");
                        }

                        Ok(())
                    }
                },
            )
            .await;
        });
        background_tasks.push(handle);
    }

    Ok(BackgroundServices {
        request_manager,
        is_leader,
        onwards_targets: initial_targets,
        background_tasks,
        shutdown_token,
        drop_guard: Some(drop_guard),
    })
}

/// Main application struct that owns all resources and lifecycle.
///
/// This is the top-level container for the entire application, managing:
/// - HTTP server and routing
/// - Database connections (main, fusillade, outlet, embedded)
/// - Application configuration
/// - Background services (probes, batches, leader election)
///
/// # Lifecycle
///
/// 1. **Create**: [`Application::new`] initializes all resources, runs migrations,
///    seeds the database, and starts background services
/// 2. **Serve**: [`Application::serve`] binds to a TCP port and starts handling requests
/// 3. **Shutdown**: When the shutdown signal is received, gracefully stops all services
/// ```
pub struct Application {
    router: Router,
    app_state: AppState,
    config: Config,
    pool: PgPool,
    _fusillade_pool: PgPool,
    _outlet_pool: Option<PgPool>,
    _embedded_db: Option<db::embedded::EmbeddedDatabase>,
    bg_services: BackgroundServices,
}

impl Application {
    /// Create a new application instance with all resources initialized
    pub async fn new(config: Config) -> anyhow::Result<Self> {
        debug!("Starting control layer with configuration: {:#?}", config);

        // Setup database connections, run migrations, and initialize data
        let (_embedded_db, pool, fusillade_pool, outlet_pool) = setup_database(&config).await?;

        // Create a shutdown token for coordinating graceful shutdown of background tasks
        let shutdown_token = tokio_util::sync::CancellationToken::new();

        // Setup background services (onwards integration, probe scheduler, batch daemon, leader election)
        let bg_services = setup_background_services(pool.clone(), fusillade_pool.clone(), config.clone(), shutdown_token.clone()).await?;

        // Build onwards router from targets
        let onwards_app_state = onwards::AppState::new(bg_services.onwards_targets.clone());
        let onwards_router = onwards::build_router(onwards_app_state);

        // Build app state and router
        let mut app_state = AppState::builder()
            .db(pool.clone())
            .config(config.clone())
            .is_leader(bg_services.is_leader)
            .request_manager(bg_services.request_manager.clone())
            .maybe_outlet_db(outlet_pool.clone())
            .build();

        let router = build_router(&mut app_state, onwards_router).await?;

        Ok(Self {
            router,
            app_state,
            config,
            pool,
            _fusillade_pool: fusillade_pool,
            _outlet_pool: outlet_pool,
            _embedded_db,
            bg_services,
        })
    }

    /// Convert application into a test server (for tests)
    #[cfg(test)]
    pub fn into_test_server(self) -> (axum_test::TestServer, BackgroundServices) {
        // Apply middleware before path matching for tests
        let middleware = from_fn_with_state(self.app_state, admin_ai_proxy_middleware);
        let service = middleware.layer(self.router).into_make_service();
        let server = axum_test::TestServer::new(service).expect("Failed to create test server");
        (server, self.bg_services)
    }

    /// Start serving the application
    pub async fn serve<F>(self, shutdown: F) -> anyhow::Result<()>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let bind_addr = self.config.bind_address();
        let listener = TcpListener::bind(&bind_addr).await?;
        info!(
            "Control layer listening on http://{}, available at http://localhost:{}",
            bind_addr, self.config.port
        );

        // Apply middleware before path matching
        let middleware = from_fn_with_state(self.app_state, admin_ai_proxy_middleware);
        let service = middleware.layer(self.router);

        // Run the server with graceful shutdown
        axum::serve(listener, service.into_make_service())
            .with_graceful_shutdown(shutdown)
            .await?;

        // Shutdown background services and wait for tasks to complete
        self.bg_services.shutdown().await;

        // Close database connections
        info!("Closing database connections...");
        self.pool.close().await;

        // Shutdown telemetry
        info!("Shutting down telemetry...");
        telemetry::shutdown_telemetry();

        // Clean up embedded database if it exists
        if let Some(embedded_db) = self._embedded_db {
            info!("Shutting down embedded database...");
            embedded_db.stop().await?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::{create_initial_admin_user, AppState};
    use crate::{
        api::models::users::Role,
        db::handlers::Users,
        request_logging::{AiRequest, AiResponse},
        test_utils::*,
    };
    use outlet_postgres::RequestFilter;
    use sqlx::{ConnectOptions, PgPool};

    /// Integration test: setup the whole stack, including syncing the onwards config from
    /// LISTEN/NOTIFY, and then test user access via headers to /admin/api/v1/ai
    #[sqlx::test]
    #[test_log::test]
    async fn test_admin_ai_proxy_middleware_with_user_access(pool: PgPool) {
        // Create test app (handles all setup including database seeding)
        let (server, _bg_services) = crate::test_utils::create_test_app(pool.clone(), true).await;

        // Create test users
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let regular_user = create_test_user(&pool, Role::StandardUser).await;

        // Create a group and add user
        let test_group = create_test_group(&pool).await;
        add_user_to_group(&pool, regular_user.id, test_group.id).await;

        // Create a deployment and add to group
        let deployment = create_test_deployment(&pool, admin_user.id, "test-model", "test-alias").await;
        add_deployment_to_group(&pool, deployment.id, test_group.id, admin_user.id).await;

        // Test 1: Admin AI proxy with X-Doubleword-User header (new middleware)
        let admin_proxy_response = server
            .post("/admin/api/v1/ai/v1/chat/completions")
            .add_header("x-doubleword-user", &regular_user.email)
            .json(&serde_json::json!({
                "model": deployment.alias,
                "messages": [{"role": "user", "content": "Hello via admin proxy"}]
            }))
            .await;

        // Should get to proxy through middleware (might 502 since no real backend, but auth should pass)
        println!("Valid user response status: {}", admin_proxy_response.status_code());
        assert!(
            admin_proxy_response.status_code().as_u16() != 401,
            "Admin proxy should accept user with model access"
        );

        // Test 2: Admin AI proxy with user who has no access to model
        let restricted_user = create_test_user(&pool, Role::StandardUser).await;

        let no_access_response = server
            .post("/admin/api/v1/ai/v1/chat/completions")
            .add_header("x-doubleword-user", &restricted_user.email)
            .json(&serde_json::json!({
                "model": deployment.alias,
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        // Should reject access - either 403 (Forbidden - API key not yet synced to onwards)
        // or 404 (Not Found - onwards knows about key but user has no model access)
        // The difference depends on timing of onwards config sync after hidden API key creation
        let status = no_access_response.status_code().as_u16();
        assert!(
            status == 403 || status == 404,
            "Admin proxy should reject user with no model access (got {})",
            status
        );

        // Test 3: Admin AI proxy with missing header
        let missing_header_response = server
            .post("/admin/api/v1/ai/v1/chat/completions")
            .json(&serde_json::json!({
                "model": deployment.alias,
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        // Should be unauthorized since no X-Doubleword-User header
        assert_eq!(
            missing_header_response.status_code().as_u16(),
            401,
            "Admin proxy should require X-Doubleword-User header"
        );

        // Test 4: Admin AI proxy with non-existent user
        let nonexistent_user_response = server
            .post("/admin/api/v1/ai/v1/chat/completions")
            .add_header("x-doubleword-user", "nonexistent@example.com")
            .json(&serde_json::json!({
                "model": deployment.alias,
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        // With auto_create_users, user is created but has no access, onwards returns 404
        assert_eq!(
            nonexistent_user_response.status_code().as_u16(),
            404,
            "Admin proxy should reject non-existent user"
        );

        // Test 5: Admin AI proxy with non-existent model
        let nonexistent_model_response = server
            .post("/admin/api/v1/ai/v1/chat/completions")
            .add_header("x-doubleword-user", &regular_user.email)
            .json(&serde_json::json!({
                "model": "nonexistent-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        // Should be not found since onwards returns 404 for nonexistent models
        assert_eq!(
            nonexistent_model_response.status_code().as_u16(),
            404,
            "Admin proxy should reject non-existent model"
        );

        // Manually trigger shutdown to clean up background tasks
        // bg_services.shutdown().await;
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_database_seeding_behavior(pool: PgPool) {
        use crate::config::ModelSource;
        use url::Url;
        use uuid::Uuid;

        // Create test model sources
        let sources = vec![
            ModelSource {
                name: "test-endpoint-1".to_string(),
                url: Url::parse("http://localhost:8001").unwrap(),
                api_key: None,
                sync_interval: std::time::Duration::from_secs(10),
            },
            ModelSource {
                name: "test-endpoint-2".to_string(),
                url: Url::parse("http://localhost:8002").unwrap(),
                api_key: None,
                sync_interval: std::time::Duration::from_secs(10),
            },
        ];

        // Create a system API key row to test the update behavior
        let system_api_key_id = Uuid::nil();
        let original_secret = "original_test_secret";
        sqlx::query!(
            "INSERT INTO api_keys (id, name, secret, purpose, user_id) VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (id) DO UPDATE SET secret = $3",
            system_api_key_id,
            "System API Key",
            original_secret,
            "inference",
            system_api_key_id,
        )
        .execute(&pool)
        .await
        .expect("Should be able to create system API key");

        // Verify initial state - no seeding flag set
        let initial_seeded = sqlx::query_scalar!("SELECT value FROM system_config WHERE key = 'endpoints_seeded'")
            .fetch_optional(&pool)
            .await
            .expect("Should be able to query system_config");
        assert_eq!(initial_seeded, Some(false), "Initial seeded flag should be false");

        // First call should seed both endpoints and API key
        super::seed_database(&sources, &pool).await.expect("First seeding should succeed");

        // Verify endpoints were created
        let endpoint_count =
            sqlx::query_scalar!("SELECT COUNT(*) FROM inference_endpoints WHERE name IN ('test-endpoint-1', 'test-endpoint-2')")
                .fetch_one(&pool)
                .await
                .expect("Should be able to count endpoints");
        assert_eq!(endpoint_count, Some(2), "Should have created 2 endpoints");

        // Verify API key was updated
        let updated_secret = sqlx::query_scalar!("SELECT secret FROM api_keys WHERE id = $1", system_api_key_id)
            .fetch_one(&pool)
            .await
            .expect("Should be able to get API key secret");
        assert_ne!(updated_secret, original_secret, "API key secret should have been updated");
        assert!(updated_secret.len() > 10, "New API key should be a reasonable length");

        // Verify seeded flag is now true
        let seeded_after_first = sqlx::query_scalar!("SELECT value FROM system_config WHERE key = 'endpoints_seeded'")
            .fetch_one(&pool)
            .await
            .expect("Should be able to query seeded flag");
        assert!(seeded_after_first, "Seeded flag should be true after first run");

        // Manually modify one endpoint and the API key to test non-overwrite behavior
        sqlx::query!("UPDATE inference_endpoints SET url = 'http://modified-url:9999' WHERE name = 'test-endpoint-1'")
            .execute(&pool)
            .await
            .expect("Should be able to update endpoint");

        let manual_secret = "manually_set_secret";
        sqlx::query!("UPDATE api_keys SET secret = $1 WHERE id = $2", manual_secret, system_api_key_id)
            .execute(&pool)
            .await
            .expect("Should be able to update API key");

        // Second call should skip all seeding (because seeded flag is true)
        super::seed_database(&sources, &pool)
            .await
            .expect("Second seeding should succeed but skip");

        // Verify the manual changes were NOT overwritten
        let preserved_url = sqlx::query_scalar!("SELECT url FROM inference_endpoints WHERE name = 'test-endpoint-1'")
            .fetch_one(&pool)
            .await
            .expect("Should be able to get endpoint URL");
        assert_eq!(preserved_url, "http://modified-url:9999", "Manual URL change should be preserved");

        let preserved_secret = sqlx::query_scalar!("SELECT secret FROM api_keys WHERE id = $1", system_api_key_id)
            .fetch_one(&pool)
            .await
            .expect("Should be able to get API key secret");
        assert_eq!(preserved_secret, manual_secret, "Manual API key change should be preserved");

        // Verify endpoint count is still correct
        let final_count =
            sqlx::query_scalar!("SELECT COUNT(*) FROM inference_endpoints WHERE name IN ('test-endpoint-1', 'test-endpoint-2')")
                .fetch_one(&pool)
                .await
                .expect("Should be able to count endpoints");
        assert_eq!(final_count, Some(2), "Should still have 2 endpoints");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_request_logging_enabled(pool: PgPool) {
        // Create test config with request logging enabled
        let mut config = crate::test_utils::create_test_config();
        config.enable_request_logging = true;
        config.database = crate::config::DatabaseConfig::External {
            url: pool.connect_options().to_url_lossy().to_string(),
        };
        config.leader_election.enabled = false;

        // Create application using proper setup (which will create outlet_db)
        let app = crate::Application::new(config).await.expect("Failed to create application");

        // Get outlet_db from app_state to query logs
        let outlet_pool = app.app_state.outlet_db.clone().expect("outlet_db should exist");
        let repository: outlet_postgres::RequestRepository<AiRequest, AiResponse> = outlet_postgres::RequestRepository::new(outlet_pool);

        let (server, _drop_guard) = app.into_test_server();

        // Make a test request to /ai/ endpoint which should be logged
        let _ = server.get("/ai/v1/models").await;

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let result = repository
            .query(RequestFilter {
                method: Some("GET".into()),
                ..Default::default()
            })
            .await
            .expect("Should be able to query requests");
        assert!(result.len() == 1);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_request_logging_disabled(pool: PgPool) {
        // Create test config with request logging disabled
        let mut config = crate::test_utils::create_test_config();
        config.enable_request_logging = false;

        // Build router with request logging disabled
        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let mut app_state = AppState::builder()
            .db(pool.clone())
            .config(config)
            .request_manager(request_manager)
            .build();
        let onwards_router = axum::Router::new(); // Empty onwards router for testing
        let router = super::build_router(&mut app_state, onwards_router)
            .await
            .expect("Failed to build router");

        let server = axum_test::TestServer::new(router).expect("Failed to create test server");

        // Make a test request to /healthz endpoint
        let response = server.get("/healthz").await;
        assert_eq!(response.status_code().as_u16(), 200);
        assert_eq!(response.text(), "OK");

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Verify that no outlet schema or tables exist when logging is disabled
        let schema_exists =
            sqlx::query_scalar::<_, Option<i64>>("SELECT COUNT(*) FROM information_schema.schemata WHERE schema_name = 'outlet'")
                .fetch_one(&pool)
                .await
                .expect("Should be able to query information_schema");

        if schema_exists.unwrap_or(0) == 0 {
            // Schema doesn't exist, which is expected when logging is disabled
            return;
        } else {
            panic!("Outlet schema should not exist when request logging is disabled");
        }
    }

    #[sqlx::test]
    async fn test_create_initial_admin_user_new_user(pool: PgPool) {
        let test_email = "new-admin@example.com";

        // User should not exist initially
        let mut user_conn = pool.acquire().await.unwrap();
        let mut users_repo = Users::new(&mut user_conn);
        let initial_user = users_repo.get_user_by_email(test_email).await;
        assert!(initial_user.is_err() || initial_user.unwrap().is_none());

        // Create the initial admin user
        let user_id = create_initial_admin_user(test_email, None, &pool)
            .await
            .expect("Should create admin user successfully");

        // Verify user was created with correct properties
        let created_user = users_repo
            .get_user_by_email(test_email)
            .await
            .expect("Should be able to query user")
            .expect("User should exist");

        assert_eq!(created_user.id, user_id);
        assert_eq!(created_user.email, test_email);
        assert_eq!(created_user.username, test_email);
        assert!(created_user.is_admin);
        assert_eq!(created_user.auth_source, "system");
        assert!(created_user.roles.contains(&Role::PlatformManager));
    }

    #[sqlx::test]
    async fn test_create_initial_admin_user_existing_user(pool: PgPool) {
        let test_email = "existing-admin@example.com";

        // Create user first with create_test_admin_user
        let existing_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let existing_user_id = existing_user.id;

        // Update the user's email to our test email to simulate an existing admin
        sqlx::query!("UPDATE users SET email = $1 WHERE id = $2", test_email, existing_user_id)
            .execute(&pool)
            .await
            .expect("Should update user email");

        // Call create_initial_admin_user - should be idempotent
        let returned_user_id = create_initial_admin_user(test_email, None, &pool)
            .await
            .expect("Should handle existing user successfully");

        // Should return the existing user's ID
        assert_eq!(returned_user_id, existing_user_id);

        // User should still exist and be admin
        let mut user_conn2 = pool.acquire().await.unwrap();
        let mut users_repo = Users::new(&mut user_conn2);
        let user = users_repo
            .get_user_by_email(test_email)
            .await
            .expect("Should be able to query user")
            .expect("User should still exist");

        assert_eq!(user.id, existing_user_id);
        assert!(user.is_admin);
        assert!(user.roles.contains(&Role::PlatformManager));
    }

    #[tokio::test]
    async fn test_openapi_yaml_endpoint() {
        // Create a simple test router with just the openapi endpoint
        let router = axum::Router::new().route(
            "/openai-openapi.yaml",
            axum::routing::get(|| async {
                const OPENAPI_SPEC: &str = include_str!("openai-openapi.yaml");
                (axum::http::StatusCode::OK, [("content-type", "application/yaml")], OPENAPI_SPEC)
            }),
        );

        let server = axum_test::TestServer::new(router).expect("Failed to create test server");
        let response = server.get("/openai-openapi.yaml").await;

        assert_eq!(response.status_code().as_u16(), 200);
        assert_eq!(response.headers().get("content-type").unwrap(), "application/yaml");

        let content = response.text();
        assert!(!content.is_empty());
        // Should contain YAML content (check for openapi version)
        assert!(content.contains("openapi:") || content.contains("swagger:"));
    }

    #[sqlx::test]
    async fn test_application_integration(pool: PgPool) {
        let mut config = create_test_config();
        config.database = crate::config::DatabaseConfig::External {
            url: pool.connect_options().to_url_lossy().to_string(),
        };
        config.leader_election.enabled = false;

        // Create application
        let app = crate::Application::new(config).await;
        assert!(app.is_ok(), "Application::new should succeed");

        let (server, _drop_guard) = app.unwrap().into_test_server();

        // Test that basic routes work
        let health_response = server.get("/healthz").await;
        assert_eq!(health_response.status_code().as_u16(), 200);
        assert_eq!(health_response.text(), "OK");

        // Test openapi endpoint
        let openapi_response = server.get("/openai-openapi.yaml").await;
        assert_eq!(openapi_response.status_code().as_u16(), 200);
        assert_eq!(openapi_response.headers().get("content-type").unwrap(), "application/yaml");

        // Test that API routes exist (should require auth)
        let api_response = server.get("/admin/api/v1/users").await;
        // Should get unauthorized (401) since no auth header provided
        assert_eq!(api_response.status_code().as_u16(), 401);
    }

    #[sqlx::test]
    async fn test_build_router_with_metrics_disabled(pool: PgPool) {
        let mut config = create_test_config();
        config.enable_metrics = false;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let mut app_state = AppState::builder().db(pool).config(config).request_manager(request_manager).build();

        let onwards_router = axum::Router::new();
        let router = super::build_router(&mut app_state, onwards_router)
            .await
            .expect("Failed to build router");
        let server = axum_test::TestServer::new(router).expect("Failed to create test server");

        // Metrics endpoint should not exist - falls through to SPA fallback
        let metrics_response = server.get("/internal/metrics").await;
        let metrics_content = metrics_response.text();
        // Should not contain Prometheus metrics format
        assert!(!metrics_content.contains("# HELP") && !metrics_content.contains("# TYPE"));
    }

    #[sqlx::test]
    async fn test_build_router_with_metrics_enabled(pool: PgPool) {
        let mut config = create_test_config();
        config.enable_metrics = true;

        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(pool.clone()));
        let mut app_state = AppState::builder().db(pool).config(config).request_manager(request_manager).build();

        let onwards_router = axum::Router::new();
        let router = super::build_router(&mut app_state, onwards_router)
            .await
            .expect("Failed to build router");
        let server = axum_test::TestServer::new(router).expect("Failed to create test server");

        // Metrics endpoint should exist and return Prometheus format
        let metrics_response = server.get("/internal/metrics").await;
        assert_eq!(metrics_response.status_code().as_u16(), 200);

        let metrics_content = metrics_response.text();
        // Should contain Prometheus metrics format
        assert!(metrics_content.contains("# HELP") || metrics_content.contains("# TYPE"));
    }
}
