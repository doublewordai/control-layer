//! An operator can pin an account to a scheduling-toleration preset: every
//! realtime, flex and batch request from the account then carries exactly the
//! preset's toleration list at `nvext.routing_constraints.tolerations`, the
//! client cannot set its own, and an account on "Default" is unchanged. The
//! tests run the full application so the key-policy map, the middleware write
//! and the batch ingest path are exercised together.
use axum::http::StatusCode;
use axum_test::TestServer;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::api::models::users::Role;
use crate::config::{SchedulingConfig, TolerationPreset};
use crate::scheduling::{SchedulingOperator, SchedulingToleration};
use crate::test::utils::{add_auth_headers, create_test_admin_user, create_test_config, create_test_user_with_roles};
use crate::{Application, BackgroundServices};

/// A config with two presets besides the default: an empty-list preset
/// (`no-overflow`) and a custom one (`gpu`).
fn test_config_with_presets() -> crate::config::Config {
    let mut config = create_test_config();
    config.scheduling = SchedulingConfig {
        toleration_presets: vec![
            SchedulingConfig::default_preset(),
            TolerationPreset {
                id: "gpu".to_string(),
                label: "GPU only".to_string(),
                description: "Pin to GPU capacity only.".to_string(),
                tolerations: vec![SchedulingToleration {
                    key: "pool".to_string(),
                    operator: Some(SchedulingOperator::Equal),
                    value: Some("gpu".to_string()),
                    effect: None,
                }],
            },
        ],
        inject_realtime_priority: false,
    };
    config
}

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
        let mut config = test_config_with_presets();
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
        sqlx::query("INSERT INTO api_keys (name,secret,user_id,created_by,purpose) VALUES ('preset-test',$1,$2,$2,'realtime')")
            .bind(&key)
            .bind(user.id)
            .execute(pool)
            .await
            .unwrap();
        let endpoint: Uuid = sqlx::query_scalar(
            "INSERT INTO inference_endpoints (name,url,created_by,kind) VALUES ('preset-test',$1,$2,'hosted') RETURNING id",
        )
        .bind(upstream.uri())
        .bind(user.id)
        .fetch_one(pool)
        .await
        .unwrap();
        let member: Uuid = sqlx::query_scalar(
            "INSERT INTO deployed_models (model_name,alias,hosted_on,created_by,type) VALUES ('preset-upstream','preset-member',$1,$2,'CHAT') RETURNING id",
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

    /// Post a Responses request carrying a client-supplied toleration and
    /// return the Chat Completions body the upstream actually received. The
    /// account's preset must survive the Responses-to-Chat translation.
    async fn post_responses(&self) -> Value {
        let body = json!({
            "model": "plain",
            "input": "hi",
            "max_output_tokens": 16,
            "nvext": {"routing_constraints": {"tolerations": [{"operator": "Exists"}]}}
        });
        self.server
            .post("/ai/v1/responses")
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
                "custom_id": "preset-batch",
                "method": "POST",
                "url": "/v1/chat/completions",
                "body": {
                    "model": "plain",
                    "messages": [{"role": "user", "content": "hi"}],
                    "nvext": {"routing_constraints": {"tolerations": [{"operator": "Exists"}]}}
                }
            })
        );
        let file = axum_test::multipart::Part::bytes(jsonl.into_bytes()).file_name("preset.jsonl");
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
    async fn set_preset(&self, id: Option<&str>) {
        match id {
            Some(id) => {
                sqlx::query("UPDATE users SET toleration_preset = $2 WHERE id = $1")
                    .bind(self.user)
                    .bind(id)
                    .execute(&self.pool)
                    .await
                    .unwrap();
            }
            None => {
                sqlx::query("UPDATE users SET toleration_preset = NULL WHERE id = $1")
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
async fn a_preset_account_carries_its_tolerations_and_the_client_cannot_override(pool: PgPool) {
    let f = Fixture::new(&pool).await;

    // Default: the client's toleration is stripped and nothing replaces it.
    let body = f.post_realtime().await;
    assert!(
        body["nvext"]["routing_constraints"]["tolerations"].is_null(),
        "a Default account carries no tolerations: {body}"
    );

    // Pinned to the empty-list preset: the account now forbids any tainted
    // capacity.
    f.set_preset(Some("no-overflow")).await;
    let body = f.post_realtime().await;
    assert_eq!(
        body["nvext"]["routing_constraints"]["tolerations"],
        json!([]),
        "the empty-list preset is written verbatim: {body}"
    );

    // Pinned to a custom preset: it replaces the stripped client value with the
    // preset's list verbatim (including the operator the operator configured).
    f.set_preset(Some("gpu")).await;
    let body = f.post_realtime().await;
    assert_eq!(
        body["nvext"]["routing_constraints"]["tolerations"],
        json!([{"key": "pool", "operator": "Equal", "value": "gpu"}]),
        "the preset list overrides the client's: {body}"
    );

    // Clearing the preset returns the account to the default (nothing carried).
    f.set_preset(None).await;
    let body = f.post_realtime().await;
    assert!(
        body["nvext"]["routing_constraints"]["tolerations"].is_null(),
        "clearing the preset removes the tolerations: {body}"
    );
}

/// An account pinned to a preset id that is not in config fails closed: the
/// request path applies the empty (most restrictive) list rather than leaving
/// the request unpinned.
#[dwctl_test_macros::test]
async fn an_unconfigured_preset_at_runtime_fails_closed_to_the_empty_list(pool: PgPool) {
    let f = Fixture::new(&pool).await;

    // Write the column directly, as a config edit that dropped a preset would
    // leave an account referencing an id.
    sqlx::query("UPDATE users SET toleration_preset = 'retired' WHERE id = $1")
        .bind(f.user)
        .execute(&pool)
        .await
        .unwrap();
    f.services.sync_key_policy(&pool).await.unwrap();

    let body = f.post_realtime().await;
    assert_eq!(
        body["nvext"]["routing_constraints"]["tolerations"],
        json!([]),
        "an unknown preset id fails closed to the empty list: {body}"
    );
}

/// A pinned `/responses` request keeps its tolerations through the Responses-to-
/// Chat translation: the translator sits inner to the middleware that stamps
/// them, so the upstream must receive the preset's list, not lose it.
#[dwctl_test_macros::test]
async fn a_preset_account_carries_its_tolerations_on_responses_requests(pool: PgPool) {
    let f = Fixture::new(&pool).await;

    f.set_preset(Some("gpu")).await;
    let body = f.post_responses().await;
    assert_eq!(
        body["nvext"]["routing_constraints"]["tolerations"],
        json!([{"key": "pool", "operator": "Equal", "value": "gpu"}]),
        "the preset survives Responses translation: {body}"
    );
}

/// The batch path affixes the preset at ingest, so the stored template the
/// daemon later dispatches already carries the preset's list, not the client's.
#[dwctl_test_macros::test]
async fn a_preset_account_stamps_its_tolerations_on_uploaded_batch_requests(pool: PgPool) {
    let f = Fixture::new(&pool).await;

    // Default: the client's batch toleration is stripped at ingest.
    let body = f.upload_batch().await;
    assert!(
        body["nvext"]["routing_constraints"]["tolerations"].is_null(),
        "a Default upload carries no tolerations: {body}"
    );

    // Pinned: the stored template carries exactly the preset's list.
    f.set_preset(Some("gpu")).await;
    let body = f.upload_batch().await;
    assert_eq!(
        body["nvext"]["routing_constraints"]["tolerations"],
        json!([{"key": "pool", "operator": "Equal", "value": "gpu"}]),
        "the preset is stamped on the stored batch body: {body}"
    );
    // Other fields the client sent still pass through.
    assert_eq!(body["model"], "plain");
}

/// A platform manager sets the preset through the admin API; a non-manager
/// cannot, an unknown id is refused, and the preset is only visible back to a
/// platform manager.
#[dwctl_test_macros::test]
async fn a_platform_manager_sets_the_preset_through_the_admin_api(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let headers = add_auth_headers(&create_test_admin_user(&pool, Role::PlatformManager).await);
    let member = create_test_user_with_roles(&pool, vec![Role::StandardUser]).await;
    let member_headers = add_auth_headers(&member);

    // Setting the preset; the GET must show it back to the manager.
    let response = f
        .server
        .patch(&format!("/admin/api/v1/users/{}", f.user))
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&json!({"toleration_preset": "no-overflow"}))
        .await;
    response.assert_status_ok();
    let updated = response.json::<Value>();
    assert_eq!(updated["toleration_preset"], json!("no-overflow"), "{updated}");

    // The preset reaches the request path on the next sync (the API write fires
    // the notify trigger; refresh the cache the way the LISTEN/NOTIFY loop
    // would, as `set_preset` does).
    f.services.sync_key_policy(&pool).await.unwrap();
    let body = f.post_realtime().await;
    assert_eq!(body["nvext"]["routing_constraints"]["tolerations"], json!([]), "{body}");

    // An unknown id is refused with a 400.
    f.server
        .patch(&format!("/admin/api/v1/users/{}", f.user))
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&json!({"toleration_preset": "does-not-exist"}))
        .await
        .assert_status(StatusCode::BAD_REQUEST);

    // A plain member's GET omits the operator's routing decision entirely.
    let as_member_response = f
        .server
        .get(&format!("/admin/api/v1/users/{}", member.id))
        .add_header(&member_headers[0].0, &member_headers[0].1)
        .add_header(&member_headers[1].0, &member_headers[1].1)
        .await;
    as_member_response.assert_status_ok();
    let as_member = as_member_response.json::<Value>();
    assert!(as_member.get("toleration_preset").is_none(), "{as_member}");

    // A plain member cannot set a preset on their own account.
    f.server
        .patch(&format!("/admin/api/v1/users/{}", member.id))
        .add_header(&member_headers[0].0, &member_headers[0].1)
        .add_header(&member_headers[1].0, &member_headers[1].1)
        .json(&json!({"toleration_preset": "no-overflow"}))
        .await
        .assert_status(StatusCode::FORBIDDEN);

    // Clearing the preset: `null` maps to no preset.
    let cleared_response = f
        .server
        .patch(&format!("/admin/api/v1/users/{}", f.user))
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .json(&json!({"toleration_preset": null}))
        .await;
    cleared_response.assert_status_ok();
    let cleared = cleared_response.json::<Value>();
    assert!(cleared["toleration_preset"].is_null(), "{cleared}");
    f.services.sync_key_policy(&pool).await.unwrap();
    let body = f.post_realtime().await;
    assert!(body["nvext"]["routing_constraints"]["tolerations"].is_null(), "{body}");
}

/// The presets are exposed on `GET /config` so the dashboard can list them.
#[dwctl_test_macros::test]
async fn config_exposes_the_scheduling_toleration_presets(pool: PgPool) {
    let f = Fixture::new(&pool).await;
    let user = create_test_user_with_roles(&pool, vec![Role::StandardUser]).await;
    let headers = add_auth_headers(&user);
    let response = f
        .server
        .get("/admin/api/v1/config")
        .add_header(&headers[0].0, &headers[0].1)
        .add_header(&headers[1].0, &headers[1].1)
        .await;
    response.assert_status_ok();
    let json: Value = response.json();
    let presets = json["scheduling_toleration_presets"].as_array().unwrap();
    assert!(presets.iter().any(|preset| preset["id"] == "no-overflow"), "{json}");
    assert!(
        presets
            .iter()
            .all(|preset| preset["label"].is_string() && preset["description"].is_string()),
        "{json}"
    );
}
