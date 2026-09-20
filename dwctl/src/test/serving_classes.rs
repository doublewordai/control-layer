//! Regression tests across DWCTL ingress, strict Onwards and batch upload.
use axum::http::StatusCode;
use axum_test::multipart::{MultipartForm, Part};
use axum_test::{TestResponse, TestServer};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wiremock::{Mock, MockServer, Request, ResponseTemplate, matchers::method};

use crate::api::models::users::Role;
use crate::test::utils::{add_auth_headers, create_test_admin_user, create_test_config, create_test_user_with_roles};
use crate::{Application, BackgroundServices};

struct Fixture {
    server: TestServer,
    upstream: MockServer,
    services: BackgroundServices,
    user: Uuid,
    key: String,
}

impl Fixture {
    async fn new(pool: &PgPool) -> Self {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(|request: &Request| {
                let body: Value = request.body_json().unwrap();
                let common = json!({"id":"chatcmpl-test", "created":1, "model":body["model"]});
                if body["stream"] == true {
                    let chunk = json!({"id":common["id"],"created":1,"model":body["model"],"object":"chat.completion.chunk",
                        "choices":[{"index":0,"delta":{"content":"OK"},"finish_reason":"stop"}],
                        "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}});
                    return ResponseTemplate::new(200)
                        .insert_header("content-type", "text/event-stream")
                        .set_body_string(format!("data: {chunk}\n\ndata: [DONE]\n\n"));
                }
                let completion = request.url.path().ends_with("/completions") && !request.url.path().ends_with("/chat/completions");
                ResponseTemplate::new(200).set_body_json(json!({
                    "id":common["id"],"created":1,"model":body["model"],
                    "object":if completion {"text_completion"} else {"chat.completion"},
                    "choices":[if completion {json!({"index":0,"text":"OK","finish_reason":"stop","logprobs":null})}
                        else {json!({"index":0,"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"})}],
                    "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}
                }))
            })
            .mount(&upstream)
            .await;
        let mut config = create_test_config();
        config.onwards.strict_mode = true;
        config.background_services.onwards_sync.enabled = true;
        let app = Application::new_with_pool(config, Some(pool.clone()), None).await.unwrap();
        let (server, services) = app.into_test_server();
        let user = create_test_user_with_roles(pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let admin = create_test_admin_user(pool, Role::PlatformManager).await;
        let headers = add_auth_headers(&admin);
        server
            .post("/admin/api/v1/transactions")
            .add_header(&headers[0].0, &headers[0].1)
            .add_header(&headers[1].0, &headers[1].1)
            .json(&json!({"user_id":user.id,"transaction_type":"admin_grant","amount":100,"source_id":admin.id}))
            .await
            .assert_status(StatusCode::CREATED);
        let key = format!("sk-{}", Uuid::new_v4());
        sqlx::query("INSERT INTO api_keys (name,secret,user_id,created_by,purpose) VALUES ('serving-test',$1,$2,$2,'realtime')")
            .bind(&key)
            .bind(user.id)
            .execute(pool)
            .await
            .unwrap();
        let endpoint: Uuid = sqlx::query_scalar(
            "INSERT INTO inference_endpoints (name,url,created_by,kind) VALUES ('serving-test',$1,$2,'dynamo') RETURNING id",
        )
        .bind(upstream.uri())
        .bind(user.id)
        .fetch_one(pool)
        .await
        .unwrap();
        for alias in ["policy", "no-offer"] {
            let member: Uuid = sqlx::query_scalar(
                "INSERT INTO deployed_models (model_name,alias,hosted_on,created_by,type) VALUES ($1,$2,$3,$4,'CHAT') RETURNING id",
            )
            .bind(format!("upstream-{alias}"))
            .bind(format!("member-{alias}"))
            .bind(endpoint)
            .bind(user.id)
            .fetch_one(pool)
            .await
            .unwrap();
            let presets = if alias == "policy" {
                json!({"interactive":{"ttft_ms":500,"itl_ms":20,"priority":200},"throughput":{"ttft_ms":5000,"itl_ms":100,"priority":100}})
            } else {
                json!({})
            };
            let model: Uuid = sqlx::query_scalar("INSERT INTO deployed_models (model_name,alias,created_by,type,is_composite,serving_classes) VALUES ($1,$1,$2,'CHAT',true,$3) RETURNING id")
                .bind(alias).bind(user.id).bind(presets).fetch_one(pool).await.unwrap();
            sqlx::query("INSERT INTO deployed_model_components (composite_model_id,deployed_model_id) VALUES ($1,$2)")
                .bind(model)
                .bind(member)
                .execute(pool)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO deployment_groups (deployment_id,group_id,granted_by) VALUES ($1,'00000000-0000-0000-0000-000000000000',$2)",
            )
            .bind(model)
            .bind(user.id)
            .execute(pool)
            .await
            .unwrap();
            sqlx::query("INSERT INTO model_tariffs (deployed_model_id,name,input_price_per_token,output_price_per_token,api_key_purpose) VALUES ($1,'test',$2,$2,'realtime')")
                .bind(model).bind(Decimal::new(1,6))
                .execute(pool).await.unwrap();
        }
        services.sync_onwards_config(pool).await.unwrap();
        Self {
            server,
            upstream,
            services,
            user: user.id,
            key,
        }
    }

    async fn post(&self, path: &str, model: &str, stream: bool) -> TestResponse {
        let body = match path {
            "responses" => json!({"model":model,"input":"hi","max_output_tokens":16,"stream":stream}),
            "completions" => json!({"model":model,"prompt":"hi","max_tokens":16,"stream":stream}),
            _ => json!({"model":model,"messages":[{"role":"user","content":"hi"}],"max_tokens":16,"stream":stream}),
        };
        self.server
            .post(&format!("/ai/v1/{path}"))
            .add_header("Authorization", format!("Bearer {}", self.key))
            .json(&body)
            .await
    }

    async fn last_body(&self) -> Value {
        self.upstream
            .received_requests()
            .await
            .unwrap()
            .last()
            .unwrap()
            .body_json()
            .unwrap()
    }
}

#[sqlx::test]
async fn serving_classes_survive_strict_and_translated_ingress(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    f.post("chat/completions", "policy:interactive", false)
        .await
        .assert_status(StatusCode::FORBIDDEN);
    assert!(f.upstream.received_requests().await.unwrap().is_empty());
    sqlx::query(
        "UPDATE users SET granted_serving_classes=ARRAY['interactive','throughput'],default_serving_class='interactive' WHERE id=$1",
    )
    .bind(f.user)
    .execute(&pool)
    .await
    .unwrap();
    f.services.sync_onwards_config(&pool).await.unwrap();
    for path in ["chat/completions", "responses", "messages", "completions"] {
        f.post(path, "policy:throughput", false).await.assert_status_ok();
        let body = f.last_body().await;
        assert_eq!(body["model"], "upstream-policy", "{path}");
        assert_eq!(body["nvext"]["router"]["ttft_target"], 5000, "{path}");
        assert_eq!(body["nvext"]["agent_hints"]["priority"], 100, "{path}");
        f.post(path, "policy:standard", false).await.assert_status_ok();
        assert!(f.last_body().await.get("nvext").is_none(), "{path}: explicit standard opts down");
        f.post(path, "no-offer:interactive", false)
            .await
            .assert_status(StatusCode::FORBIDDEN);
    }
    let response = f.post("chat/completions", "policy:throughput", true).await;
    response.assert_status_ok();
    assert!(response.text().contains("[DONE]"));
    assert_eq!(f.last_body().await["nvext"]["agent_hints"]["priority"], 100);
    f.post("chat/completions", "policy:unknown", false)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
    // Multiple suffixes are malformed even if each class is known.
    f.post("chat/completions", "policy:throughput:interactive", false)
        .await
        .assert_status(StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn serving_classes_batch_upload_discards_suffixes(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let headers = format!("Bearer {}", f.key);
    // No final newline exercises the final buffered-line parser as well.
    let lines = [
        "policy:interactive",
        "policy:throughput",
        "policy:standard",
        "policy",
        "policy:x-fast",
    ]
    .into_iter()
    .enumerate()
    .map(|(i, model)| {
        json!({"custom_id":format!("r{i}"),"method":"POST","url":"/v1/chat/completions",
            "body":{"model":model,"messages":[{"role":"user","content":"hi"}]}})
        .to_string()
    })
    .collect::<Vec<_>>()
    .join("\n");
    let response = f
        .server
        .post("/ai/v1/files")
        .add_header("Authorization", &headers)
        .multipart(
            MultipartForm::new()
                .add_text("purpose", "batch")
                .add_part("file", Part::bytes(lines.into_bytes()).file_name("classes.jsonl")),
        )
        .await;
    response.assert_status(StatusCode::CREATED);
    let id = response.json::<Value>()["id"].as_str().unwrap().to_owned();
    let content = f
        .server
        .get(&format!("/ai/v1/files/{id}/content"))
        .add_header("Authorization", &headers)
        .await;
    content.assert_status_ok();
    let models = content
        .text()
        .lines()
        .map(|line| {
            serde_json::from_str::<Value>(line).unwrap()["body"]["model"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(models, ["policy", "policy", "policy", "policy", "policy"]);

    for (model, expected) in [
        ("inaccessible:interactive", StatusCode::FORBIDDEN),
        ("policy:throughput:interactive", StatusCode::BAD_REQUEST),
        ("policy:", StatusCode::BAD_REQUEST),
    ] {
        let line =
            json!({"custom_id":"denied","method":"POST","url":"/v1/chat/completions","body":{"model":model,"messages":[]}}).to_string();
        f.server
            .post("/ai/v1/files")
            .add_header("Authorization", &headers)
            .multipart(
                MultipartForm::new()
                    .add_text("purpose", "batch")
                    .add_part("file", Part::bytes(line.into_bytes()).file_name("denied.jsonl")),
            )
            .await
            .assert_status(expected);
    }
}

#[sqlx::test]
async fn model_override_cannot_split_routing_and_billing_identity(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    for path in ["chat/completions", "responses", "messages", "completions", "embeddings"] {
        let response = f
            .server
            .post(&format!("/ai/v1/{path}"))
            .add_header("Authorization", format!("Bearer {}", f.key))
            .add_header("Model-Override", "no-offer")
            .json(&json!({"model":"policy","messages":[{"role":"user","content":"hi"}]}))
            .await;
        response.assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(response.json::<Value>()["error"]["code"], "unsupported_header");
    }
    assert!(f.upstream.received_requests().await.unwrap().is_empty());
}

#[sqlx::test]
async fn dispatch_clears_stored_targets_but_preserves_deadline_priority(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    // Daemon loopback skips ingestion, as it does for an old stored template.
    let response = f
        .server
        .post("/ai/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {}", f.key))
        .add_header("x-fusillade-request-id", Uuid::new_v4().to_string())
        .json(&json!({"model":"policy","messages":[{"role":"user","content":"hi"}],
            "nvext":{"router":{"ttft_target":1,"itl_target":1},
                "agent_hints":{"priority":-1700000000},"cache_control":{"enabled":true}}}))
        .await;
    response.assert_status_ok();
    let body = f.last_body().await;
    assert!(body["nvext"].get("router").is_none());
    assert_eq!(body["nvext"]["agent_hints"]["priority"], -1700000000);
    assert_eq!(body["nvext"]["cache_control"]["enabled"], true);
}
