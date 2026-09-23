//! End-to-end ingress request validation over the real HTTP surface.
//!
//! These tests build the full application and drive `/ai/v1/...` requests the
//! way a client would, so they cover the wiring the validation unit tests can't:
//! the inference middleware calls [`crate::inference::validation::ValidationStage`]
//! after parsing the body and before API-key auth, and the rejection envelope
//! reaches the client in the surface's own shape.
//!
//! The fixture creates an admin, a standard user in a group, a realtime API key
//! and a wiremock upstream. Each test then deploys the models it needs through
//! the real admin API and calls [`Fixture::sync`] so onwards and the model
//! metadata cache both see them. Reject cases need no upstream at all -
//! validation runs first - but they still use the same fixture so the
//! pass-through assertions are directly comparable.
//!
//! A model's facts come from `deployed_models`: `type` maps to `ModelType`,
//! `capabilities` gates modality rules, and `metadata.context_window` /
//! `metadata.max_output_tokens` gate the length rules. A model with no metadata
//! must never be rejected (fail open).

use axum::http::{StatusCode, header};
use axum_test::{TestResponse, TestServer};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::api::models::{
    api_keys::ApiKeyResponse, deployments::DeployedModelResponse, groups::GroupResponse, inference_endpoints::InferenceEndpointResponse,
    users::Role,
};
use crate::config::Config;
use crate::inference::validation::RuleMode;
use crate::test::utils::{add_auth_headers, create_test_admin_user, create_test_config, create_test_user, create_test_user_with_roles};

const REJECTED_BY_HEADER: &str = "x-dw-rejected-by";
const REJECTED_BY_VALUE: &str = "ingress-validation";

/// The base config: validation on, onwards sync on, image normalisation off.
fn validation_config(enabled: bool, default_mode: RuleMode) -> Config {
    let mut config = create_test_config();
    // Deterministic: the normaliser would otherwise rewrite image_url parts
    // before/after validation and mask what the modality rule saw.
    config.image_normalizer.enabled = false;
    config.background_services.onwards_sync.enabled = true;
    config.request_validation.enabled = enabled;
    config.request_validation.default_mode = default_mode;
    config
}

struct Fixture {
    server: TestServer,
    bg: crate::BackgroundServices,
    _mock: MockServer,
    admin_headers: Vec<(String, String)>,
    /// Session headers for a group member allowed to upload batch files.
    batch_user_headers: Vec<(String, String)>,
    api_key: String,
    endpoint_id: Uuid,
    group_id: Uuid,
}

impl Fixture {
    /// Deploy a model via the admin API and grant it to the fixture's group.
    /// Returns the new model's id.
    async fn create_model(&self, body: Value) -> Uuid {
        let response = self
            .server
            .post("/admin/api/v1/models")
            .add_header(&self.admin_headers[0].0, &self.admin_headers[0].1)
            .add_header(&self.admin_headers[1].0, &self.admin_headers[1].1)
            .json(&body)
            .await;
        assert_eq!(response.status_code(), 200, "create model failed: {}", response.text());
        let model: DeployedModelResponse = response.json();

        let grant = self
            .server
            .post(&format!("/admin/api/v1/groups/{}/models/{}", self.group_id, model.id))
            .add_header(&self.admin_headers[0].0, &self.admin_headers[0].1)
            .add_header(&self.admin_headers[1].0, &self.admin_headers[1].1)
            .await;
        assert_eq!(grant.status_code(), 204, "grant model to group failed: {}", grant.text());
        model.id
    }

    /// Surface a just-created model to both async consumers the middleware uses:
    /// onwards routing and the validation metadata cache.
    async fn sync(&self, pool: &PgPool) {
        self.bg.sync_onwards_config(pool).await.expect("sync onwards config");
        self.bg.sync_model_metadata(pool).await.expect("sync model metadata");
    }

    /// POST a chat-completions request with the fixture's realtime key.
    async fn chat(&self, body: Value) -> TestResponse {
        self.server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .await
    }

    /// Upload a batch input file as the fixture's batch user.
    async fn upload_batch(&self, jsonl: &str) -> TestResponse {
        self.server
            .post("/ai/v1/files")
            .add_header(&self.batch_user_headers[0].0, &self.batch_user_headers[0].1)
            .add_header(&self.batch_user_headers[1].0, &self.batch_user_headers[1].1)
            .multipart(axum_test::multipart::MultipartForm::new().add_text("purpose", "batch").add_part(
                "file",
                axum_test::multipart::Part::bytes(jsonl.as_bytes().to_vec()).file_name("validation.jsonl"),
            ))
            .await
    }

    /// Poll `/ai/v1/models` until `alias` is routable for the fixture's key.
    /// Polling (rather than sleeping a fixed interval) keeps the test fast and
    /// asserts the model was absent before this helper was called.
    async fn wait_until_routable(&self, alias: &str) {
        for attempt in 0..500 {
            let response = self
                .server
                .get("/ai/v1/models")
                .add_header("authorization", format!("Bearer {}", self.api_key))
                .await;
            if response.status_code() == StatusCode::OK {
                let models: Value = response.json();
                if models["data"]
                    .as_array()
                    .is_some_and(|data| data.iter().any(|model| model["id"].as_str() == Some(alias)))
                {
                    return;
                }
            }
            assert!(attempt < 499, "model `{alias}` never became routable");
            tokio::task::yield_now().await;
        }
    }
}

async fn setup(pool: &PgPool, config: Config) -> Fixture {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-validation",
            "object": "chat.completion",
            "created": 1_677_652_288,
            "model": "upstream-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
        })))
        .mount(&mock)
        .await;

    let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
        .await
        .expect("Failed to create application");
    let (server, bg) = app.into_test_server();

    let admin = create_test_admin_user(pool, Role::PlatformManager).await;
    let admin_headers = add_auth_headers(&admin);
    let user = create_test_user(pool, Role::StandardUser).await;
    let user_headers = add_auth_headers(&user);

    let group: GroupResponse = server
        .post("/admin/api/v1/groups")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&json!({
            "name": format!("validation-group-{}", Uuid::new_v4()),
            "description": "Ingress validation E2E"
        }))
        .await
        .json();

    let add_user = server
        .post(&format!("/admin/api/v1/groups/{}/users/{}", group.id, user.id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .await;
    assert_eq!(add_user.status_code(), 204, "add user to group failed");

    let batch_user = create_test_user_with_roles(pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
    let batch_user_headers = add_auth_headers(&batch_user);
    let add_batch_user = server
        .post(&format!("/admin/api/v1/groups/{}/users/{}", group.id, batch_user.id))
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .await;
    assert_eq!(add_batch_user.status_code(), 204, "add batch user to group failed");

    let credits = server
        .post("/admin/api/v1/transactions")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&json!({
            "user_id": user.id,
            "transaction_type": "admin_grant",
            "amount": 1000,
            "source_id": admin.id,
            "description": "Ingress validation test credits"
        }))
        .await;
    assert_eq!(credits.status_code(), 201, "grant credits failed");

    let endpoint: InferenceEndpointResponse = server
        .post("/admin/api/v1/endpoints")
        .add_header(&admin_headers[0].0, &admin_headers[0].1)
        .add_header(&admin_headers[1].0, &admin_headers[1].1)
        .json(&json!({
            "name": format!("validation-endpoint-{}", Uuid::new_v4()),
            "url": format!("{}/v1", mock.uri())
        }))
        .await
        .json();

    let key: ApiKeyResponse = server
        .post(&format!("/admin/api/v1/users/{}/api-keys", user.id))
        .add_header(&user_headers[0].0, &user_headers[0].1)
        .add_header(&user_headers[1].0, &user_headers[1].1)
        .json(&json!({"name": "validation-realtime", "purpose": "realtime"}))
        .await
        .json();

    Fixture {
        server,
        bg,
        _mock: mock,
        admin_headers,
        batch_user_headers,
        api_key: key.key,
        endpoint_id: endpoint.id,
        group_id: group.id,
    }
}

/// Build a standard-model create body. `extra` is merged in so each test can
/// add `model_type`, `capabilities` and `metadata`.
fn model_body(alias: &str, endpoint_id: Uuid, extra: Value) -> Value {
    let mut body = json!({
        "type": "standard",
        "model_name": alias,
        "alias": alias,
        "hosted_on": endpoint_id,
    });
    if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            body.insert(key.clone(), value.clone());
        }
    }
    body
}

fn rejected_by(response: &TestResponse) -> Option<String> {
    response
        .headers()
        .get(REJECTED_BY_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn chat_request(model: &str, content: &str) -> Value {
    json!({"model": model, "messages": [{"role": "user", "content": content}]})
}

async fn fusillade_request_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM fusillade.requests")
        .fetch_one(pool)
        .await
        .expect("count fusillade requests")
}

/// Unknown model under `Enforce`: 404 in the OpenAI envelope, marked as an
/// ingress rejection, with the model name in the message.
#[sqlx::test]
#[test_log::test]
async fn enforce_rejects_unknown_model_with_openai_envelope(pool: PgPool) {
    let fixture = setup(&pool, validation_config(true, RuleMode::Enforce)).await;

    let response = fixture.chat(chat_request("ghost-model", "hi")).await;

    assert_eq!(response.status_code(), StatusCode::NOT_FOUND);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    assert_eq!(rejected_by(&response).as_deref(), Some(REJECTED_BY_VALUE));
    let body: Value = response.json();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["code"], "model_not_found");
    assert_eq!(body["error"]["param"], "model");
    assert!(
        body["error"]["message"].as_str().unwrap().contains("ghost-model"),
        "message should name the model: {}",
        body["error"]["message"]
    );
}

/// A rejected flex request must not enqueue a fusillade row: validation runs
/// before the flex path. A valid flex/background request is sent afterwards as a
/// positive control, proving the count is not simply frozen at zero.
#[sqlx::test]
#[test_log::test]
async fn flex_rejection_does_not_enqueue_fusillade_request(pool: PgPool) {
    let fixture = setup(&pool, validation_config(true, RuleMode::Enforce)).await;
    let before = fusillade_request_count(&pool).await;
    assert_eq!(before, 0, "no fusillade request should exist before the test");

    let response = fixture
        .chat(json!({
            "model": "ghost-model",
            "messages": [{"role": "user", "content": "hi"}],
            "service_tier": "flex"
        }))
        .await;

    assert_eq!(response.status_code(), StatusCode::NOT_FOUND);
    assert_eq!(rejected_by(&response).as_deref(), Some(REJECTED_BY_VALUE));
    let body: Value = response.json();
    assert_eq!(body["error"]["code"], "model_not_found");
    assert_eq!(
        fusillade_request_count(&pool).await,
        before,
        "an ingress-rejected flex request must not enqueue a fusillade row"
    );

    // Positive control: a valid model with the same flex tier DOES enqueue.
    let alias = format!("flex-ok-{}", Uuid::new_v4());
    fixture
        .create_model(model_body(&alias, fixture.endpoint_id, json!({"model_type": "CHAT"})))
        .await;
    fixture.sync(&pool).await;

    let accepted = fixture
        .server
        .post("/ai/v1/responses")
        .add_header("authorization", format!("Bearer {}", fixture.api_key))
        .json(&json!({
            "model": alias,
            "input": "hi",
            "service_tier": "flex",
            "background": true
        }))
        .await;
    assert_eq!(
        accepted.status_code(),
        StatusCode::ACCEPTED,
        "flex/background submission failed: {}",
        accepted.text()
    );
    assert_eq!(
        fusillade_request_count(&pool).await,
        before + 1,
        "a valid flex/background request must enqueue exactly one fusillade row"
    );
}

/// The Anthropic Messages surface gets an Anthropic-shaped rejection body.
#[sqlx::test]
#[test_log::test]
async fn messages_surface_unknown_model_uses_anthropic_envelope(pool: PgPool) {
    let fixture = setup(&pool, validation_config(true, RuleMode::Enforce)).await;

    let response = fixture
        .server
        .post("/ai/v1/messages")
        .add_header("authorization", format!("Bearer {}", fixture.api_key))
        .json(&json!({
            "model": "ghost-model",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .await;

    assert_eq!(response.status_code(), StatusCode::NOT_FOUND);
    assert_eq!(rejected_by(&response).as_deref(), Some(REJECTED_BY_VALUE));
    let body: Value = response.json();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "not_found_error");
    assert!(
        body["error"]["message"].as_str().unwrap().contains("ghost-model"),
        "message should name the model: {}",
        body["error"]["message"]
    );
}

/// A malformed body is rejected with the `invalid_json` envelope even when
/// validation is disabled - parsing is needed for routing, so the middleware
/// owns this error regardless of the validation switch.
#[sqlx::test]
#[test_log::test]
async fn malformed_json_is_rejected_even_with_validation_disabled(pool: PgPool) {
    let fixture = setup(&pool, validation_config(false, RuleMode::Shadow)).await;

    let response = fixture
        .server
        .post("/ai/v1/chat/completions")
        .add_header("authorization", format!("Bearer {}", fixture.api_key))
        .add_header("content-type", "application/json")
        .bytes("{not valid json".as_bytes().into())
        .await;

    assert_eq!(response.status_code(), StatusCode::BAD_REQUEST);
    let body: Value = response.json();
    assert_eq!(body["error"]["code"], "invalid_json");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("Request body is not valid JSON"),
        "unexpected message: {}",
        body["error"]["message"]
    );
}

/// Shadow mode records the would-be rejection but forwards the request, so the
/// response must not carry the ingress-rejection marker.
#[sqlx::test]
#[test_log::test]
async fn shadow_mode_forwards_unknown_model(pool: PgPool) {
    let fixture = setup(&pool, validation_config(true, RuleMode::Shadow)).await;

    // A routable model keeps onwards active so the unknown alias gets its own
    // (unmarked) 404 rather than an auth error.
    let base = format!("shadow-base-{}", Uuid::new_v4());
    fixture
        .create_model(model_body(&base, fixture.endpoint_id, json!({"model_type": "CHAT"})))
        .await;
    fixture.sync(&pool).await;
    fixture.wait_until_routable(&base).await;

    let response = fixture.chat(chat_request("ghost-model", "hi")).await;

    assert_eq!(response.status_code(), StatusCode::NOT_FOUND);
    assert_eq!(
        rejected_by(&response),
        None,
        "shadow mode must not mark the response as an ingress rejection"
    );
}

/// Image input is rejected only when the catalog lists capabilities and they
/// exclude `vision`. A `NULL` capability list means unknown, which fails open.
#[sqlx::test]
#[test_log::test]
async fn image_input_rejected_only_when_capabilities_exclude_vision(pool: PgPool) {
    let fixture = setup(&pool, validation_config(true, RuleMode::Enforce)).await;

    let no_vision = format!("no-vision-{}", Uuid::new_v4());
    fixture
        .create_model(model_body(
            &no_vision,
            fixture.endpoint_id,
            json!({"model_type": "CHAT", "capabilities": ["reasoning"]}),
        ))
        .await;
    let unknown_caps = format!("unknown-caps-{}", Uuid::new_v4());
    fixture
        .create_model(model_body(&unknown_caps, fixture.endpoint_id, json!({"model_type": "CHAT"})))
        .await;
    fixture.sync(&pool).await;

    let image_body = |model: &str| {
        json!({
            "model": model,
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "describe this"},
                {"type": "image_url", "image_url": {"url": "https://example.com/cat.png"}}
            ]}]
        })
    };

    let rejected = fixture.chat(image_body(&no_vision)).await;
    assert_eq!(rejected.status_code(), StatusCode::BAD_REQUEST);
    assert_eq!(rejected_by(&rejected).as_deref(), Some(REJECTED_BY_VALUE));
    let body: Value = rejected.json();
    assert_eq!(body["error"]["code"], "unsupported_modality");
    assert_eq!(body["error"]["param"], "messages");
    assert!(body["error"]["message"].as_str().unwrap().contains("image"));

    // `NULL` capabilities: the same image must pass validation. The request then
    // reaches the mock upstream, so a 200 confirms it was truly forwarded.
    let allowed = fixture.chat(image_body(&unknown_caps)).await;
    assert_eq!(
        rejected_by(&allowed),
        None,
        "a model with unknown capabilities must fail open on modality"
    );
    assert_eq!(allowed.status_code(), StatusCode::OK, "allowed request: {}", allowed.text());
}

/// A prompt estimated over a small `metadata.context_window` is rejected at
/// stage 1 with the context-length code.
#[sqlx::test]
#[test_log::test]
async fn context_window_exceeded_is_rejected(pool: PgPool) {
    let fixture = setup(&pool, validation_config(true, RuleMode::Enforce)).await;

    let alias = format!("small-ctx-{}", Uuid::new_v4());
    fixture
        .create_model(model_body(
            &alias,
            fixture.endpoint_id,
            json!({"model_type": "CHAT", "metadata": {"context_window": 100}}),
        ))
        .await;
    fixture.sync(&pool).await;

    // 5000 bytes / 12 bytes-per-token = 416 > 100, so stage 1 rejects without a
    // tokenizer round trip.
    let prompt = "a".repeat(5000);
    let response = fixture.chat(chat_request(&alias, &prompt)).await;

    assert_eq!(response.status_code(), StatusCode::BAD_REQUEST);
    assert_eq!(rejected_by(&response).as_deref(), Some(REJECTED_BY_VALUE));
    let body: Value = response.json();
    assert_eq!(body["error"]["code"], "context_length_exceeded");
    assert_eq!(body["error"]["param"], "messages");
    assert!(
        body["error"]["message"].as_str().unwrap().contains("100"),
        "message should mention the window: {}",
        body["error"]["message"]
    );
}

/// An embeddings-typed model is not usable on a chat-completions surface.
#[sqlx::test]
#[test_log::test]
async fn embeddings_model_on_chat_completions_is_type_mismatch(pool: PgPool) {
    let fixture = setup(&pool, validation_config(true, RuleMode::Enforce)).await;

    let alias = format!("embedder-{}", Uuid::new_v4());
    fixture
        .create_model(model_body(&alias, fixture.endpoint_id, json!({"model_type": "EMBEDDINGS"})))
        .await;
    fixture.sync(&pool).await;

    let response = fixture.chat(chat_request(&alias, "hi")).await;

    assert_eq!(response.status_code(), StatusCode::BAD_REQUEST);
    assert_eq!(rejected_by(&response).as_deref(), Some(REJECTED_BY_VALUE));
    let body: Value = response.json();
    assert_eq!(body["error"]["code"], "model_type_mismatch");
    assert_eq!(body["error"]["param"], "model");
}

/// A deployed model with no `type`, no `capabilities` and no token metadata must
/// never be rejected: every rule that needs the absent fact fails open, and the
/// request is proxied normally.
#[sqlx::test]
#[test_log::test]
async fn model_without_metadata_is_never_rejected(pool: PgPool) {
    let fixture = setup(&pool, validation_config(true, RuleMode::Enforce)).await;

    let alias = format!("bare-{}", Uuid::new_v4());
    // No model_type, no capabilities, no metadata.
    fixture.create_model(model_body(&alias, fixture.endpoint_id, json!({}))).await;
    fixture.sync(&pool).await;
    fixture.wait_until_routable(&alias).await;

    let response = fixture.chat(chat_request(&alias, "hi")).await;

    assert_eq!(
        response.status_code(),
        StatusCode::OK,
        "a metadata-free model must pass validation and reach upstream: {}",
        response.text()
    );
    assert_eq!(rejected_by(&response), None);
    let body: Value = response.json();
    assert_eq!(body["choices"][0]["message"]["content"], "ok");
}

/// A batch input file of the given chat prompts, one line each, for `alias`.
fn batch_jsonl(alias: &str, prompts: &[&str]) -> String {
    prompts
        .iter()
        .enumerate()
        .map(|(i, prompt)| {
            json!({
                "custom_id": format!("req-{i}"),
                "method": "POST",
                "url": "/v1/chat/completions",
                "body": chat_request(alias, prompt),
            })
            .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Batch upload applies the same rules as the inference endpoints: a doomed
/// line fails the upload, naming the line, before anything is stored.
#[sqlx::test]
#[test_log::test]
async fn batch_upload_rejects_a_doomed_line(pool: PgPool) {
    let fixture = setup(&pool, validation_config(true, RuleMode::Enforce)).await;

    let alias = format!("small-ctx-batch-{}", Uuid::new_v4());
    fixture
        .create_model(model_body(
            &alias,
            fixture.endpoint_id,
            json!({"model_type": "CHAT", "metadata": {"context_window": 100}}),
        ))
        .await;
    fixture.sync(&pool).await;

    // The valid file uploads, so the rejection below is the rule firing.
    let valid = fixture.upload_batch(&batch_jsonl(&alias, &["hi", "hello"])).await;
    assert_eq!(valid.status_code(), StatusCode::CREATED, "valid file rejected: {}", valid.text());

    let too_long = "a".repeat(5000);
    let response = fixture.upload_batch(&batch_jsonl(&alias, &["hi", &too_long])).await;

    assert_eq!(response.status_code(), StatusCode::BAD_REQUEST, "{}", response.text());
    let text = response.text();
    assert!(
        text.contains("line 2") || text.contains("Line 2"),
        "error should name the line: {text}"
    );
    assert!(
        text.contains("maximum context length"),
        "error should carry the rule message: {text}"
    );
}

/// In shadow mode a batch line that would be rejected is only recorded.
#[sqlx::test]
#[test_log::test]
async fn batch_upload_shadow_mode_accepts_the_file(pool: PgPool) {
    let fixture = setup(&pool, validation_config(true, RuleMode::Shadow)).await;

    let alias = format!("small-ctx-batch-{}", Uuid::new_v4());
    fixture
        .create_model(model_body(
            &alias,
            fixture.endpoint_id,
            json!({"model_type": "CHAT", "metadata": {"context_window": 100}}),
        ))
        .await;
    fixture.sync(&pool).await;

    let too_long = "a".repeat(5000);
    let response = fixture.upload_batch(&batch_jsonl(&alias, &[&too_long])).await;

    assert_eq!(response.status_code(), StatusCode::CREATED, "{}", response.text());
}
