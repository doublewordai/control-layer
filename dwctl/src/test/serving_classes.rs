//! Regression tests across DWCTL ingress, strict Onwards and batch upload:
//! overlay targets, model suffixes and billed serving outcomes.
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
            let model: Uuid = sqlx::query_scalar(
                "INSERT INTO deployed_models (model_name,alias,created_by,type,is_composite) VALUES ($1,$1,$2,'CHAT',true) RETURNING id",
            )
            .bind(alias)
            .bind(user.id)
            .fetch_one(pool)
            .await
            .unwrap();
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

    /// Give the test account an overlay with explicit targets on `policy`, or
    /// remove it, and resync.
    async fn set_overlay(&self, pool: &PgPool, targets: Option<Value>) {
        sqlx::query("DELETE FROM model_overlays WHERE user_id=$1")
            .bind(self.user)
            .execute(pool)
            .await
            .unwrap();
        if let Some(targets) = targets {
            sqlx::query(
                "INSERT INTO model_overlays (user_id,deployed_model_id,targets) SELECT $1,id,$2 FROM deployed_models WHERE alias='policy'",
            )
            .bind(self.user)
            .bind(targets)
            .execute(pool)
            .await
            .unwrap();
        }
        self.services.sync_onwards_config(pool).await.unwrap();
    }
}

#[dwctl_test_macros::test]
async fn overlay_targets_survive_strict_and_translated_ingress(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    f.set_overlay(&pool, Some(json!({"ttft_ms":5000,"itl_ms":100,"priority":100})))
        .await;
    for path in ["chat/completions", "responses", "messages", "completions"] {
        f.post(path, "policy", false).await.assert_status_ok();
        let body = f.last_body().await;
        assert_eq!(body["model"], "upstream-policy", "{path}");
        assert_eq!(body["nvext"]["router"]["ttft_target"], 5000, "{path}");
        assert_eq!(body["nvext"]["agent_hints"]["priority"], 100, "{path}");
        f.post(path, "no-offer", false).await.assert_status_ok();
        assert_eq!(
            f.last_body().await["nvext"],
            json!({"agent_hints": {"priority": 0}}),
            "{path}: a model without an overlay sends realtime's priority alone"
        );
    }
    let response = f.post("chat/completions", "policy", true).await;
    response.assert_status_ok();
    assert!(response.text().contains("[DONE]"));
    assert_eq!(f.last_body().await["nvext"]["agent_hints"]["priority"], 100);
}

#[dwctl_test_macros::test]
async fn realtime_class_suffixes_are_not_parsed(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    for model in ["policy:interactive", "policy:standard", "policy:throughput:interactive"] {
        let response = f.post("chat/completions", model, false).await;
        assert!(response.status_code().is_client_error(), "{model}: {}", response.status_code());
        assert_ne!(response.status_code(), StatusCode::FORBIDDEN, "{model}");
    }
    assert!(f.upstream.received_requests().await.unwrap().is_empty());
}

#[dwctl_test_macros::test]
async fn batch_upload_discards_model_suffixes(pool: PgPool) {
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

#[dwctl_test_macros::test]
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

#[dwctl_test_macros::test]
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

#[dwctl_test_macros::test]
async fn overlay_targets_change_neither_the_class_nor_the_price(pool: PgPool) {
    // The account has a standard-class deal and a class-less one. Targets are
    // scheduling hints: with or without them the request is standard and pays
    // the standard deal.
    let f = Fixture::new(&pool).await;
    sqlx::query("INSERT INTO model_tariffs(deployed_model_id,user_id,serving_class,name,api_key_purpose,input_price_per_token,output_price_per_token) SELECT id,$1,c.class,'org-deal','realtime',c.price,c.price FROM deployed_models CROSS JOIN (VALUES (NULL::text,0.002::numeric),('standard',0.003::numeric)) c(class,price) WHERE alias='policy'")
        .bind(f.user).execute(&pool).await.unwrap();
    f.set_overlay(&pool, Some(json!({"ttft_ms":800,"itl_ms":30,"priority":50}))).await;
    f.post("chat/completions", "policy", false).await.assert_status_ok();
    f.set_overlay(&pool, None).await;
    f.post("chat/completions", "policy", false).await.assert_status_ok();
    let rows = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let rows: Vec<(String, String, Decimal)> = sqlx::query_as("SELECT model,resolved_serving_class,total_cost FROM http_analytics WHERE user_id=$1 AND model='policy' AND status_code=200 ORDER BY resolved_serving_class")
                .bind(f.user).fetch_all(&pool).await.unwrap();
            if rows.len() == 2 { break rows; }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }).await.expect("two priced analytics rows must be persisted");
    assert_eq!(
        rows,
        vec![
            ("policy".to_owned(), "standard".to_owned(), Decimal::new(45, 3)),
            ("policy".to_owned(), "standard".to_owned(), Decimal::new(45, 3)),
        ]
    );
    f.services.shutdown().await;
}

const ORDERED_SCHEMA: &str = r#"{"type":"object","properties":{"zeta":{"type":"string"},"alpha":{"type":"string"}},"required":["zeta","alpha"],"additionalProperties":false}"#;
const ORDERED_PARAMETERS: &str = r#"{"type":"object","properties":{"query":{"type":"string"},"limit":{"type":"integer"}},"required":["query"],"additionalProperties":false}"#;

#[dwctl_test_macros::test]
async fn json_key_order_survives_ingress(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let (schema, parameters) = (ORDERED_SCHEMA, ORDERED_PARAMETERS);
    let requests = [
        (
            "chat/completions",
            format!(
                r#"{{"model":"policy","messages":[{{"role":"user","content":"hi"}}],"stream":true,"response_format":{{"type":"json_schema","json_schema":{{"name":"n","strict":true,"schema":{schema}}}}},"tools":[{{"type":"function","function":{{"name":"lookup","parameters":{parameters}}}}}]}}"#
            ),
            vec![schema, parameters],
        ),
        (
            "responses",
            format!(
                r#"{{"model":"policy","input":"hi","text":{{"format":{{"type":"json_schema","name":"n","strict":true,"schema":{schema}}}}},"tools":[{{"type":"function","name":"lookup","description":"d","parameters":{parameters}}}]}}"#
            ),
            vec![schema, parameters],
        ),
        (
            "messages",
            format!(
                r#"{{"model":"policy","max_tokens":16,"messages":[{{"role":"user","content":"hi"}}],"tools":[{{"name":"lookup","description":"d","input_schema":{parameters}}}]}}"#
            ),
            vec![parameters],
        ),
    ];
    for (path, body, expected) in requests {
        f.server
            .post(&format!("/ai/v1/{path}"))
            .add_header("Authorization", format!("Bearer {}", f.key))
            .content_type("application/json")
            .bytes(body.into())
            .await
            .assert_status_ok();
        let received = f.upstream.received_requests().await.unwrap();
        let forwarded = String::from_utf8(received.last().unwrap().body.clone()).unwrap();
        for fragment in expected {
            assert!(forwarded.contains(fragment), "{path}: {forwarded}");
        }
    }
}

#[dwctl_test_macros::test]
async fn json_key_order_survives_batch_upload(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let headers = format!("Bearer {}", f.key);
    let line = format!(
        r#"{{"custom_id":"r0","method":"POST","url":"/v1/chat/completions","body":{{"model":"policy","messages":[{{"role":"user","content":"hi"}}],"response_format":{{"type":"json_schema","json_schema":{{"name":"n","strict":true,"schema":{ORDERED_SCHEMA}}}}}}}}}"#
    );
    let response = f
        .server
        .post("/ai/v1/files")
        .add_header("Authorization", &headers)
        .multipart(
            MultipartForm::new()
                .add_text("purpose", "batch")
                .add_part("file", Part::bytes(line.into_bytes()).file_name("ordered.jsonl")),
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
    assert!(content.text().contains(ORDERED_SCHEMA), "{}", content.text());
}

/// Activate `policy` as a class-routed model: standard and fast, each its own
/// upstream on the fixture's endpoint, with public class prices.
async fn activate_policy_classes(f: &Fixture, pool: &PgPool) {
    sqlx::query(
        "INSERT INTO model_serving_classes (deployed_model_id, class_key, display_name, inference_endpoint_id, upstream_model_name, realtime_inflight_limit)
         SELECT dm.id, c.key, c.key, ie.id, c.upstream, c.lim
         FROM deployed_models dm, inference_endpoints ie,
              (VALUES ('standard', 'pool-standard', NULL::int), ('fast', 'pool-fast', 3)) c(key, upstream, lim)
         WHERE dm.alias = 'policy' AND ie.name = 'serving-test'",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO model_tariffs (deployed_model_id, serving_class, name, input_price_per_token, output_price_per_token, api_key_purpose)
         SELECT id, c.key, c.key, c.price, c.price, 'realtime' FROM deployed_models,
              (VALUES ('standard', 0.000002::numeric), ('fast', 0.000004::numeric)) c(key, price)
         WHERE alias = 'policy'",
    )
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("UPDATE deployed_models SET routing_mode = 'class_routes' WHERE alias = 'policy'")
        .execute(pool)
        .await
        .unwrap();
    f.services.sync_onwards_config(pool).await.unwrap();
}

#[dwctl_test_macros::test]
async fn class_routes_serve_realtime_by_class_and_bill_the_class(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    activate_policy_classes(&f, &pool).await;

    // Realtime: the alias reaches standard's upstream, alias:fast reaches fast's.
    f.post("chat/completions", "policy", false).await.assert_status_ok();
    assert_eq!(f.last_body().await["model"], "pool-standard");
    f.post("chat/completions", "policy:fast", false).await.assert_status_ok();
    assert_eq!(f.last_body().await["model"], "pool-fast");

    // Flex never runs on a non-standard class.
    let response = f
        .server
        .post("/ai/v1/chat/completions")
        .add_header("Authorization", format!("Bearer {}", f.key))
        .json(&json!({"model": "policy:fast", "messages": [{"role": "user", "content": "hi"}], "service_tier": "flex"}))
        .await;
    response.assert_status(StatusCode::BAD_REQUEST);
    assert!(response.text().contains("serving_class_realtime_only"), "{}", response.text());

    // Both realtime requests are recorded as the model, in their class, at
    // their class's price.
    let rows = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let rows: Vec<(String, Option<String>, Option<String>, Decimal)> = sqlx::query_as(
                "SELECT model, requested_serving_class, resolved_serving_class, total_cost FROM http_analytics
                 WHERE user_id = $1 AND status_code = 200 ORDER BY total_cost",
            )
            .bind(f.user)
            .fetch_all(&pool)
            .await
            .unwrap();
            if rows.len() == 2 {
                break rows;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("two priced analytics rows must be persisted");
    assert_eq!(
        rows,
        vec![
            ("policy".to_owned(), None, Some("standard".to_owned()), Decimal::new(30, 6)),
            (
                "policy".to_owned(),
                Some("fast".to_owned()),
                Some("fast".to_owned()),
                Decimal::new(60, 6)
            ),
        ]
    );
    f.services.shutdown().await;
}

#[dwctl_test_macros::test]
async fn batch_lines_naming_a_class_route_are_refused(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    activate_policy_classes(&f, &pool).await;
    let upload = |model: &str| {
        let line = json!({"custom_id": "r0", "method": "POST", "url": "/v1/chat/completions",
            "body": {"model": model, "messages": [{"role": "user", "content": "hi"}]}});
        MultipartForm::new()
            .add_part("purpose", Part::text("batch"))
            .add_part("file", Part::bytes(format!("{line}\n").into_bytes()).file_name("batch.jsonl"))
    };
    let response = f
        .server
        .post("/ai/v1/files")
        .add_header("Authorization", format!("Bearer {}", f.key))
        .multipart(upload("policy:fast"))
        .await;
    response.assert_status(StatusCode::BAD_REQUEST);
    assert!(response.text().contains("realtime requests only"), "{}", response.text());
    f.server
        .post("/ai/v1/files")
        .add_header("Authorization", format!("Bearer {}", f.key))
        .multipart(upload("policy"))
        .await
        .assert_status(StatusCode::CREATED);
    f.services.shutdown().await;
}
