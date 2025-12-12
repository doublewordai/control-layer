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
mod error_enrichment;
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

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

use crate::{
    api::models::{deployments::DeployedModelCreate, users::Role},
    auth::password,
    config::CorsOrigin,
    db::handlers::{Deployments, Groups, Repository, Users},
    db::models::{deployments::DeploymentCreateDBRequest, users::UserCreateDBRequest},
    metrics::GenAiMetrics,
    openapi::ApiDoc,
    request_logging::serializers::{AnalyticsResponseSerializer, parse_ai_request},
};
use anyhow::Context;
use auth::middleware::admin_ai_proxy_middleware;
use axum::extract::DefaultBodyLimit;
use axum::http::HeaderValue;
use axum::{
    Router, ServiceExt, http, middleware,
    routing::{delete, get, patch, post},
};
use axum_prometheus::PrometheusMetricLayer;
use bon::Builder;
pub use config::Config;
use outlet::{RequestLoggerConfig, RequestLoggerLayer};
use outlet_postgres::PostgresHandler;
use request_logging::{AiResponse, ParsedAIRequest};
use sqlx::{Executor, PgPool};
use std::sync::Arc;
use tokio::net::TcpListener;
use tower::Layer;
use tower_http::{
    cors::CorsLayer,
    trace::{DefaultMakeSpan, DefaultOnRequest, DefaultOnResponse, TraceLayer},
};
use tracing::{Level, debug, info, instrument};
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
/// # use dwctl::auth::password::Argon2Params;
/// # async fn example(pool: PgPool) -> Result<(), sqlx::Error> {
/// let user_id = create_initial_admin_user(
///     "admin@example.com",
///     Some("secure_password"),
///     Argon2Params::default(),
///     &pool
/// ).await?;
/// # Ok(())
/// # }
/// ```
#[instrument(skip_all)]
pub async fn create_initial_admin_user(
    email: &str,
    password: Option<&str>,
    argon2_params: password::Argon2Params,
    db: &PgPool,
) -> Result<UserId, sqlx::Error> {
    // Hash password if provided
    let password_hash = if let Some(pwd) = password {
        Some(
            password::hash_string_with_params(pwd, Some(argon2_params))
                .map_err(|e| sqlx::Error::Encode(format!("Failed to hash admin password: {e}").into()))?,
        )
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
        external_user_id: None,
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
        if let Some(endpoint_id) = sqlx::query_scalar!(
            "INSERT INTO inference_endpoints (name, description, url, created_by)
            VALUES ($1, $2, $3, $4)
            ON CONFLICT (name) DO NOTHING
            RETURNING id",
            source.name,
            None::<String>, // System-created endpoints don't have descriptions
            source.url.as_str(),
            system_user_id,
        )
        .fetch_optional(&mut *tx)
        .await?
        {
            for model in source.default_models.clone().unwrap_or(vec![]) {
                // Insert deployed model if it doesn't already exist
                let mut model_repo = Deployments::new(&mut *tx);
                if let Ok(row) = model_repo
                    .create(&DeploymentCreateDBRequest::from_api_create(
                        Uuid::nil(),
                        DeployedModelCreate {
                            model_name: model.name.clone(),
                            alias: Some(model.name.clone()),
                            hosted_on: endpoint_id,
                            description: None,
                            model_type: None,
                            capabilities: None,
                            requests_per_second: None,
                            burst_size: None,
                            capacity: None,
                            batch_capacity: None,
                            pricing: None,
                            downstream_pricing: None,
                        },
                    ))
                    .await
                {
                    if model.add_to_everyone_group {
                        let mut groups_repo = Groups::new(&mut *tx);
                        if let Err(e) = groups_repo.add_deployment_to_group(row.id, Uuid::nil(), Uuid::nil()).await {
                            debug!("Failed to add deployed model {} to 'everyone' group during seeding: {}", model.name, e);
                        }
                    }
                }
            }
        }
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
///
/// If `pool` is provided, it will be used directly instead of creating a new connection.
/// This is useful for tests where sqlx::test provides a pool.
async fn setup_database(
    config: &Config,
    pool: Option<PgPool>,
) -> anyhow::Result<(Option<db::embedded::EmbeddedDatabase>, PgPool, PgPool, Option<PgPool>)> {
    // If a pool is provided (e.g., from tests), use it directly
    let (embedded_db, pool) = if let Some(existing_pool) = pool {
        info!("Using provided database pool");
        (None, existing_pool)
    } else {
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
            config::DatabaseConfig::External { url, .. } => {
                info!("Using external database");
                (None::<db::embedded::EmbeddedDatabase>, url.clone())
            }
        };

        let pool_config = config.database.pool_config();
        let main_settings = &pool_config.main;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(main_settings.max_connections)
            .min_connections(main_settings.min_connections)
            .acquire_timeout(std::time::Duration::from_secs(main_settings.acquire_timeout_secs))
            .idle_timeout(if main_settings.idle_timeout_secs > 0 {
                Some(std::time::Duration::from_secs(main_settings.idle_timeout_secs))
            } else {
                None
            })
            .max_lifetime(if main_settings.max_lifetime_secs > 0 {
                Some(std::time::Duration::from_secs(main_settings.max_lifetime_secs))
            } else {
                None
            })
            .connect(&database_url)
            .await?;
        (_embedded_db, pool)
    };

    migrator().run(&pool).await?;

    // Get connection options from the main pool to create child pools
    let connect_opts = pool.connect_options().as_ref().clone();

    // Get pool configuration
    let pool_config = config.database.pool_config();

    // Setup fusillade schema and pool (only if batches are enabled)
    info!("Setting up fusillade batch processing pool (batches enabled)");
    let fusillade_settings = &pool_config.fusillade;
    let fusillade_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(fusillade_settings.max_connections)
        .min_connections(fusillade_settings.min_connections)
        .acquire_timeout(std::time::Duration::from_secs(fusillade_settings.acquire_timeout_secs))
        .idle_timeout(if fusillade_settings.idle_timeout_secs > 0 {
            Some(std::time::Duration::from_secs(fusillade_settings.idle_timeout_secs))
        } else {
            None
        })
        .max_lifetime(if fusillade_settings.max_lifetime_secs > 0 {
            Some(std::time::Duration::from_secs(fusillade_settings.max_lifetime_secs))
        } else {
            None
        })
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                // Set search path to fusillade schema for all connections in this pool
                conn.execute("SET search_path = 'fusillade'").await?;
                Ok(())
            })
        })
        .connect_lazy_with(connect_opts.clone());

    fusillade_pool.execute("CREATE SCHEMA IF NOT EXISTS fusillade").await?;
    fusillade::migrator().run(&fusillade_pool).await?;

    // Setup outlet schema and pool if request logging is enabled
    let outlet_pool = if config.enable_request_logging {
        info!("Setting up outlet request logging pool (logging enabled)");
        let outlet_settings = &pool_config.outlet;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(outlet_settings.max_connections)
            .min_connections(outlet_settings.min_connections)
            .acquire_timeout(std::time::Duration::from_secs(outlet_settings.acquire_timeout_secs))
            .idle_timeout(if outlet_settings.idle_timeout_secs > 0 {
                Some(std::time::Duration::from_secs(outlet_settings.idle_timeout_secs))
            } else {
                None
            })
            .max_lifetime(if outlet_settings.max_lifetime_secs > 0 {
                Some(std::time::Duration::from_secs(outlet_settings.max_lifetime_secs))
            } else {
                None
            })
            .after_connect(|conn, _meta| {
                Box::pin(async move {
                    // Set search path to outlet schema for all connections in this pool
                    conn.execute("SET search_path = 'outlet'").await?;
                    Ok(())
                })
            })
            .connect_lazy_with(connect_opts.clone());

        pool.execute("CREATE SCHEMA IF NOT EXISTS outlet").await?;
        outlet_postgres::migrator().run(&pool).await?;

        Some(pool)
    } else {
        info!("Skipping outlet pool setup (logging disabled)");
        None
    };

    // Create initial admin user if it doesn't exist
    let argon2_params = password::Argon2Params {
        memory_kib: config.auth.native.password.argon2_memory_kib,
        iterations: config.auth.native.password.argon2_iterations,
        parallelism: config.auth.native.password.argon2_parallelism,
    };
    create_initial_admin_user(&config.admin_email, config.admin_password.as_deref(), argon2_params, &pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create initial admin user: {}", e))?;

    // Seed database with initial configuration (only runs once)
    seed_database(&config.model_sources, &pool).await?;

    Ok((embedded_db, pool, fusillade_pool, outlet_pool))
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

        let postgres_handler = PostgresHandler::<ParsedAIRequest, AiResponse>::from_pool(outlet_pool.clone())
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
                .route("/files/{file_id}/cost-estimate", get(api::handlers::files::get_file_cost_estimate))
                // Batches management
                .route("/batches", post(api::handlers::batches::create_batch))
                .route("/batches", get(api::handlers::batches::list_batches))
                .route("/batches/{batch_id}", get(api::handlers::batches::get_batch))
                .route("/batches/{batch_id}/analytics", get(api::handlers::batches::get_batch_analytics))
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

    // Apply error enrichment middleware to onwards router (before outlet logging)
    let onwards_router = onwards_router.layer(middleware::from_fn_with_state(
        state.db.clone(),
        error_enrichment::error_enrichment_middleware,
    ));

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
    #[cfg_attr(not(test), allow(dead_code))]
    onwards_sender: Option<tokio::sync::watch::Sender<onwards::target::Targets>>,
    // JoinSet is cancel-safe - can be polled in select! without losing tasks
    background_tasks: tokio::task::JoinSet<anyhow::Result<()>>,
    // Map task IDs to names for logging
    task_names: std::collections::HashMap<tokio::task::Id, &'static str>,
    shutdown_token: tokio_util::sync::CancellationToken,
    // Pub so that we can disarm it if we want to
    pub drop_guard: Option<tokio_util::sync::DropGuard>,
}

impl BackgroundServices {
    /// Wait for any background task to complete (indicating a failure)
    /// This method is cancel-safe - can be used in tokio::select! without losing tasks
    /// Returns an error with details about which task failed
    pub async fn wait_for_failure(&mut self) -> anyhow::Result<std::convert::Infallible> {
        match self.background_tasks.join_next_with_id().await {
            None => {
                // No background tasks - wait forever
                futures::future::pending::<()>().await;
                unreachable!()
            }
            Some(Ok((task_id, Ok(())))) => {
                let task_name = self.task_names.get(&task_id).copied().unwrap_or("unknown");
                tracing::warn!(task = task_name, "Background task completed unexpectedly");
                anyhow::bail!("Background task '{}' completed early", task_name)
            }
            Some(Ok((task_id, Err(e)))) => {
                let task_name = self.task_names.get(&task_id).copied().unwrap_or("unknown");
                tracing::error!(task = task_name, error = %e, "Background task failed");
                anyhow::bail!("Background task '{}' failed: {}", task_name, e)
            }
            Some(Err(e)) => {
                let task_id = e.id();
                let task_name = self.task_names.get(&task_id).copied().unwrap_or("unknown");
                tracing::error!(task = task_name, error = %e, "Background task panicked");
                anyhow::bail!("Background task '{}' panicked: {}", task_name, e)
            }
        }
    }

    /// Gracefully shutdown all background tasks
    pub async fn shutdown(mut self) {
        // Signal all background tasks to shutdown
        self.shutdown_token.cancel();

        // Wait for all background tasks to complete and check for errors
        while let Some(result) = self.background_tasks.join_next_with_id().await {
            match result {
                Ok((task_id, Ok(()))) => {
                    let task_name = self.task_names.get(&task_id).copied().unwrap_or("unknown");
                    tracing::debug!(task = task_name, "Background task completed successfully");
                }
                Ok((task_id, Err(e))) => {
                    let task_name = self.task_names.get(&task_id).copied().unwrap_or("unknown");
                    tracing::error!(task = task_name, error = %e, "Background task failed");
                }
                Err(e) => {
                    let task_id = e.id();
                    let task_name = self.task_names.get(&task_id).copied().unwrap_or("unknown");
                    tracing::error!(task = task_name, error = %e, "Background task panicked");
                }
            }
        }
    }

    /// Manually trigger a sync of onwards targets (for testing)
    /// This reloads the configuration from the database and updates the live routing
    /// Uses the same codepath as the automatic LISTEN/NOTIFY sync
    #[cfg(test)]
    pub async fn sync_onwards_config(&self, pool: &sqlx::PgPool) -> anyhow::Result<()> {
        let sender = self
            .onwards_sender
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Onwards sync not enabled"))?;

        // Use the same load function as the automatic sync
        let new_targets = crate::sync::onwards_config::load_targets_from_db(pool).await?;

        // Send through the watch channel (same as automatic sync)
        sender
            .send(new_targets)
            .map_err(|_| anyhow::anyhow!("Failed to send targets update"))?;

        Ok(())
    }
}

/// Helper for spawning named background tasks during setup
struct BackgroundTaskBuilder {
    tasks: tokio::task::JoinSet<anyhow::Result<()>>,
    names: std::collections::HashMap<tokio::task::Id, &'static str>,
}

impl BackgroundTaskBuilder {
    fn new() -> Self {
        Self {
            tasks: tokio::task::JoinSet::new(),
            names: std::collections::HashMap::new(),
        }
    }

    fn spawn<F>(&mut self, name: &'static str, future: F)
    where
        F: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let abort_handle = self.tasks.spawn(future);
        self.names.insert(abort_handle.id(), name);
    }

    fn into_parts(
        self,
    ) -> (
        tokio::task::JoinSet<anyhow::Result<()>>,
        std::collections::HashMap<tokio::task::Id, &'static str>,
    ) {
        (self.tasks, self.names)
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
    let mut background_tasks = BackgroundTaskBuilder::new();

    // Create shared model capacity limits map for daemon coordination
    // This is populated by onwards config sync and read by fusillade daemon
    let model_capacity_limits = Arc::new(dashmap::DashMap::new());

    // Start onwards integration for proxying AI requests (if enabled)
    #[cfg_attr(not(test), allow(unused_variables))]
    let (initial_targets, onwards_sender) = if config.background_services.onwards_sync.enabled {
        let (onwards_config_sync, initial_targets, onwards_stream) =
            sync::onwards_config::OnwardsConfigSync::new_with_daemon_limits(pool.clone(), Some(model_capacity_limits.clone())).await?;

        // Clone the sender before moving onwards_config_sync into the spawn (for manual sync)
        let sender = onwards_config_sync.sender();

        // Start target updates - this spawns a background task internally and returns immediately
        initial_targets
            .receive_updates(onwards_stream)
            .await
            .map_err(anyhow::Error::from)
            .context("Onwards target updates failed")?;

        // Start the onwards configuration listener
        let onwards_shutdown = shutdown_token.clone();
        background_tasks.spawn("onwards-config-sync", async move {
            info!("Starting onwards configuration listener");
            onwards_config_sync
                .start(Default::default(), onwards_shutdown)
                .await
                .context("Onwards configuration listener failed")
        });

        (initial_targets, Some(sender))
    } else {
        info!("Onwards config sync disabled - AI proxy will not receive config updates");
        // Create empty targets when onwards sync is disabled
        let empty_config = onwards::target::ConfigFile {
            targets: std::collections::HashMap::new(),
            auth: None,
        };
        (onwards::target::Targets::from_config(empty_config)?, None)
    };
    // Leader election lock ID: 0x44574354_50524F42 (DWCT_PROB in hex for "dwctl probes")
    const LEADER_LOCK_ID: i64 = 0x4457_4354_5052_4F42_i64;

    let probe_scheduler = probes::ProbeScheduler::new(pool.clone(), config.clone());

    // Initialize the fusillade request manager (for batch processing)
    let request_manager = Arc::new(
        fusillade::PostgresRequestManager::new(fusillade_pool)
            .with_config(
                config
                    .background_services
                    .batch_daemon
                    .to_fusillade_config_with_limits(Some(model_capacity_limits.clone())),
            )
            .with_download_buffer_size(config.batches.files.download_buffer_size),
    );

    let is_leader: bool;

    if !config.background_services.leader_election.enabled {
        info!("Launching without leader election: running as leader");
        // Skip leader election - just become leader immediately
        is_leader = true;

        // Start probe scheduler if enabled
        if config.background_services.probe_scheduler.enabled {
            probe_scheduler.initialize(shutdown_token.clone()).await?;

            // Start the scheduler daemon in the background
            let daemon_scheduler = probe_scheduler.clone();
            let daemon_shutdown = shutdown_token.clone();
            background_tasks.spawn("probe-scheduler", async move {
                // Use LISTEN/NOTIFY in production, but disable in tests to avoid hangs
                let use_listen_notify = !cfg!(test);
                daemon_scheduler.run_daemon(daemon_shutdown, use_listen_notify, 300).await;
                // Probe scheduler runs until cancelled, then exits normally
                Ok(())
            });
        } else {
            info!("Probe scheduler disabled by configuration");
        }

        // Start the fusillade batch processing daemon based on config
        use crate::config::DaemonEnabled;
        use fusillade::DaemonExecutor;
        match config.background_services.batch_daemon.enabled {
            DaemonEnabled::Always | DaemonEnabled::Leader => {
                let daemon_handle = request_manager.clone().run(shutdown_token.clone())?;
                // Spawn task that propagates daemon errors
                background_tasks.spawn("fusillade-daemon", async move {
                    match daemon_handle.await {
                        Ok(Ok(())) => {
                            tracing::info!("Fusillade daemon exited normally");
                        }
                        Ok(Err(e)) => {
                            tracing::error!(error = %e, "Fusillade daemon failed");
                            anyhow::bail!("Fusillade daemon error: {}", e);
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "Fusillade daemon task panicked");
                            anyhow::bail!("Fusillade daemon panic: {}", e);
                        }
                    }
                    Ok(())
                });
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
        if config.background_services.batch_daemon.enabled == DaemonEnabled::Always {
            use fusillade::DaemonExecutor;
            let daemon_handle = request_manager.clone().run(shutdown_token.clone())?;
            // Spawn task that propagates daemon errors
            background_tasks.spawn("fusillade-daemon", async move {
                match daemon_handle.await {
                    Ok(Ok(())) => {
                        tracing::info!("Fusillade daemon exited normally");
                    }
                    Ok(Err(e)) => {
                        tracing::error!(error = %e, "Fusillade daemon failed");
                        anyhow::bail!("Fusillade daemon error: {}", e);
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Fusillade daemon task panicked");
                        anyhow::bail!("Fusillade daemon panic: {}", e);
                    }
                }
                Ok(())
            });
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
        background_tasks.spawn("leader-election", async move {
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

                        // Start probe scheduler if enabled
                        if config.background_services.probe_scheduler.enabled {
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
                        } else {
                            tracing::info!("Probe scheduler disabled by configuration");
                        }

                        // Start the fusillade batch processing daemon based on config
                        use crate::config::DaemonEnabled;
                        use fusillade::DaemonExecutor;
                        match config.background_services.batch_daemon.enabled {
                            DaemonEnabled::Leader => {
                                let handle = request_manager
                                    .run(session_token.clone())
                                    .map_err(|e| anyhow::anyhow!("Failed to start fusillade daemon: {}", e))?;

                                // Store the handle so we can abort it when losing leadership
                                *daemon_handle.lock().await = Some(handle);

                                tracing::info!("Fusillade batch daemon started on elected leader");
                            }
                            DaemonEnabled::Always => {
                                // Daemon already started earlier, nothing to do here
                            }
                            DaemonEnabled::Never => {
                                tracing::info!("Fusillade batch daemon disabled by configuration");
                            }
                        }

                        Ok(())
                    }
                },
                move |_pool, config| {
                    // This closure is run when a replica stops being the leader
                    let scheduler = leader_election_scheduler_lose.clone();
                    let daemon_handle = daemon_handle_lose.clone();
                    let leadership_shutdown = leadership_shutdown_lose.clone();
                    async move {
                        // Cancel the leadership session token first, which will stop all background tasks gracefully
                        if let Some(token) = leadership_shutdown.lock().await.take() {
                            token.cancel();
                        }

                        // Now stop all schedulers if probe scheduler was enabled
                        if config.background_services.probe_scheduler.enabled {
                            scheduler
                                .stop_all()
                                .await
                                .map_err(|e| anyhow::anyhow!("Failed to stop probe scheduler: {}", e))?;
                        }

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
            // Leader election runs until cancelled, then exits normally
            Ok(())
        });
    }

    let (background_tasks, task_names) = background_tasks.into_parts();

    Ok(BackgroundServices {
        request_manager,
        is_leader,
        onwards_targets: initial_targets,
        onwards_sender,
        background_tasks,
        task_names,
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
    ///
    /// If `pool` is provided, it will be used directly instead of creating a new connection.
    /// This is useful for tests where sqlx::test provides a pool.
    pub async fn new(config: Config) -> anyhow::Result<Self> {
        Self::new_with_pool(config, None).await
    }

    /// Create a new application instance with an existing database pool
    ///
    /// This method is primarily for tests where sqlx::test provides a pool.
    /// For production use, prefer [`Application::new`] which will create its own pool.
    pub async fn new_with_pool(config: Config, pool: Option<PgPool>) -> anyhow::Result<Self> {
        debug!("Starting control layer with configuration: {:#?}", config);

        // Setup database connections, run migrations, and initialize data
        let (_embedded_db, pool, fusillade_pool, outlet_pool) = setup_database(&config, pool).await?;

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
        let middleware = middleware::from_fn_with_state(self.app_state, admin_ai_proxy_middleware);
        let service = middleware.layer(self.router).into_make_service();
        let server = axum_test::TestServer::new(service).expect("Failed to create test server");
        (server, self.bg_services)
    }

    /// Start serving the application
    pub async fn serve<F>(mut self, shutdown: F) -> anyhow::Result<()>
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
        let middleware = middleware::from_fn_with_state(self.app_state, admin_ai_proxy_middleware);
        let service = middleware.layer(self.router);

        // Race the server against background task failures (fail-fast)
        let server_error: Option<anyhow::Error> = tokio::select! {
            result = axum::serve(listener, service.into_make_service()).with_graceful_shutdown(shutdown) => {
                result.err().map(Into::into) // None if server shut down cleanly
            }
            result = self.bg_services.wait_for_failure() => {
                // Background task failed - save error for fail-fast restart after cleanup
                match result {
                    Ok(_infallible) => unreachable!("wait_for_failure never returns Ok"),
                    Err(e) => Some(e),
                }
            }
        };

        // Graceful shutdown - even if we're failing fast, clean up properly
        info!("Shutting down background services...");
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

        // If there was an error (either server or background task), propagate it after cleanup
        if let Some(e) = server_error {
            return Err(e);
        }

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::{AppState, create_initial_admin_user};
    use crate::{
        api::models::{groups::GroupResponse, users::Role},
        auth::password,
        db::handlers::Users,
        request_logging::{AiRequest, AiResponse},
        test_utils::*,
    };
    use outlet_postgres::RequestFilter;
    use sqlx::PgPool;
    use tracing::info;

    /// End-to-end integration test: Full AI proxy flow through API
    /// Follows a real user journey: admin creates endpoint/model, user gets API key, user makes inference request
    #[sqlx::test]
    #[test_log::test]
    async fn test_e2e_ai_proxy_with_mocked_inference(pool: PgPool) {
        // Setup wiremock server to mock inference endpoint
        let mock_server = wiremock::MockServer::start().await;

        // Mock OpenAI-style chat completion response
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-123",
                "object": "chat.completion",
                "created": 1677652288,
                "model": "gpt-3.5-turbo",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "Hello! How can I help you today?"
                    },
                    "finish_reason": "stop"
                }],
                "usage": {
                    "prompt_tokens": 9,
                    "completion_tokens": 12,
                    "total_tokens": 21
                }
            })))
            .mount(&mock_server)
            .await;

        // Create test app with onwards_sync and request logging enabled
        let mut config = crate::test_utils::create_test_config();
        config.background_services.onwards_sync.enabled = true;
        config.enable_request_logging = true;

        let app = crate::Application::new_with_pool(config, Some(pool.clone()))
            .await
            .expect("Failed to create application");
        let (server, bg_services) = app.into_test_server();

        // Step 1: Create admin and regular users
        let admin_user = create_test_admin_user(&pool, Role::PlatformManager).await;
        let admin_headers = add_auth_headers(&admin_user);

        let regular_user = create_test_user(&pool, Role::StandardUser).await;
        let regular_headers = add_auth_headers(&regular_user);

        // Step 2: Admin creates a group via API
        let group_response = server
            .post("/admin/api/v1/groups")
            .add_header(&admin_headers[0].0, &admin_headers[0].1)
            .add_header(&admin_headers[1].0, &admin_headers[1].1)
            .json(&serde_json::json!({
                "name": "test-group",
                "description": "Test group for E2E test"
            }))
            .await;
        assert_eq!(group_response.status_code(), 201, "Failed to create group");
        let group: GroupResponse = group_response.json();

        // Step 3: Admin adds user to group via API
        let add_user_response = server
            .post(&format!("/admin/api/v1/groups/{}/users/{}", group.id, regular_user.id))
            .add_header(&admin_headers[0].0, &admin_headers[0].1)
            .add_header(&admin_headers[1].0, &admin_headers[1].1)
            .await;
        assert_eq!(add_user_response.status_code(), 204, "Failed to add user to group");

        // Step 4: Admin grants credits to user via API
        let credits_response = server
            .post("/admin/api/v1/transactions")
            .add_header(&admin_headers[0].0, &admin_headers[0].1)
            .add_header(&admin_headers[1].0, &admin_headers[1].1)
            .json(&serde_json::json!({
                "user_id": regular_user.id,
                "transaction_type": "admin_grant",
                "amount": 1000,
                "source_id": admin_user.id,
                "description": "Test credits for E2E test"
            }))
            .await;
        assert_eq!(credits_response.status_code(), 201, "Failed to grant credits");

        // Step 5: Admin creates inference endpoint via API
        let mock_endpoint_url = format!("{}/v1", mock_server.uri());
        let endpoint_response = server
            .post("/admin/api/v1/endpoints")
            .add_header(&admin_headers[0].0, &admin_headers[0].1)
            .add_header(&admin_headers[1].0, &admin_headers[1].1)
            .json(&serde_json::json!({
                "name": "Mock Inference Endpoint",
                "url": mock_endpoint_url,
                "description": "Mock OpenAI-compatible endpoint for testing"
            }))
            .await;
        assert_eq!(endpoint_response.status_code(), 201, "Failed to create endpoint");
        let endpoint: crate::api::models::inference_endpoints::InferenceEndpointResponse = endpoint_response.json();

        // Step 6: Admin creates deployment via API (with pricing for credit deduction)
        let deployment_response = server
            .post("/admin/api/v1/models")
            .add_header(&admin_headers[0].0, &admin_headers[0].1)
            .add_header(&admin_headers[1].0, &admin_headers[1].1)
            .json(&serde_json::json!({
                "model_name": "gpt-3.5-turbo",
                "alias": "test-model",
                "description": "Test model deployment",
                "hosted_on": endpoint.id,
                "pricing": {
                    "input_price_per_token": "0.001",
                    "output_price_per_token": "0.003"
                }
            }))
            .await;
        assert_eq!(deployment_response.status_code(), 200, "Failed to create deployment");
        let deployment: crate::api::models::deployments::DeployedModelResponse = deployment_response.json();

        // Step 7: Admin adds deployment to group via API
        let add_deployment_response = server
            .post(&format!("/admin/api/v1/groups/{}/models/{}", group.id, deployment.id))
            .add_header(&admin_headers[0].0, &admin_headers[0].1)
            .add_header(&admin_headers[1].0, &admin_headers[1].1)
            .await;
        assert_eq!(add_deployment_response.status_code(), 204, "Failed to add deployment to group");

        // Step 8: User creates API key via API
        let api_key_response = server
            .post(&format!("/admin/api/v1/users/{}/api-keys", regular_user.id))
            .add_header(&regular_headers[0].0, &regular_headers[0].1)
            .add_header(&regular_headers[1].0, &regular_headers[1].1)
            .json(&serde_json::json!({
                "name": "Test Inference Key",
                "description": "API key for E2E test",
                "purpose": "inference"
            }))
            .await;
        assert_eq!(api_key_response.status_code(), 201, "Failed to create API key");
        let api_key: crate::api::models::api_keys::ApiKeyResponse = api_key_response.json();

        // Step 9: Sync once, then poll until model becomes available in onwards config
        bg_services.sync_onwards_config(&pool).await.expect("Failed to sync onwards config");

        // Poll: Initial state should be 404, target state is 200
        let poll_start = std::time::Instant::now();
        let mut status = 404;
        let mut attempts = 0;
        for i in 0..50 {
            // 50 attempts * 10ms = 500ms max
            attempts = i + 1;
            let test_response = server
                .post("/ai/v1/chat/completions")
                .add_header("authorization", format!("Bearer {}", api_key.key))
                .json(&serde_json::json!({
                    "model": "test-model",
                    "messages": [{"role": "user", "content": "test"}]
                }))
                .await;

            status = test_response.status_code().as_u16();
            if status != 404 {
                // Model is now in onwards config
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
        let poll_duration = poll_start.elapsed();
        println!(
            "Polled for {:?} over {} attempts, final status: {}",
            poll_duration, attempts, status
        );
        assert_ne!(status, 404, "Model should be available in onwards config after polling");

        // Test 1: User makes successful inference request via API key
        let inference_response = server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", format!("Bearer {}", api_key.key))
            .json(&serde_json::json!({
                "model": "test-model",
                "messages": [{"role": "user", "content": "Hello from E2E test"}]
            }))
            .await;

        assert_eq!(inference_response.status_code().as_u16(), 200, "Inference request should succeed");
        let inference_body: serde_json::Value = inference_response.json();
        assert_eq!(
            inference_body["choices"][0]["message"]["content"], "Hello! How can I help you today?",
            "Should receive mocked response from inference endpoint"
        );

        let mut tries = 0;
        // Verify credit deduction: Check that credits were deducted based on token usage
        // Credits can lag usage, so poll
        let usage_tx = loop {
            let transactions_response = server
                .get(&format!("/admin/api/v1/transactions?user_id={}", regular_user.id))
                .add_header(&admin_headers[0].0, &admin_headers[0].1)
                .add_header(&admin_headers[1].0, &admin_headers[1].1)
                .await;

            assert_eq!(transactions_response.status_code(), 200, "Should fetch transactions");
            let transactions: serde_json::Value = transactions_response.json();

            info!("Received {:?}", serde_json::to_string(&transactions));
            // Find the usage transaction (there should be an admin_grant and a usage transaction)
            let usage_tx = transactions
                .as_array()
                .and_then(|x| x.iter().find(|tx| tx["transaction_type"] == "usage"));

            if let Some(tx) = usage_tx {
                break tx.clone();
            } else {
                tries += 1;
                if tries >= 50 {
                    panic!("Usage transaction not found after {} attempts", tries);
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            }
        };

        assert_eq!(usage_tx["transaction_type"], "usage", "Should be usage transaction");
        // Amount is returned as string due to high precision decimal
        let amount: f64 = usage_tx["amount"].as_str().unwrap().parse().unwrap();
        let balance: f64 = usage_tx["balance_after"].as_str().unwrap().parse().unwrap();
        assert!(amount > 0.0, "Usage amount should be positive (absolute value), got: {}", amount);
        assert!(
            balance < 1000.0,
            "Balance should be less than initial 1000 due to credit deduction, got: {}",
            balance
        );

        // Verify request was logged: Check outlet recorded the request via API
        let requests_response = server
            .get(&format!("/admin/api/v1/requests?user_id={}&limit=1", regular_user.id))
            .add_header(&admin_headers[0].0, &admin_headers[0].1)
            .add_header(&admin_headers[1].0, &admin_headers[1].1)
            .await;

        assert_eq!(requests_response.status_code(), 200, "Should fetch logged requests");
        let requests: serde_json::Value = requests_response.json();
        let logged_entry = &requests["requests"][0];

        // Request details
        assert_eq!(logged_entry["request"]["method"], "POST");
        assert_eq!(logged_entry["request"]["uri"], "http://localhost/chat/completions");

        // Response details
        assert_eq!(logged_entry["response"]["status_code"], 200);
        let usage = &logged_entry["response"]["body"]["data"]["usage"];
        assert_eq!(usage["prompt_tokens"], 9, "Should have 9 prompt tokens from mock");
        assert_eq!(usage["completion_tokens"], 12, "Should have 12 completion tokens from mock");
        assert_eq!(usage["total_tokens"], 21, "Should match mocked token count");

        // Verify pricing headers were set by onwards
        let headers = &logged_entry["response"]["headers"];
        assert_eq!(headers["onwards-input-price-per-token"], "0.00100000");
        assert_eq!(headers["onwards-output-price-per-token"], "0.00300000");

        // Test 2: Proxy header auth also works (SSO-style authentication)
        let regular_user_external_id = regular_user.external_user_id.as_ref().unwrap_or(&regular_user.username);

        // First request creates a hidden API key, but it's not synced yet - should be 403
        let first_proxy_response = server
            .post("/admin/api/v1/ai/v1/chat/completions")
            .add_header("x-doubleword-user", regular_user_external_id)
            .add_header("x-doubleword-email", &regular_user.email)
            .json(&serde_json::json!({
                "model": "test-model",
                "messages": [{"role": "user", "content": "Hello via proxy headers"}]
            }))
            .await;
        let first_status = first_proxy_response.status_code().as_u16();
        assert!(
            first_status == 200 || first_status == 403,
            "First proxy request might succeed (200) or fail (403) depending on sync timing, got {}",
            first_status
        );

        // Sync to pick up the hidden API key
        bg_services.sync_onwards_config(&pool).await.expect("Failed to sync onwards config");

        // Poll until hidden key becomes available
        for _ in 0..50 {
            let test_response = server
                .post("/admin/api/v1/ai/v1/chat/completions")
                .add_header("x-doubleword-user", regular_user_external_id)
                .add_header("x-doubleword-email", &regular_user.email)
                .json(&serde_json::json!({
                    "model": "test-model",
                    "messages": [{"role": "user", "content": "test"}]
                }))
                .await;

            status = test_response.status_code().as_u16();
            if status == 200 {
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }

        // Now proxy auth should work
        let proxy_response = server
            .post("/admin/api/v1/ai/v1/chat/completions")
            .add_header("x-doubleword-user", regular_user_external_id)
            .add_header("x-doubleword-email", &regular_user.email)
            .json(&serde_json::json!({
                "model": "test-model",
                "messages": [{"role": "user", "content": "Hello via proxy headers"}]
            }))
            .await;

        assert_eq!(
            proxy_response.status_code().as_u16(),
            200,
            "Proxy header auth should work after sync"
        );
        let proxy_body: serde_json::Value = proxy_response.json();
        assert_eq!(proxy_body["choices"][0]["message"]["content"], "Hello! How can I help you today?");

        // Test 3: User without model access should be rejected (not in group)
        let restricted_user = create_test_user(&pool, Role::StandardUser).await;
        let restricted_headers = add_auth_headers(&restricted_user);

        // Create API key for restricted user
        let restricted_key_response = server
            .post(&format!("/admin/api/v1/users/{}/api-keys", restricted_user.id))
            .add_header(&restricted_headers[0].0, &restricted_headers[0].1)
            .add_header(&restricted_headers[1].0, &restricted_headers[1].1)
            .json(&serde_json::json!({
                "name": "Restricted User Key",
                "purpose": "inference"
            }))
            .await;
        let restricted_key: crate::api::models::api_keys::ApiKeyResponse = restricted_key_response.json();

        let no_access_response = server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", format!("Bearer {}", restricted_key.key))
            .json(&serde_json::json!({
                "model": "test-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        // Should reject - 403 (key not synced) or 404 (user has no access to model)
        let status = no_access_response.status_code().as_u16();
        assert!(
            status == 403 || status == 404,
            "Should reject user without model access, got {}",
            status
        );

        // Test 4: Missing authentication should fail
        let missing_auth_response = server
            .post("/ai/v1/chat/completions")
            .json(&serde_json::json!({
                "model": "test-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(missing_auth_response.status_code().as_u16(), 401, "Should require authentication");

        // Test 5: Invalid API key should fail
        let invalid_key_response = server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", "Bearer invalid-key-12345")
            .json(&serde_json::json!({
                "model": "test-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(invalid_key_response.status_code().as_u16(), 403, "Should reject invalid API key");

        // Test 6: Non-existent model should return 404
        let nonexistent_model_response = server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", format!("Bearer {}", api_key.key))
            .json(&serde_json::json!({
                "model": "nonexistent-model",
                "messages": [{"role": "user", "content": "Hello"}]
            }))
            .await;

        assert_eq!(
            nonexistent_model_response.status_code().as_u16(),
            404,
            "Should return 404 for non-existent model"
        );

        // Gracefully shutdown background services to avoid slow test cleanup
        bg_services.shutdown().await;
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
                default_models: None,
            },
            ModelSource {
                name: "test-endpoint-2".to_string(),
                url: Url::parse("http://localhost:8002").unwrap(),
                api_key: None,
                sync_interval: std::time::Duration::from_secs(10),
                default_models: None,
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
        config.background_services.leader_election.enabled = false;

        // Create application using proper setup (which will create outlet_db)
        let app = crate::Application::new_with_pool(config, Some(pool))
            .await
            .expect("Failed to create application");

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
        let user_id = create_initial_admin_user(
            test_email,
            None,
            password::Argon2Params {
                memory_kib: 128,
                iterations: 1,
                parallelism: 1,
            },
            &pool,
        )
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
        let returned_user_id = create_initial_admin_user(
            test_email,
            None,
            password::Argon2Params {
                memory_kib: 128,
                iterations: 1,
                parallelism: 1,
            },
            &pool,
        )
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
