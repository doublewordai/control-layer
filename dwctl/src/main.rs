mod api;
mod auth;
mod config;
mod crypto;
mod db;
mod email;
mod errors;
mod metrics;
mod openapi;
mod probes;
mod request_logging;
mod static_assets;
mod sync;
mod types;

#[cfg(test)]
mod test_utils;

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
use axum::http::HeaderValue;
use axum::{
    body::Body,
    http::{Request, Response, StatusCode, Uri},
    middleware::from_fn_with_state,
    response::{Html, IntoResponse},
    routing::{delete, get, patch, post},
    Router, ServiceExt,
};
use axum_prometheus::PrometheusMetricLayer;
use bon::Builder;
use clap::Parser;
use config::{Args, Config};
use outlet::{RequestLoggerConfig, RequestLoggerLayer};
use outlet_postgres::PostgresHandler;
use request_logging::{AiRequest, AiResponse};
use sqlx::{ConnectOptions, Executor, PgPool};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tower::Layer;
use tower::ServiceBuilder;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::{debug, info, instrument, Span};
use utoipa::OpenApi;
use utoipa_rapidoc::RapiDoc;
use uuid::Uuid;

pub use types::{ApiKeyId, DeploymentId, GroupId, InferenceEndpointId, UserId};

#[derive(Clone, Builder)]
pub struct AppState {
    pub db: PgPool,
    pub config: Config,
    pub outlet_db: Option<PgPool>,
    pub metrics_recorder: Option<GenAiMetrics>,
    #[builder(default = false)]
    pub is_leader: bool,
}

/// Create the initial admin user if it doesn't exist
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

/// Background task for leader election
/// Runs periodically to maintain leadership or attempt to acquire it
///
/// We use leadership election for figuring out who runs background tasks like sending probes to
/// the endpoints. At some point, we may want to expand this to other tasks as well.
///
/// PostgreSQL advisory locks are session-based, so we need to maintain a dedicated connection
/// for the entire duration we want to hold the lock.
#[instrument(skip(pool, config, lock_id, on_gain_leadership, on_lose_leadership))]
async fn leader_election_task<F1, F2, Fut1, Fut2>(
    pool: PgPool,
    config: config::Config,
    is_leader: Arc<AtomicBool>,
    lock_id: i64,
    on_gain_leadership: F1,
    on_lose_leadership: F2,
) where
    F1: Fn(PgPool, config::Config) -> Fut1 + Send + 'static,
    F2: Fn(PgPool, config::Config) -> Fut2 + Send + 'static,
    Fut1: std::future::Future<Output = Result<(), anyhow::Error>> + Send + 'static,
    Fut2: std::future::Future<Output = Result<(), anyhow::Error>> + Send + 'static,
{
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
    let mut leader_conn: Option<sqlx::pool::PoolConnection<sqlx::Postgres>> = None;

    loop {
        interval.tick().await;

        let current_status = is_leader.load(Ordering::Relaxed);

        // If we're not leader, try to acquire the lock
        if !current_status {
            // Try to acquire a connection and the lock
            match pool.acquire().await {
                Ok(mut conn) => {
                    match sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock($1)")
                        .bind(lock_id)
                        .fetch_one(&mut *conn)
                        .await
                    {
                        Ok(true) => {
                            // Successfully acquired lock!
                            info!("Gained leadership");
                            is_leader.store(true, Ordering::Relaxed);
                            leader_conn = Some(conn); // Keep connection alive

                            if let Err(e) = on_gain_leadership(pool.clone(), config.clone()).await {
                                tracing::error!("Failed to execute on_gain_leadership callback: {}", e);
                            }
                        }
                        Ok(false) => {
                            // Someone else has the lock
                            debug!("Following - will retry");
                        }
                        Err(e) => {
                            tracing::error!("Failed to check leader lock: {}", e);
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to acquire connection for leader election: {}", e);
                }
            }
        } else {
            // We think we're leader - verify we still hold the lock
            // by checking if our connection is still valid
            if let Some(ref mut conn) = leader_conn {
                // Ping the connection to keep it alive
                match sqlx::query("SELECT 1").execute(&mut **conn).await {
                    Ok(_) => {
                        debug!("✓ Leadership renewed (connection alive)");
                    }
                    Err(e) => {
                        // Connection died, which will drop the advisory lock, we lost leadership
                        tracing::warn!("Lost leadership (connection died): {}", e);
                        info!("Lost leadership");
                        is_leader.store(false, Ordering::Relaxed);
                        leader_conn = None;

                        if let Err(e) = on_lose_leadership(pool.clone(), config.clone()).await {
                            tracing::error!("Failed to execute on_lose_leadership callback: {}", e);
                        }
                    }
                }
            } else {
                // We think we're leader but have no connection, this can't happen
                tracing::error!("Inconsistent state: is_leader=true but no connection");
                is_leader.store(false, Ordering::Relaxed);
            }
        }
    }
}

/// Seed the database with initial configuration (run only once)
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

/// Create CORS layer from configuration
fn create_cors_layer(config: &Config) -> anyhow::Result<CorsLayer> {
    use crate::config::CorsOrigin;

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
        .allow_credentials(config.auth.security.cors.allow_credentials);

    if let Some(max_age) = config.auth.security.cors.max_age {
        cors = cors.max_age(std::time::Duration::from_secs(max_age));
    }

    Ok(cors)
}

/// Serve embedded static assets with SPA fallback
#[instrument]
async fn serve_embedded_asset(uri: Uri) -> impl IntoResponse {
    let mut path = uri.path().trim_start_matches('/');

    // If path is empty or ends with /, serve index.html
    if path.is_empty() || path.ends_with('/') {
        path = "index.html";
    }

    // Try to serve the requested file
    if let Some(content) = static_assets::Assets::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return Response::builder()
            .header(axum::http::header::CONTENT_TYPE, mime.as_ref())
            .body(Body::from(content.data.into_owned()))
            .unwrap();
    }

    // If not found, serve index.html for SPA client-side routing
    if let Some(index) = static_assets::Assets::get("index.html") {
        return Response::builder()
            .header(axum::http::header::CONTENT_TYPE, "text/html")
            .body(Body::from(index.data.into_owned()))
            .unwrap();
    }

    // If even index.html is missing, return 404
    Response::builder().status(StatusCode::NOT_FOUND).body(Body::empty()).unwrap()
}

/// SPA fallback handler - serves index.html for client-side routes
#[instrument(err)]
async fn spa_fallback(uri: Uri) -> Result<Html<String>, StatusCode> {
    debug!("Hitting SPA fallback for: {}", uri.path());

    // Serve embedded index.html
    if let Some(index) = static_assets::Assets::get("index.html") {
        let content = String::from_utf8_lossy(&index.data).to_string();
        Ok(Html(content))
    } else {
        Err(StatusCode::INTERNAL_SERVER_ERROR)
    }
}

/// Setup the complete application with onwards integration
/// Returns router, onwards config sync handle, and optional drop guard for shutdown
#[instrument(skip(pool, config))]
pub async fn setup_app(
    pool: PgPool,
    config: Config,
    skip_leader_election: bool,
) -> anyhow::Result<(Router, sync::onwards_config::OnwardsConfigSync, tokio_util::sync::DropGuard)> {
    debug!("Setting up application");
    // Seed database with initial configuration (only runs once)
    seed_database(&config.model_sources, &pool).await?;

    // Start onwards integration
    let (onwards_config_sync, initial_targets, onwards_stream, drop_guard) =
        sync::onwards_config::OnwardsConfigSync::new(pool.clone()).await?;

    // Build the onwards router
    let onwards_app_state = onwards::AppState::new(initial_targets.clone());
    let onwards_router = onwards::build_router(onwards_app_state);

    // Start target updates (infallible task, handle internally)
    tokio::spawn(async move {
        let _ = initial_targets.receive_updates(onwards_stream).await;
    });

    // Leader election lock ID: 0x44574354_50524F42 (DWCT_PROB in hex for "dwctl probes")
    const LEADER_LOCK_ID: i64 = 0x4457_4354_5052_4F42_i64;

    let probe_scheduler = probes::ProbeScheduler::new(pool.clone(), config.clone());
    let is_leader: bool;

    if skip_leader_election {
        // Skip leader election - just become leader immediately
        is_leader = true;
        probe_scheduler.initialize().await?;

        // Start the scheduler daemon in the background
        let daemon_scheduler = probe_scheduler.clone();
        tokio::spawn(async move {
            // Use LISTEN/NOTIFY in production, but disable in tests to avoid hangs
            let use_listen_notify = !cfg!(test);
            daemon_scheduler.run_daemon(use_listen_notify, 300).await; // Fallback sync every 5 minutes
        });

        info!("Skipping leader election - running as leader with probe scheduler");
    } else {
        // Normal leader election
        is_leader = false;
        info!("Starting leader election - will attempt to acquire leadership");

        let is_leader_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Spawn leader election background task
        // This is designed to solve a problem that could have been solved by spinning up a
        // separate service, but we're trying to keep everything in one replicated binary. There
        // are some tasks that should only be run by one replica of the control layer service - for
        // example, running the probes scheduler. To figure out which replica should run these
        // tasks, we use 'leader election'. This is an elaborate name for 'taking a postgres
        // advisory lock'.
        //
        // All the replicas try to take the lock on an interval. The one that succeeds becomes the
        // leader. If the leader dies, another replica will succeed at the next interval. The
        // leader election task takes two callbacks: one that runs when we become leader, and one
        // that runs when we stop being leader.
        let leader_election_pool = pool.clone();
        let leader_election_scheduler_gain = probe_scheduler.clone();
        let leader_election_scheduler_lose = probe_scheduler.clone();
        let leader_election_config = config.clone();
        let leader_election_flag = is_leader_flag.clone();
        tokio::spawn(async move {
            leader_election_task(
                leader_election_pool,
                leader_election_config,
                leader_election_flag,
                LEADER_LOCK_ID,
                move |_pool, _config| {
                    // This closure is run when a replica becomes the leader
                    let scheduler = leader_election_scheduler_gain.clone();
                    async move {
                        // Wait for the server to be fully up before starting probes
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                        scheduler
                            .initialize()
                            .await
                            .map_err(|e| anyhow::anyhow!("Failed to initialize probe scheduler: {}", e))?;

                        // Start the probe scheduler daemon in the background
                        let daemon_scheduler = scheduler.clone();
                        tokio::spawn(async move {
                            // Use LISTEN/NOTIFY in production, but disable in tests, because
                            // LISTEN/NOTIFY can be annoying in test environments.
                            let use_listen_notify = !cfg!(test);
                            daemon_scheduler.run_daemon(use_listen_notify, 300).await;
                        });

                        Ok(())
                    }
                },
                move |_pool, _config| {
                    // This closure is run when a replica stops being the leader
                    let scheduler = leader_election_scheduler_lose.clone();
                    async move {
                        scheduler
                            .stop_all()
                            .await
                            .map_err(|e| anyhow::anyhow!("Failed to stop probe scheduler: {}", e))
                    }
                },
            )
            .await;
        });
    }

    let mut app_state = AppState::builder().db(pool).config(config).is_leader(is_leader).build();
    let router = build_router(&mut app_state, onwards_router).await?;

    Ok((router, onwards_config_sync, drop_guard))
}

#[instrument(skip(state, onwards_router))]
pub async fn build_router(state: &mut AppState, onwards_router: Router) -> anyhow::Result<Router> {
    // Setup request logging if enabled
    let outlet_layer = if state.config.enable_request_logging {
        // Setup request logging with PostgreSQL handler using schema separation

        // Get the database URL from the existing pool
        let database_url = state.db.connect_options().to_url_lossy().to_string();

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
            .await
            .expect("Failed to create outlet database pool");

        outlet_pool
            .execute("CREATE SCHEMA IF NOT EXISTS outlet")
            .await
            .expect("Failed to create outlet schema");

        outlet_postgres::migrator()
            .run(&outlet_pool)
            .await
            .expect("Failed to run outlet migrations");

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
            .with_path_prefix("/ai/")
            .with_request_serializer(parse_ai_request)
            .with_response_serializer(analytics_serializer.create_serializer());

        state.outlet_db = Some(outlet_pool.clone());

        let outlet_config = RequestLoggerConfig {
            capture_request_body: true,
            capture_response_body: true,
        };

        Some(RequestLoggerLayer::new(outlet_config, postgres_handler))
    } else {
        None
    };
    // Authentication routes (at root level, masked by proxy when deployed behind vouch)
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
        .route("/probes/{id}/statistics", get(api::handlers::probes::get_statistics))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|request: &Request<_>| {
                    tracing::info_span!(
                        "request",
                        method = %request.method(),
                        uri = %request.uri(),
                    )
                })
                .on_response(|response: &Response<_>, latency: Duration, _span: &Span| {
                    tracing::info!(
                        status = %response.status(),
                        latency = ?latency,
                        "request completed"
                    );
                }),
        )
        .with_state(state.clone());

    // Serve embedded static assets, falling back to SPA for unmatched routes
    let fallback = get(serve_embedded_asset).fallback(get(spa_fallback));

    // Build the app with admin API and onwards proxy nested. serve the (restricted) openai spec.
    let router = Router::new()
        .route("/healthz", get(|| async { "OK" }))
        .route(
            "/openai-openapi.yaml",
            get(|| async {
                const OPENAPI_SPEC: &str = include_str!("openai-openapi.yaml");
                (axum::http::StatusCode::OK, [("content-type", "application/yaml")], OPENAPI_SPEC)
            }),
        )
        .merge(auth_routes)
        .nest("/ai/v1", onwards_router)
        .nest("/admin/api/v1", api_routes)
        .merge(RapiDoc::with_openapi("/api-docs/openapi.json", ApiDoc::openapi()).path("/admin/docs"))
        .merge(RapiDoc::new("/openai-openapi.yaml").path("/ai/docs"))
        .fallback_service(fallback);

    // Create CORS layer from config
    let cors_layer = create_cors_layer(&state.config)?;

    // Apply layers conditionally
    let mut router = if let Some(outlet_layer) = outlet_layer {
        router.layer(ServiceBuilder::new().layer(outlet_layer).layer(cors_layer))
    } else {
        router.layer(cors_layer)
    };

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

    Ok(router)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing with environment filter
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,onwards_pilot::sync=warn")),
        )
        .init();

    // Parse CLI args
    let args = Args::parse();
    debug!("{:?}", args);

    // Load configuration
    let config = Config::load(&args)?;
    debug!("Starting control layer with configuration: {:#?}", config);

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

    // Run migrations
    sqlx::migrate!("./migrations").run(&pool).await?;

    // create admin user if it doesn't exist
    create_initial_admin_user(&config.admin_email, config.admin_password.as_deref(), &pool)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create initial admin user: {}", e))?;

    // Setup the complete application
    let (router, onwards_config_sync, _drop_guard) = setup_app(pool.clone(), config.clone(), false).await?;

    // Apply middleware at root level BEFORE routing decisions are made
    let middleware = from_fn_with_state(
        AppState::builder().db(pool.clone()).config(config.clone()).build(),
        admin_ai_proxy_middleware,
    );

    // Apply the layer around the whole Router so middleware runs before Router receives the request
    let app_with_middleware = middleware.layer(router);

    // Start the onwards integration task
    tokio::spawn(async move {
        info!("Starting onwards configuration listener");
        if let Err(e) = onwards_config_sync.start().await {
            tracing::error!("Onwards configuration listener error: {}", e);
        }
    });

    let bind_addr = config.bind_address();
    let listener = TcpListener::bind(&bind_addr).await?;
    info!(
        "Control layer listening on http://{}, available at http://localhost:{}",
        bind_addr, config.port
    );

    // Run the server with graceful shutdown
    axum::serve(listener, app_with_middleware.into_make_service())
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // Clean up embedded database if it exists
    if let Some(embedded_db) = _embedded_db {
        info!("Shutting down embedded database...");
        embedded_db.stop().await?;
    }

    Ok(())
}

/// Wait for shutdown signal (SIGTERM or Ctrl+C)
async fn shutdown_signal() {
    use tokio::signal;

    let ctrl_c = async {
        signal::ctrl_c().await.expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            info!("Received Ctrl+C, shutting down gracefully...");
        },
        _ = terminate => {
            info!("Received SIGTERM, shutting down gracefully...");
        },
    }
}

#[cfg(test)]
mod test {
    use super::{create_initial_admin_user, AppState};
    use crate::{
        api::models::users::Role,
        auth::middleware::admin_ai_proxy_middleware,
        db::handlers::Users,
        request_logging::{AiRequest, AiResponse},
        test_utils::*,
    };
    use axum::ServiceExt as _;
    use outlet_postgres::RequestFilter;
    use sqlx::PgPool;
    use tower::Layer as _;

    /// Integration test: setup the whole stack, including syncing the onwards config from
    /// LISTEN/NOTIFY, and then test user access via headers to /admin/api/v1/ai
    #[sqlx::test]
    #[test_log::test]
    async fn test_admin_ai_proxy_middleware_with_user_access(pool: PgPool) {
        // Create test app with sync enabled
        let (router, onwards_config_sync, _drop_guard) = crate::setup_app(pool.clone(), crate::test_utils::create_test_config(), true)
            .await
            .expect("Failed to setup test app");

        // Apply middleware for this test
        let middleware = admin_ai_proxy_middleware;
        let app_state = crate::AppState::builder()
            .db(pool.clone())
            .config(crate::test_utils::create_test_config())
            .build();
        // TODO: put the middleware application into some function that's shared w/ main.rs. The
        // reason it isn't now is that `Router` is a nice concrete type for setup_app to return,
        // but impl IntoMakeService is a bit tougher
        let middleware_layer = axum::middleware::from_fn_with_state(app_state, middleware);
        let router_with_middleware = middleware_layer.layer(router);

        let server = axum_test::TestServer::new(router_with_middleware.into_make_service()).expect("Failed to create test server");

        // Start the config sync in background for test
        tokio::spawn(async move {
            if let Err(e) = onwards_config_sync.start().await {
                eprintln!("Config sync error in test: {e}");
            }
        });

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

        // Should be forbidden since user has no group membership
        assert_eq!(
            no_access_response.status_code().as_u16(),
            403,
            "Admin proxy should reject user with no model access"
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

        // Should be forbidden since user doesn't exist
        assert_eq!(
            nonexistent_user_response.status_code().as_u16(),
            403,
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

        // Should be not found since model doesn't exist
        assert_eq!(
            nonexistent_model_response.status_code().as_u16(),
            403,
            "Admin proxy should reject non-existent model"
        );
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
            "INSERT INTO api_keys (id, name, secret, user_id) VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO UPDATE SET secret = $3",
            system_api_key_id,
            "System API Key",
            original_secret,
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

        // Build router with request logging enabled
        let mut app_state = AppState::builder().db(pool.clone()).config(config).build();
        let onwards_router = axum::Router::new().route("/v1/models", axum::routing::get(|| async { "AI Models" })); // Simple
                                                                                                                    // onwards router for testing
        let router = super::build_router(&mut app_state, onwards_router)
            .await
            .expect("Failed to build router");
        let outlet_pool = app_state.outlet_db.clone().expect("outlet_db should exist");
        let repository: outlet_postgres::RequestRepository<AiRequest, AiResponse> = outlet_postgres::RequestRepository::new(outlet_pool);

        let server = axum_test::TestServer::new(router).expect("Failed to create test server");

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
        let mut app_state = AppState::builder().db(pool.clone()).config(config).build();
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
    async fn test_setup_app_integration(pool: PgPool) {
        let config = create_test_config();

        // Call setup_app
        let result = super::setup_app(pool.clone(), config, true).await;
        assert!(result.is_ok(), "setup_app should succeed");

        let (router, _onwards_sync, _drop_guard) = result.unwrap();
        let server = axum_test::TestServer::new(router).expect("Failed to create test server");

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

        let mut app_state = AppState::builder().db(pool).config(config).build();

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

        let mut app_state = AppState::builder().db(pool).config(config).build();

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
