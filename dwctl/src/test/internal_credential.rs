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
use crate::test::utils::{add_auth_headers, create_test_admin_user, create_test_config, create_test_user};

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
        let user = create_test_user(pool, Role::StandardUser).await;
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
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
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
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
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
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut response = playground().await;
    while response.status_code() != 200 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
        response = playground().await;
    }
    assert_eq!(response.status_code(), 200, "playground: {}", response.text());
    assert_eq!(response.json::<serde_json::Value>()["choices"][0]["message"]["content"], "hi");

    // A probe calls this process's `/ai` over HTTP as the system key.
    let result = ProbeManager::test_probe(&pool, h.deployment_id, &h.config, &h.credentials, None, None, None)
        .await
        .expect("probe runs");
    assert!(result.success, "probe: {:?} {:?}", result.status_code, result.error_message);
    assert_eq!(result.status_code, Some(200));

    h.assert_no_token_reached_upstream().await;
}
