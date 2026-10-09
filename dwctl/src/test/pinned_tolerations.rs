//! An operator can pin an account's scheduling tolerations: every realtime,
//! flex and batch request from the account then carries exactly the pinned
//! list at `nvext.routing_constraints.tolerations`, the client cannot set its
//! own, and an unpinned account is unchanged. The tests run the full
//! application so the key-policy map, the middleware write and the batch
//! ingest path are exercised together.
use axum::http::StatusCode;
use axum_test::TestServer;
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
    upstream: MockServer,
    services: BackgroundServices,
    user: Uuid,
    key: String,
    pool: PgPool,
}

impl Fixture {
    async fn new(pool: &PgPool) -> Self {
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
        sqlx::query("INSERT INTO api_keys (name,secret,user_id,created_by,purpose) VALUES ('pin-test',$1,$2,$2,'realtime')")
            .bind(&key)
            .bind(user.id)
            .execute(pool)
            .await
            .unwrap();
        let endpoint: Uuid = sqlx::query_scalar(
            "INSERT INTO inference_endpoints (name,url,created_by,kind) VALUES ('pin-test',$1,$2,'dynamo') RETURNING id",
        )
        .bind(upstream.uri())
        .bind(user.id)
        .fetch_one(pool)
        .await
        .unwrap();
        let member: Uuid = sqlx::query_scalar(
            "INSERT INTO deployed_models (model_name,alias,hosted_on,created_by,type) VALUES ('pin-upstream','pin-member',$1,$2,'CHAT') RETURNING id",
        )
        .bind(endpoint)
        .bind(user.id)
        .fetch_one(pool)
        .await
        .unwrap();
        let model: Uuid = sqlx::query_scalar(
            "INSERT INTO deployed_models (model_name,alias,created_by,type,is_composite) VALUES ('plain','plain',$1,'CHAT',true) RETURNING id",
        )
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
            .bind(model).bind(rust_decimal::Decimal::new(1,6))
            .execute(pool).await.unwrap();
        services.sync_onwards_config(pool).await.unwrap();
        Self {
            server,
            upstream,
            services,
            user: user.id,
            key,
            pool: pool.clone(),
        }
    }

    /// Post a chat completion carrying a client-supplied toleration (which must
    /// never survive) and return the body the upstream actually received.
    async fn post_realtime(&self) -> Value {
        let body = json!({
            "model": "plain",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 16,
            "nvext": {"routing_constraints": {"tolerations": [{"operator": "Exists"}]}}
        });
        self.server
            .post("/ai/v1/chat/completions")
            .add_header("Authorization", format!("Bearer {}", self.key))
            .json(&body)
            .await
            .assert_status_ok();
        self.upstream
            .received_requests()
            .await
            .unwrap()
            .last()
            .unwrap()
            .body_json()
            .unwrap()
    }

    /// Upload a one-line batch file for this account and return the stored
    /// template body, which is what the daemon later dispatches upstream.
    async fn upload_batch(&self) -> Value {
        let jsonl = format!(
            "{}\n",
            json!({
                "custom_id": "pin-batch",
                "method": "POST",
                "url": "/v1/chat/completions",
                "body": {
                    "model": "plain",
                    "messages": [{"role": "user", "content": "hi"}],
                    "nvext": {"routing_constraints": {"tolerations": [{"operator": "Exists"}]}}
                }
            })
        );
        let file = axum_test::multipart::Part::bytes(jsonl.into_bytes()).file_name("pin.jsonl");
        let form = axum_test::multipart::MultipartForm::new()
            .add_part("purpose", axum_test::multipart::Part::text("batch"))
            .add_part("file", file);
        self.server
            .post("/ai/v1/files")
            .add_header("Authorization", format!("Bearer {}", self.key))
            .multipart(form)
            .await
            .assert_status(StatusCode::CREATED);
        sqlx::query_scalar::<_, String>("SELECT body FROM fusillade.request_templates_all ORDER BY created_at DESC LIMIT 1")
            .fetch_one(&self.pool)
            .await
            .unwrap()
            .parse()
            .unwrap()
    }

    /// Pin the account by writing the column the operator's catalog writes and
    /// refresh the per-key policy map as the LISTEN/NOTIFY loop would.
    async fn set_pin(&self, value: Option<Value>) {
        match value {
            Some(value) => {
                sqlx::query("UPDATE users SET pinned_tolerations = $2 WHERE id = $1")
                    .bind(self.user)
                    .bind(value)
                    .execute(&self.pool)
                    .await
                    .unwrap();
            }
            None => {
                sqlx::query("UPDATE users SET pinned_tolerations = NULL WHERE id = $1")
                    .bind(self.user)
                    .execute(&self.pool)
                    .await
                    .unwrap();
            }
        }
        // The middleware reads the per-key policy map; refresh it as the
        // LISTEN/NOTIFY loop would (the test app runs with sync disabled).
        self.services.sync_key_policy(&self.pool).await.unwrap();
    }
}

#[dwctl_test_macros::test]
async fn a_pinned_account_carries_its_tolerations_and_the_client_cannot_override(pool: PgPool) {
    let f = Fixture::new(&pool).await;

    // Unpinned: the client's toleration is stripped and nothing replaces it.
    let body = f.post_realtime().await;
    assert!(
        body["nvext"]["routing_constraints"]["tolerations"].is_null(),
        "an unpinned account carries no tolerations: {body}"
    );

    // Pinned to `[]`: the account now forbids any tainted capacity.
    f.set_pin(Some(json!([]))).await;
    let body = f.post_realtime().await;
    assert_eq!(
        body["nvext"]["routing_constraints"]["tolerations"],
        json!([]),
        "a pinned empty list is written verbatim: {body}"
    );

    // Pinned to a real list: it replaces the stripped client value exactly.
    f.set_pin(Some(json!([{"key": "dedicated", "value": "only"}]))).await;
    let body = f.post_realtime().await;
    assert_eq!(
        body["nvext"]["routing_constraints"]["tolerations"],
        json!([{"key": "dedicated", "value": "only"}]),
        "the pinned list overrides the client's: {body}"
    );

    // Clearing the pin returns the account to the default (nothing carried).
    f.set_pin(None).await;
    let body = f.post_realtime().await;
    assert!(
        body["nvext"]["routing_constraints"]["tolerations"].is_null(),
        "clearing the pin removes the tolerations: {body}"
    );
}

/// The batch path affixes the pin at ingest, so the stored template the daemon
/// later dispatches already carries the pinned list and not the client's.
#[dwctl_test_macros::test]
async fn a_pinned_account_stamps_its_tolerations_on_uploaded_batch_requests(pool: PgPool) {
    let f = Fixture::new(&pool).await;

    // Unpinned: the client's batch toleration is stripped at ingest.
    let body = f.upload_batch().await;
    assert!(
        body["nvext"]["routing_constraints"]["tolerations"].is_null(),
        "an unpinned upload carries no tolerations: {body}"
    );

    // Pinned: the stored template carries exactly the pinned list.
    f.set_pin(Some(json!([{"key": "dedicated", "value": "only", "effect": "NoSchedule"}])))
        .await;
    let body = f.upload_batch().await;
    assert_eq!(
        body["nvext"]["routing_constraints"]["tolerations"],
        json!([{"key": "dedicated", "value": "only", "effect": "NoSchedule"}]),
        "the pin is stamped on the stored batch body: {body}"
    );
    // Other nvext the client sent still passes through.
    assert_eq!(body["model"], "plain");
}
