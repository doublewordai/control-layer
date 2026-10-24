//! The conversation key forwarded in a header, through the real routing and
//! forwarding path with the real HTTP client: every turn of a conversation
//! carries the same value, different conversations carry different values,
//! the value is a hash rather than anything from the request, a caller cannot
//! choose it, and providers that do not opt in see nothing new.

use axum::{Router, body::Body, extract::Request, http::StatusCode};
use onwards::{AppState, build_router, client::create_hyper_client, target::Targets};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const HEADER: &str = "x-affinity-key";

async fn upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "choices": [],
        })))
        .mount(&server)
        .await;
    server
}

fn router(target: Value) -> Router {
    let config = json!({"targets": {"test-model": target}});
    let targets = Targets::from_config(serde_json::from_value(config).unwrap()).unwrap();
    build_router(AppState::with_client(targets, create_hyper_client(10, 60)))
}

fn opted_in(server: &MockServer) -> Router {
    router(json!({
        "url": format!("{}/v1", server.uri()),
        "affinity_key_header": HEADER,
    }))
}

fn request(headers: &[(&str, &str)], body: Value) -> Request {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

fn turn(conversation: &str, turn: usize) -> Value {
    let mut messages = vec![
        json!({"role": "system", "content": "You are terse."}),
        json!({"role": "user", "content": conversation}),
    ];
    for i in 0..turn {
        messages.push(json!({"role": "assistant", "content": format!("answer {i}")}));
        messages.push(json!({"role": "user", "content": format!("follow-up {i}")}));
    }
    json!({"model": "test-model", "messages": messages})
}

/// The header values the upstream received, in order (`None` where absent).
async fn received_keys(server: &MockServer) -> Vec<Option<String>> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            r.headers
                .get(HEADER)
                .map(|v| v.to_str().unwrap().to_string())
        })
        .collect()
}

async fn send(app: &Router, request: Request) {
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn every_turn_of_a_conversation_carries_the_same_key() {
    let server = upstream().await;
    let app = opted_in(&server);
    for n in 0..4 {
        send(&app, request(&[], turn("conversation a", n))).await;
    }
    send(&app, request(&[], turn("conversation b", 0))).await;

    let keys = received_keys(&server).await;
    let first = keys[0].clone().expect("a keyed request carries the header");
    assert!(
        keys[..4]
            .iter()
            .all(|k| k.as_deref() == Some(first.as_str())),
        "turns of one conversation carried different keys: {keys:?}"
    );
    assert_ne!(
        keys[4].as_deref(),
        Some(first.as_str()),
        "two conversations carried the same key"
    );
}

#[tokio::test]
async fn the_key_is_a_hash_not_request_content() {
    let server = upstream().await;
    let app = opted_in(&server);
    send(
        &app,
        request(&[("x-session-id", "session-123")], turn("hello", 0)),
    )
    .await;
    let mut with_cache_key = turn("hello", 0);
    with_cache_key["prompt_cache_key"] = json!("cache-key-456");
    send(&app, request(&[], with_cache_key)).await;

    for key in received_keys(&server).await {
        let key = key.expect("a keyed request carries the header");
        assert_eq!(key.len(), 16, "not a 64-bit hex hash: {key}");
        assert!(key.bytes().all(|b| b.is_ascii_hexdigit()), "not hex: {key}");
        for leaked in ["session-123", "cache-key-456", "hello", "terse"] {
            assert!(!key.contains(leaked), "header leaked {leaked:?}: {key}");
        }
    }
}

#[tokio::test]
async fn a_caller_cannot_choose_the_key() {
    let server = upstream().await;
    let app = opted_in(&server);
    send(&app, request(&[], turn("conversation a", 0))).await;
    // A spoofed value is replaced by the request's own key...
    send(
        &app,
        request(&[(HEADER, "0000000000000000")], turn("conversation a", 1)),
    )
    .await;
    // ...and removed when the request has no key.
    send(
        &app,
        request(
            &[(HEADER, "0000000000000000")],
            json!({"model": "test-model", "prompt": "no conversation"}),
        ),
    )
    .await;

    let keys = received_keys(&server).await;
    assert_eq!(
        keys[1], keys[0],
        "a spoofed header replaced the request's key"
    );
    assert_eq!(
        keys[2], None,
        "a spoofed header passed through on an unkeyed request"
    );
}

#[tokio::test]
async fn providers_that_do_not_opt_in_see_no_key() {
    let server = upstream().await;
    let app = router(json!({"url": format!("{}/v1", server.uri())}));
    send(&app, request(&[], turn("conversation a", 0))).await;
    assert_eq!(received_keys(&server).await, vec![None]);
}

#[tokio::test]
async fn only_opted_in_providers_of_a_pool_receive_the_key() {
    let preferred = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&preferred)
        .await;
    let fallback = upstream().await;
    let app = router(json!({
        "strategy": "priority",
        "fallback": {"enabled": true, "on_status": [503]},
        "providers": [
            {"url": format!("{}/v1", preferred.uri()), "affinity_key_header": HEADER},
            {"url": format!("{}/v1", fallback.uri())},
        ],
    }));
    send(&app, request(&[], turn("conversation a", 0))).await;

    assert!(received_keys(&preferred).await[0].is_some());
    assert_eq!(received_keys(&fallback).await, vec![None]);
}

#[test]
fn an_invalid_or_reserved_header_fails_config_load() {
    for name in ["not a header", "authorization", "host"] {
        let config = json!({"targets": {"test-model": {
            "url": "http://upstream:8000/v1",
            "affinity_key_header": name,
        }}});
        let result = Targets::from_config(serde_json::from_value(config).unwrap());
        assert!(result.is_err(), "{name:?} was accepted");
    }
}
