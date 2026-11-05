//! Internal credentials end to end (DW-1009): the playground, health probes
//! and direct `/ai` calls authenticate as a key by id, with a token only this
//! process accepts, and never with the key's secret.

use std::time::Duration;

use axum::ServiceExt as _;
use axum::middleware;
use sqlx::PgPool;
use tower::Layer as _;
use uuid::Uuid;

use crate::api::models::users::{Role, UserResponse};
use crate::auth::internal_credential::InternalCredentials;
use crate::db::handlers::{Repository as _, api_keys::ApiKeys};
use crate::db::models::api_keys::ApiKeyPurpose;
use crate::probes::db::ProbeManager;
use crate::test::utils::{add_auth_headers, create_test_admin_user, create_test_config, create_test_user_with_roles};

const MODEL: &str = "gpt-4";

struct Harness {
    server: axum_test::TestServer,
    bg: crate::BackgroundServices,
    credentials: InternalCredentials,
    config: crate::config::Config,
    mock: wiremock::MockServer,
    user: UserResponse,
    deployment_id: Uuid,
}

impl Harness {
    /// A live app on a real port (probes call `localhost:{port}/ai`), with one
    /// model on a mock upstream, granted to everyone, and a user with credit.
    async fn start(pool: &PgPool) -> Self {
        Self::start_with(pool, |_| {}).await
    }

    /// As [`Self::start`], with `configure` applied to the config first.
    async fn start_with(pool: &PgPool, configure: impl FnOnce(&mut crate::config::Config)) -> Self {
        let mock = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-1", "object": "chat.completion", "created": 1, "model": MODEL,
                "choices": [ { "index": 0, "message": { "role": "assistant", "content": "hi" }, "finish_reason": "stop" } ],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
            })))
            .mount(&mock)
            .await;

        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut config = create_test_config();
        config.host = "127.0.0.1".to_string();
        config.port = port;
        config.background_services.onwards_sync.enabled = true;
        // Write each completed response row as soon as it arrives.
        config.background_services.task_workers.response_writer_batch_size = 1;
        configure(&mut config);
        let app = crate::Application::new_with_pool(config.clone(), Some(pool.clone()), None)
            .await
            .expect("app");
        let crate::Application {
            router,
            app_state,
            bg_services,
            ..
        } = app;
        let credentials = app_state.internal_credentials.clone();
        // As `into_test_server`, but listening on the configured port.
        let service = middleware::from_fn(crate::assign_request_id)
            .layer(middleware::from_fn_with_state(app_state, crate::admin_ai_proxy_middleware).layer(router))
            .into_make_service();
        let server = axum_test::TestServer::builder()
            .http_transport_with_ip_port(Some([127, 0, 0, 1].into()), Some(port))
            .build(service)
            .expect("test server");

        let admin = create_test_admin_user(pool, Role::PlatformManager).await;
        let h = add_auth_headers(&admin);
        let endpoint: serde_json::Value = server
            .post("/admin/api/v1/endpoints")
            .add_header(&h[0].0, &h[0].1)
            .add_header(&h[1].0, &h[1].1)
            .json(&serde_json::json!({ "name": "mock", "url": mock.uri(), "auto_sync_models": false }))
            .await
            .json();
        let model: serde_json::Value = server
            .post("/admin/api/v1/models")
            .add_header(&h[0].0, &h[0].1)
            .add_header(&h[1].0, &h[1].1)
            .json(&serde_json::json!({ "type": "standard", "model_name": MODEL, "alias": MODEL, "hosted_on": endpoint["id"] }))
            .await
            .json();
        let deployment_id: Uuid = model["id"].as_str().unwrap().parse().unwrap();
        server
            .post(&format!("/admin/api/v1/groups/{}/models/{deployment_id}", Uuid::nil()))
            .add_header(&h[0].0, &h[0].1)
            .add_header(&h[1].0, &h[1].1)
            .await
            .assert_status_success();
        let user = create_test_user_with_roles(pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        server
            .post("/admin/api/v1/transactions")
            .add_header(&h[0].0, &h[0].1)
            .add_header(&h[1].0, &h[1].1)
            .json(&serde_json::json!({ "user_id": user.id, "transaction_type": "admin_grant", "amount": 1000, "source_id": admin.id }))
            .await
            .assert_status_success();

        Self {
            server,
            bg: bg_services,
            credentials,
            config,
            mock,
            user,
            deployment_id,
        }
    }

    async fn chat(&self, bearer: &str) -> axum_test::TestResponse {
        self.server
            .post("/ai/v1/chat/completions")
            .add_header("Authorization", format!("Bearer {bearer}"))
            .json(&serde_json::json!({ "model": MODEL, "messages": [ { "role": "user", "content": "hi" } ] }))
            .await
    }

    /// Poll until `bearer()` is admitted: onwards reloads its key set asynchronously.
    async fn chat_until_admitted(&self, bearer: impl Fn() -> String) -> axum_test::TestResponse {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let response = self.chat(&bearer()).await;
            if response.status_code() == 200 || std::time::Instant::now() >= deadline {
                return response;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// No upstream request carried an internal credential.
    async fn assert_no_token_reached_upstream(&self) {
        for request in self.mock.received_requests().await.unwrap_or_default() {
            let auth = request
                .headers
                .get("authorization")
                .map(|v| v.to_str().unwrap_or_default().to_string());
            assert!(
                !auth.as_deref().unwrap_or_default().contains("dwi-"),
                "internal credential forwarded upstream: {auth:?}"
            );
        }
    }
}

async fn hidden_key(pool: &PgPool, user: Uuid, purpose: ApiKeyPurpose) -> Uuid {
    let mut conn = pool.acquire().await.unwrap();
    ApiKeys::new(&mut conn).get_or_create_hidden_key(user, purpose, user).await.unwrap()
}

/// A token for a live key authenticates on `/ai` as that key; forged,
/// expired, other-process and deleted-key tokens are refused before any
/// upstream call, and never treated as an ordinary key.
#[dwctl_test_macros::test]
async fn internal_credentials_authenticate_only_valid_tokens_for_live_keys(pool: PgPool) {
    let h = Harness::start(&pool).await;
    let key_id = hidden_key(&pool, h.user.id, ApiKeyPurpose::Realtime).await;
    h.bg.sync_onwards_config(&pool).await.unwrap();

    let ok = h.chat_until_admitted(|| h.credentials.mint(key_id, Duration::from_secs(60))).await;
    assert_eq!(ok.status_code(), 200, "valid token: {}", ok.text());
    assert_eq!(ok.json::<serde_json::Value>()["choices"][0]["message"]["content"], "hi");
    let upstream_calls = h.mock.received_requests().await.unwrap().len();

    let valid = h.credentials.mint(key_id, Duration::from_secs(60));
    let (payload, signature) = valid.rsplit_once('.').unwrap();
    let other_key = payload.replace(&key_id.simple().to_string(), &Uuid::new_v4().simple().to_string());
    let refused = [
        ("forged key id", format!("{other_key}.{signature}")),
        ("expired", h.credentials.mint_expiring_at(key_id, 1)),
        (
            "other process",
            InternalCredentials::generate().mint(key_id, Duration::from_secs(60)),
        ),
        ("unknown key", h.credentials.mint(Uuid::new_v4(), Duration::from_secs(60))),
    ];
    for (case, token) in refused {
        let response = h.chat(&token).await;
        assert_eq!(response.status_code(), 401, "{case}: {}", response.text());
        assert_eq!(response.json::<serde_json::Value>()["error"]["code"], "invalid_api_key", "{case}");
    }

    // A deleted key's tokens stop working once onwards drops the key.
    let mut conn = pool.acquire().await.unwrap();
    assert!(ApiKeys::new(&mut conn).delete(key_id).await.unwrap());
    drop(conn);
    h.bg.sync_onwards_config(&pool).await.unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut status = h.chat(&h.credentials.mint(key_id, Duration::from_secs(60))).await.status_code();
    while status != 401 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
        status = h.chat(&h.credentials.mint(key_id, Duration::from_secs(60))).await.status_code();
    }
    assert_eq!(status, 401, "deleted key");

    assert_eq!(
        h.mock.received_requests().await.unwrap().len(),
        upstream_calls,
        "no refused token reached the upstream"
    );
    h.assert_no_token_reached_upstream().await;
}

/// The playground authenticates as the user's hidden playground key, and a
/// health probe as the system key, both with internal credentials, so
/// neither needs a key's secret.
#[dwctl_test_macros::test]
async fn playground_and_probes_work_without_key_secrets(pool: PgPool) {
    let h = Harness::start(&pool).await;
    // The playground key is created on first use; create it up front so the
    // key set can be synced before the first request.
    hidden_key(&pool, h.user.id, ApiKeyPurpose::Playground).await;
    h.bg.sync_onwards_config(&pool).await.unwrap();

    let session = add_auth_headers(&h.user);
    let playground = || {
        h.server
            .post("/admin/api/v1/ai/v1/chat/completions")
            .add_header(&session[0].0, &session[0].1)
            .add_header(&session[1].0, &session[1].1)
            .json(&serde_json::json!({ "model": MODEL, "messages": [ { "role": "user", "content": "hi" } ] }))
    };
    // The routing cache reloads asynchronously after the setup writes, and a
    // loaded machine can take seconds: poll for each path, with a deadline
    // only a real failure reaches.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut response = playground().await;
    while response.status_code() != 200 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
        response = playground().await;
    }
    assert_eq!(response.status_code(), 200, "playground: {}", response.text());
    assert_eq!(response.json::<serde_json::Value>()["choices"][0]["message"]["content"], "hi");

    // A probe calls this process's `/ai` over HTTP as the system key.
    let probe = || ProbeManager::test_probe(&pool, h.deployment_id, &h.config, &h.credentials, None, None, None);
    let mut result = probe().await.expect("probe runs");
    while !result.success && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
        result = probe().await.expect("probe runs");
    }
    assert!(result.success, "probe: {:?} {:?}", result.status_code, result.error_message);
    assert_eq!(result.status_code, Some(200));

    h.assert_no_token_reached_upstream().await;
}

/// A background playground request that an inner layer refuses before
/// onwards sees it (here protocol translation, for an unsupported server
/// tool) still has its pre-created row completed with the failure, so it
/// never stays `processing`.
#[dwctl_test_macros::test]
async fn background_playground_request_refused_inside_the_stack_is_recorded(pool: PgPool) {
    let h = Harness::start(&pool).await;
    hidden_key(&pool, h.user.id, ApiKeyPurpose::Playground).await;
    h.bg.sync_onwards_config(&pool).await.unwrap();

    let session = add_auth_headers(&h.user);
    let accepted = h
        .server
        .post("/admin/api/v1/ai/v1/responses")
        .add_header(&session[0].0, &session[0].1)
        .add_header(&session[1].0, &session[1].1)
        .json(&serde_json::json!({
            "model": MODEL,
            "input": "hi",
            "background": true,
            "service_tier": "priority",
            "tools": [ { "type": "file_search", "vector_store_ids": ["vs_1"] } ]
        }))
        .await;
    assert_eq!(accepted.status_code(), 202, "{}", accepted.text());
    let response_id = accepted.json::<serde_json::Value>()["id"].as_str().unwrap().to_string();
    let request_id: Uuid = response_id.trim_start_matches("resp_").parse().unwrap();

    let state = || async {
        sqlx::query_scalar::<_, String>("SELECT state::text FROM fusillade.requests WHERE id = $1")
            .bind(request_id)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut current = state().await;
    while current == "processing" && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
        current = state().await;
    }
    assert_eq!(current, "failed", "the refusal is recorded on the row");
    assert!(
        h.mock.received_requests().await.unwrap_or_default().is_empty(),
        "the refused request never reached the upstream"
    );
}

/// The analytics row of the daemon's successful dispatch of a fusillade
/// request: the key it authenticated as. Analytics are written asynchronously, so poll.
async fn dispatched_as(pool: &PgPool, request_filter: &str, bind: Uuid) -> Option<Uuid> {
    let query = format!(
        "SELECT api_key_id FROM http_analytics WHERE {request_filter} AND fusillade_request_id IS NOT NULL AND status_code = 200 LIMIT 1"
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let row: Option<Option<Uuid>> = sqlx::query_scalar(&query).bind(bind).fetch_optional(pool).await.unwrap();
        if let Some(key) = row {
            return key;
        }
        assert!(std::time::Instant::now() < deadline, "no analytics row for the dispatch");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Batch and flex requests record the key id they run as, and the daemon's dispatch authenticates as that key with a fresh internal
/// credential: a flex request as the caller's hidden batch key (or the
/// caller's cap-scope child), a batch as the key the batch names.
#[dwctl_test_macros::test]
async fn daemon_dispatches_batch_and_flex_requests_as_their_key_id(pool: PgPool) {
    let h = Harness::start_with(&pool, |config| {
        config.background_services.batch_daemon.enabled = crate::config::DaemonEnabled::Always;
        config.background_services.batch_daemon.claim_interval_ms = 50;
    })
    .await;
    let caller = crate::test::utils::create_test_api_key_for_user(&pool, h.user.id).await;
    let batch_key = hidden_key(&pool, h.user.id, ApiKeyPurpose::Batch).await;
    h.bg.sync_onwards_config(&pool).await.unwrap();

    // Flex: the caller's key is resolved to the hidden batch key at submit.
    let flex = || {
        h.server
            .post("/ai/v1/chat/completions")
            .add_header("Authorization", format!("Bearer {}", caller.secret))
            .json(&serde_json::json!({ "model": MODEL, "service_tier": "flex", "messages": [ { "role": "user", "content": "hi" } ] }))
    };
    let response = flex().await;
    assert_eq!(response.status_code(), 200, "flex: {}", response.text());
    assert_eq!(response.json::<serde_json::Value>()["choices"][0]["message"]["content"], "hi");
    let stored: Vec<Option<Uuid>> = sqlx::query_scalar("SELECT api_key_id FROM fusillade.request_templates_all WHERE file_id IS NULL")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(stored, vec![Some(batch_key)], "flex stores the key id");
    assert_eq!(
        dispatched_as(&pool, "api_key_id = $1", batch_key).await,
        Some(batch_key),
        "flex dispatch authenticates as the hidden batch key"
    );

    // A capped caller's flex request runs on its cap-scope child.
    let child = {
        let mut conn = pool.acquire().await.unwrap();
        ApiKeys::new(&mut conn).get_or_create_child_hidden_key(caller.id).await.unwrap()
    };
    h.bg.sync_onwards_config(&pool).await.unwrap();
    let response = flex().await;
    assert_eq!(response.status_code(), 200, "capped flex: {}", response.text());
    assert_eq!(dispatched_as(&pool, "api_key_id = $1", child).await, Some(child));

    // Batch: the batch names the key it runs as.
    let session = add_auth_headers(&h.user);
    let jsonl = format!(
        "{}\n",
        serde_json::json!({ "custom_id": "r1", "method": "POST", "url": "/v1/chat/completions",
            "body": { "model": MODEL, "messages": [ { "role": "user", "content": "hi" } ] } })
    );
    let file = h
        .server
        .post("/ai/v1/files")
        .add_header(&session[0].0, &session[0].1)
        .add_header(&session[1].0, &session[1].1)
        .multipart(
            axum_test::multipart::MultipartForm::new()
                .add_text("purpose", "batch")
                .add_part("file", axum_test::multipart::Part::bytes(jsonl.into_bytes()).file_name("in.jsonl")),
        )
        .await;
    assert_eq!(file.status_code(), 201, "upload: {}", file.text());
    let file = file.json::<serde_json::Value>();
    let batch = h
        .server
        .post("/ai/v1/batches")
        .add_header(&session[0].0, &session[0].1)
        .add_header(&session[1].0, &session[1].1)
        .json(&serde_json::json!({ "input_file_id": file["id"], "endpoint": "/v1/chat/completions", "completion_window": "24h" }))
        .await;
    assert_eq!(batch.status_code(), 201, "batch: {}", batch.text());
    let batch_id: Uuid = batch.json::<serde_json::Value>()["id"]
        .as_str()
        .unwrap()
        .trim_start_matches("batch_")
        .parse()
        .unwrap();
    let named_key: Option<Uuid> = sqlx::query_scalar("SELECT api_key_id FROM fusillade.batches WHERE id = $1")
        .bind(batch_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let named_key = named_key.expect("the batch names its key");
    assert_eq!(named_key, batch_key, "the batch runs as the hidden batch key");
    assert_eq!(
        dispatched_as(&pool, "fusillade_batch_id = $1", batch_id).await,
        Some(named_key),
        "batch dispatch authenticates as the batch's key"
    );

    h.assert_no_token_reached_upstream().await;
}
