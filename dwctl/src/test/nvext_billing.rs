//! Every request sent to a `dynamo` endpoint carries the per-token prices it is
//! charged at as `nvext.billing`; a request to any other endpoint carries none,
//! and a client cannot state its own. The tests run the full application so the
//! key-policy map, the price book, the outbound layer and onwards are exercised
//! together, on the realtime path and on the path the batch daemon dispatches.
use axum::http::StatusCode;
use axum_test::TestServer;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::api::models::users::Role;
use crate::test::utils::{add_auth_headers, create_test_admin_user, create_test_config, create_test_user_with_roles};
use crate::{Application, BackgroundServices};

struct Fixture {
    server: TestServer,
    dynamo: MockServer,
    external: MockServer,
    _services: BackgroundServices,
    realtime_key: String,
    batch_key: String,
}

async fn upstream() -> MockServer {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = request.body_json().unwrap();
            ResponseTemplate::new(200).set_body_json(json!({
                "id":"chatcmpl-test","created":1,"model":body["model"],
                "object":"chat.completion",
                "choices":[{"index":0,"message":{"role":"assistant","content":"OK"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}
            }))
        })
        .mount(&upstream)
        .await;
    upstream
}

/// Insert a price for `model`, valid from a minute ago by the host clock.
async fn price(pool: &PgPool, model: Uuid, purpose: &str, window: Option<&str>, input: Decimal, output: Decimal) {
    sqlx::query(
        "INSERT INTO model_tariffs (deployed_model_id,name,input_price_per_token,output_price_per_token,api_key_purpose,completion_window,valid_from) VALUES ($1,'test',$2,$3,$4,$5,$6)",
    )
    .bind(model)
    .bind(input)
    .bind(output)
    .bind(purpose)
    .bind(window)
    .bind(chrono::Utc::now() - chrono::Duration::minutes(1))
    .execute(pool)
    .await
    .unwrap();
}

impl Fixture {
    async fn new(pool: &PgPool) -> Self {
        let dynamo = upstream().await;
        let external = upstream().await;
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

        let mut keys = Vec::new();
        for purpose in ["realtime", "batch"] {
            let key = format!("sk-{}", Uuid::new_v4());
            sqlx::query("INSERT INTO api_keys (name,secret,user_id,created_by,purpose) VALUES ($3,$1,$2,$2,$3)")
                .bind(&key)
                .bind(user.id)
                .bind(purpose)
                .execute(pool)
                .await
                .unwrap();
            keys.push(key);
        }

        for (kind, upstream, alias) in [("dynamo", &dynamo, "on-dynamo"), ("external", &external, "on-external")] {
            let endpoint: Uuid =
                sqlx::query_scalar("INSERT INTO inference_endpoints (name,url,created_by,kind) VALUES ($1,$2,$3,$4) RETURNING id")
                    .bind(alias)
                    .bind(upstream.uri())
                    .bind(user.id)
                    .bind(kind)
                    .fetch_one(pool)
                    .await
                    .unwrap();
            let model: Uuid = sqlx::query_scalar(
                "INSERT INTO deployed_models (model_name,alias,hosted_on,created_by,type) VALUES ($1,$1,$2,$3,'CHAT') RETURNING id",
            )
            .bind(alias)
            .bind(endpoint)
            .bind(user.id)
            .fetch_one(pool)
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
            price(pool, model, "realtime", None, Decimal::new(1, 6), Decimal::new(3, 6)).await;
            price(pool, model, "batch", Some("24h"), Decimal::new(5, 7), Decimal::new(15, 7)).await;
        }
        services.sync_onwards_config(pool).await.unwrap();
        services.sync_key_policy(pool).await.unwrap();

        let batch_key = keys.pop().unwrap();
        let realtime_key = keys.pop().unwrap();
        Self {
            server,
            dynamo,
            external,
            _services: services,
            realtime_key,
            batch_key,
        }
    }

    /// Post a chat completion for `model`, stating its own prices when
    /// `forged`, and return the body the upstream received.
    async fn post(&self, upstream: &MockServer, model: &str, key: &str, headers: &[(&str, String)], forged: bool) -> Value {
        let mut body = json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 16,
        });
        if forged {
            body["nvext"] = json!({"billing": {"prompt": 0, "cached": 0, "output": 0}});
        }
        let mut request = self
            .server
            .post("/ai/v1/chat/completions")
            .add_header("Authorization", format!("Bearer {key}"))
            .json(&body);
        for (name, value) in headers {
            request = request.add_header(*name, value.as_str());
        }
        request.await.assert_status_ok();
        upstream.received_requests().await.unwrap().last().unwrap().body_json().unwrap()
    }
}

#[dwctl_test_macros::test]
async fn realtime_requests_to_dynamo_carry_their_realtime_prices(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    for forged in [false, true] {
        let body = f.post(&f.dynamo, "on-dynamo", &f.realtime_key, &[], forged).await;
        assert_eq!(
            body["nvext"]["billing"],
            json!({"prompt": 1e-6, "cached": 1e-6, "output": 3e-6}),
            "the realtime price, whatever the client sent: {body}"
        );
    }
}

/// The batch daemon dispatches through the same stack, on the batch-purpose
/// key, with the batch's SLA and creation time as headers.
#[dwctl_test_macros::test]
async fn dispatched_batch_requests_to_dynamo_carry_their_batch_prices(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let headers = [
        ("x-fusillade-request-id", Uuid::new_v4().to_string()),
        ("x-fusillade-batch-id", Uuid::new_v4().to_string()),
        ("x-fusillade-batch-completion-window", "24h".to_string()),
        ("x-fusillade-batch-created-at", chrono::Utc::now().to_rfc3339()),
    ];
    let body = f.post(&f.dynamo, "on-dynamo", &f.batch_key, &headers, true).await;
    assert_eq!(
        body["nvext"]["billing"],
        json!({"prompt": 5e-7, "cached": 5e-7, "output": 1.5e-6}),
        "the batch price for the SLA: {body}"
    );
}

#[dwctl_test_macros::test]
async fn requests_to_other_endpoints_carry_no_prices(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let body = f.post(&f.external, "on-external", &f.realtime_key, &[], false).await;
    assert!(body.get("nvext").is_none(), "no nvext reaches a third party: {body}");
    let body = f.post(&f.external, "on-external", &f.realtime_key, &[], true).await;
    assert!(body["nvext"].get("billing").is_none(), "nor does a client's own price: {body}");
}
