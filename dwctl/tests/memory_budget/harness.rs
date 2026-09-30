//! Runs the application in-process against a fresh database, with every
//! request-path layer enabled, and registers one model served by the mock
//! upstream.
//!
//! The application runs on its own runtime and its allocations are counted.
//! The mocks, the load generator and setup traffic run on a second runtime
//! whose threads are excluded from the count.

use std::str::FromStr;
use std::time::Duration;

use dwctl::Application;
use dwctl::config::Config;
use reqwest::{Client, Method};
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgConnection};
use sqlx::{ConnectOptions, Connection};
use tokio::runtime::Runtime;
use tokio::sync::oneshot;

use crate::alloc;
use crate::tokenizer::Tokenizer;
use crate::upstream::Upstream;

pub const MODEL_ALIAS: &str = "memory-budget-model";
const UPSTREAM_MODEL: &str = "upstream-model";
const ADMIN_EMAIL: &str = "admin@memory-budget.test";
const ADMIN_PASSWORD: &str = "memory-budget-password";
const EVERYONE_GROUP: &str = "00000000-0000-0000-0000-000000000000";

pub struct Options {
    /// Inference keys allowed to call the model. The routing layer keeps them
    /// all in the model's key set.
    pub api_keys: usize,
}

pub struct Harness {
    pub driver: Runtime,
    pub upstream: Upstream,
    pub client: Client,
    pub base_url: String,
    pub api_key: String,
    app: Option<Runtime>,
    shutdown: Option<oneshot::Sender<()>>,
    database: Database,
    _tokenizer: Tokenizer,
}

impl Harness {
    pub fn start(options: Options) -> Harness {
        let driver = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("memory-budget-driver")
            .on_thread_start(alloc::exclude_current_thread)
            .enable_all()
            .build()
            .expect("driver runtime");
        let app = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("memory-budget-app")
            .enable_all()
            .build()
            .expect("application runtime");

        let upstream = driver.block_on(driver.spawn(Upstream::start(UPSTREAM_MODEL))).unwrap();
        let tokenizer = driver.block_on(driver.spawn(Tokenizer::start(MODEL_ALIAS))).unwrap();
        let database = driver.block_on(driver.spawn(Database::create())).unwrap();

        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .expect("free port")
            .port();
        let config = config(port, &database.url, &tokenizer.base_url);

        let application = app
            .block_on(Application::new_with_pool(config, None, None))
            .expect("start application");
        let (shutdown, shutdown_signal) = oneshot::channel::<()>();
        app.spawn(application.serve(async move {
            let _ = shutdown_signal.await;
        }));

        // A new connection per request keeps idle keep-alive buffers out of the count.
        let client = Client::builder().pool_max_idle_per_host(0).build().expect("http client");
        let base_url = format!("http://127.0.0.1:{port}");
        let setup = setup(
            client.clone(),
            base_url.clone(),
            upstream.base_url.clone(),
            database.url.clone(),
            options,
        );
        let api_key = driver.block_on(driver.spawn(setup)).unwrap();

        Harness {
            driver,
            upstream,
            client,
            base_url,
            api_key,
            app: Some(app),
            shutdown: Some(shutdown),
            database,
            _tokenizer: tokenizer,
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(app) = self.app.take() {
            app.shutdown_timeout(Duration::from_secs(5));
        }
        let database = self.database.name.clone();
        let admin = self.database.admin.clone();
        let _ = self
            .driver
            .block_on(self.driver.spawn(async move { drop_database(admin, &database).await }));
    }
}

/// Every request-path layer enabled, with each external dependency replaced
/// by a local mock or an in-memory backend.
fn config(port: u16, database_url: &str, tokenizer_url: &str) -> Config {
    let config = json!({
        "host": "127.0.0.1",
        "port": port,
        "database": { "type": "external", "url": database_url },
        "secret_key": "memory-budget-secret-key-memory-budget",
        "admin_email": ADMIN_EMAIL,
        "admin_password": ADMIN_PASSWORD,
        "enable_metrics": true,
        "enable_request_logging": true,
        "enable_analytics": true,
        "background_services": { "batch_daemon": { "enabled": "never" } },
        "limits": { "requests": { "max_body_size": 5 * 1024 * 1024 } },
        "onwards": { "strict_mode": true, "sse_buffer_limit": 1024 * 1024 },
        "image_normalizer": { "enabled": true, "backend": { "type": "memory" } },
        "cache": { "enabled": true, "tokenizer_url": tokenizer_url, "render_counting": true },
        "continuation": { "enabled": true },
    });
    serde_json::from_value(config).expect("valid configuration")
}

/// Registers the mock upstream as a model open to every key, creates the
/// inference keys, and waits until the model is routable. Returns one key.
async fn setup(client: Client, base_url: String, upstream_url: String, database_url: String, options: Options) -> String {
    wait_for(&client, &format!("{base_url}/health")).await;

    let login = client
        .post(format!("{base_url}/authentication/login"))
        .json(&json!({ "email": ADMIN_EMAIL, "password": ADMIN_PASSWORD }))
        .send()
        .await
        .expect("login");
    assert!(login.status().is_success(), "login failed: {}", login.status());
    let session = login
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|value| value.split(';').next().filter(|pair| pair.starts_with("dwctl_session=")))
        .expect("session cookie")
        .to_string();

    let admin = |method: Method, path: &str, body: Option<Value>| {
        let mut request = client
            .request(method, format!("{base_url}/admin/api/v1{path}"))
            .header("cookie", &session);
        if let Some(body) = body {
            request = request.json(&body);
        }
        async move {
            let response = request.send().await.expect("admin request");
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            assert!(status.is_success(), "admin request failed with {status}: {text}");
            serde_json::from_str::<Value>(&text).unwrap_or(Value::Null)
        }
    };

    let user = admin(Method::GET, "/users/current", None).await;
    let user_id = user["id"].as_str().expect("current user id").to_string();
    let endpoint = admin(
        Method::POST,
        "/endpoints",
        Some(json!({ "name": "memory-budget-upstream", "url": upstream_url, "sync": false })),
    )
    .await;
    let model = admin(
        Method::POST,
        "/models",
        Some(json!({
            "type": "standard",
            "model_name": UPSTREAM_MODEL,
            "alias": MODEL_ALIAS,
            "hosted_on": endpoint["id"],
        })),
    )
    .await;
    let model_id = model["id"].as_str().expect("model id");
    admin(Method::POST, &format!("/groups/{EVERYONE_GROUP}/models/{model_id}"), None).await;

    let api_key = insert_api_keys(&database_url, &user_id, options.api_keys).await;

    // The model becomes routable once the routing layer reloads, which the
    // inserts above trigger.
    let url = format!("{base_url}/ai/v1/chat/completions");
    let body = json!({ "model": MODEL_ALIAS, "messages": [{ "role": "user", "content": "ready?" }] });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let status = client
            .post(&url)
            .bearer_auth(&api_key)
            .json(&body)
            .send()
            .await
            .map(|response| response.status());
        if matches!(status, Ok(status) if status.is_success()) {
            return api_key;
        }
        assert!(tokio::time::Instant::now() < deadline, "model never became routable: {status:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn insert_api_keys(database_url: &str, user_id: &str, count: usize) -> String {
    let mut connection = PgConnection::connect(database_url).await.expect("connect for key setup");
    let user_id = uuid::Uuid::parse_str(user_id).expect("user id is a uuid");
    let secrets: Vec<(String,)> = sqlx::query_as(
        "INSERT INTO api_keys (name, secret, purpose, user_id, created_by)
         SELECT 'memory-budget-' || n, 'sk-memory-budget-' || n || '-' || md5(random()::text), 'realtime', $1, $1
         FROM generate_series(1, $2) AS n
         RETURNING secret",
    )
    .bind(user_id)
    .bind(count as i32)
    .fetch_all(&mut connection)
    .await
    .expect("insert api keys");
    secrets.into_iter().next().expect("at least one key").0
}

async fn wait_for(client: &Client, url: &str) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !matches!(client.get(url).send().await, Ok(response) if response.status().is_success()) {
        assert!(tokio::time::Instant::now() < deadline, "{url} never became healthy");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A database created for one harness and dropped with it.
struct Database {
    name: String,
    url: String,
    admin: PgConnectOptions,
}

impl Database {
    async fn create() -> Database {
        let server = PgConnectOptions::from_str(&database_url()).expect("DATABASE_URL is a Postgres URL");
        let admin = server.clone().database("postgres");
        let name = format!("memory_budget_{}", uuid::Uuid::new_v4().simple());
        let mut connection = admin.connect().await.expect("connect to Postgres");
        sqlx::query(&format!("CREATE DATABASE \"{name}\""))
            .execute(&mut connection)
            .await
            .expect("create test database");

        let mut url = url::Url::parse(&database_url()).expect("DATABASE_URL is a URL");
        url.set_path(&format!("/{name}"));
        url.set_query(None);
        Database {
            name,
            url: url.to_string(),
            admin,
        }
    }
}

async fn drop_database(admin: PgConnectOptions, name: &str) {
    if let Ok(mut connection) = admin.connect().await {
        let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
            .execute(&mut connection)
            .await;
    }
}

/// `DATABASE_URL` from the environment, or from `dwctl/.env` as written by `just db-setup`.
fn database_url() -> String {
    if let Ok(url) = std::env::var("DATABASE_URL") {
        return url;
    }
    let env_file = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(".env");
    std::fs::read_to_string(&env_file)
        .ok()
        .and_then(|contents| {
            contents.lines().find_map(|line| {
                line.strip_prefix("DATABASE_URL=")
                    .map(|url| url.trim().trim_matches('"').to_string())
            })
        })
        .expect("DATABASE_URL is set or present in dwctl/.env (run `just db-setup`)")
}
