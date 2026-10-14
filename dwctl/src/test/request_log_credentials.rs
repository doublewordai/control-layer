//! End-to-end check that credentials never reach the outlet request log.
//!
//! Drives a real `/ai/v1` request through the full application with
//! `enable_request_logging` on, then reads back the `http_requests` and
//! `http_responses` rows outlet wrote. The API key the request authenticated
//! with must appear nowhere in either row: not in the `headers` column, and not
//! in the copy of the headers that the request serializer embeds in `body`.
//!
//! The unit tests in `request_logging::credential_scrubber` prove the wrapper
//! strips the right names; this proves the wrapper is actually in the chain.

use crate::api::models::users::Role;
use crate::test::utils::{
    add_deployment_to_group, add_user_to_group, create_test_api_key_for_user, create_test_app_with_config, create_test_config,
    create_test_endpoint, create_test_model, create_test_user,
};
use sqlx::PgPool;

const MODEL_ALIAS: &str = "request-log-credentials";
/// Sent alongside the real key so the test covers a header dwctl itself does not read.
const COOKIE_VALUE: &str = "session=request-log-cookie-sentinel-4c1e";
const SET_COOKIE_VALUE: &str = "upstream=request-log-set-cookie-sentinel-8a2f";

/// Poll for the most-recent row of `table` as JSON, until the async outlet writer flushes it.
async fn poll_latest_row(pool: &PgPool, table: &str) -> serde_json::Value {
    let query = format!("SELECT to_jsonb(t)::text FROM {table} t ORDER BY t.correlation_id DESC LIMIT 1");
    for _ in 0..200 {
        let row: Option<(String,)> = sqlx::query_as(&query).fetch_optional(pool).await.expect("query request log");
        if let Some((json,)) = row {
            return serde_json::from_str(&json).expect("row is valid json");
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("no row written to {table}");
}

#[dwctl_test_macros::test]
async fn api_key_and_cookies_are_not_stored_in_the_request_log(pool: PgPool) {
    let user = create_test_user(&pool, Role::StandardUser).await;
    add_user_to_group(&pool, user.id, uuid::Uuid::nil()).await;
    let api_key = create_test_api_key_for_user(&pool, user.id).await;

    let mock_server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("set-cookie", SET_COOKIE_VALUE)
                .set_body_json(serde_json::json!({
                    "id": "chatcmpl-request-log-credentials",
                    "object": "chat.completion",
                    "created": 1_677_652_288,
                    "model": "upstream-model",
                    "choices": [{
                        "index": 0,
                        "message": { "role": "assistant", "content": "ok" },
                        "finish_reason": "stop"
                    }],
                    "usage": { "prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8 }
                })),
        )
        .mount(&mock_server)
        .await;

    let endpoint_id = create_test_endpoint(&pool, "request-log-credentials-endpoint", user.id).await;
    sqlx::query("UPDATE inference_endpoints SET url = $1 WHERE id = $2")
        .bind(format!("{}/v1", mock_server.uri()))
        .bind(endpoint_id)
        .execute(&pool)
        .await
        .expect("update endpoint url");
    let deployment_id = create_test_model(&pool, "request-log-credentials-model", MODEL_ALIAS, endpoint_id, user.id).await;
    add_deployment_to_group(&pool, deployment_id, uuid::Uuid::nil(), user.id).await;

    let mut config = create_test_config();
    config.enable_request_logging = true;
    config.background_services.onwards_sync.enabled = true;
    let (server, _bg_services) = create_test_app_with_config(pool.clone(), config, false).await;

    // Onwards picks the model up asynchronously; 403/404 mean "not synced yet".
    let body = serde_json::json!({
        "model": MODEL_ALIAS,
        "messages": [{ "role": "user", "content": "hello" }]
    });
    for attempt in 0..100 {
        let resp = server
            .post("/ai/v1/chat/completions")
            .add_header("authorization", format!("Bearer {}", api_key.secret))
            .add_header("x-api-key", &api_key.secret)
            .add_header("cookie", COOKIE_VALUE)
            .json(&body)
            .await;
        let status = resp.status_code().as_u16();
        if status == 200 {
            break;
        }
        assert!(
            matches!(status, 403 | 404),
            "request should succeed; got {status}, body: {}",
            resp.text()
        );
        assert!(attempt < 99, "model never became routable, last status: {status}");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let request_row = poll_latest_row(&pool, "outlet.http_requests").await;
    let request_text = request_row.to_string();
    assert!(
        !request_text.contains(&api_key.secret),
        "api key persisted to http_requests: {request_text}"
    );
    assert!(
        !request_text.contains(COOKIE_VALUE),
        "cookie persisted to http_requests: {request_text}"
    );
    for name in ["authorization", "x-api-key", "cookie"] {
        assert!(
            request_row["headers"].get(name).is_none(),
            "{name} present in http_requests.headers: {request_text}"
        );
        assert!(
            request_row["body"]["headers"].get(name).is_none(),
            "{name} present in http_requests.body.headers: {request_text}"
        );
    }
    // The row is otherwise intact: non-credential headers and the body survive.
    assert_eq!(
        request_row["headers"]["content-type"].as_str(),
        Some("application/json"),
        "content-type dropped: {request_text}"
    );
    assert_eq!(
        request_row["body"]["request"]["model"].as_str(),
        Some(MODEL_ALIAS),
        "{request_text}"
    );

    let response_row = poll_latest_row(&pool, "outlet.http_responses").await;
    let response_text = response_row.to_string();
    assert!(
        response_row["headers"].get("set-cookie").is_none(),
        "set-cookie present in http_responses.headers: {response_text}"
    );
    assert!(
        !response_text.contains(SET_COOKIE_VALUE),
        "set-cookie value persisted to http_responses: {response_text}"
    );
    assert_eq!(response_row["status_code"].as_i64(), Some(200), "{response_text}");
}
