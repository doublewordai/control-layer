//! Replica affinity through the real routing and forwarding path, with the
//! real HTTP client: a conversation stays on one replica, a replica that
//! refuses connections hands its conversations to the next-ranked replica, and
//! requests without a conversation key use the provider's own URL.

use axum::{Router, body::Body, extract::Request, http::StatusCode};
use onwards::{AppState, build_router, client::create_hyper_client, target::Targets};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

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

/// A local address with nothing listening, so connecting to it is refused.
fn refused_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("http://127.0.0.1:{port}/v1")
}

fn router(service: &str, replicas: &[String]) -> Router {
    router_with(service, json!({"urls": replicas}))
}

fn router_with(service: &str, replicas: Value) -> Router {
    let config = json!({"targets": {"test-model": {
        "url": service,
        "replicas": replicas,
    }}});
    let targets = Targets::from_config(serde_json::from_value(config).unwrap()).unwrap();
    build_router(AppState::with_client(targets, create_hyper_client(10, 60)))
}

/// An upstream that turns every request away with `status`, as a draining
/// server does.
async fn refusing(status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(status))
        .mount(&server)
        .await;
    server
}

fn request(session: Option<&str>, body: Value) -> Request {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json");
    if let Some(session) = session {
        builder = builder.header("x-session-id", session);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

fn chat(content: &str) -> Value {
    json!({"model": "test-model", "messages": [{"role": "user", "content": content}]})
}

async fn received(server: &MockServer) -> usize {
    server.received_requests().await.unwrap_or_default().len()
}

#[tokio::test]
async fn each_conversation_stays_on_one_replica() {
    let service = upstream().await;
    let replicas = [upstream().await, upstream().await, upstream().await];
    let urls: Vec<String> = replicas.iter().map(|r| format!("{}/v1", r.uri())).collect();
    let app = router(&format!("{}/v1", service.uri()), &urls);

    let mut spread: u32 = 0;
    for conversation in 0..12 {
        let before: Vec<usize> =
            futures_util::future::join_all(replicas.iter().map(received)).await;
        for turn in 0..4 {
            let response = app
                .clone()
                .oneshot(request(
                    None,
                    json!({
                        "model": "test-model",
                        "messages": [
                            {"role": "system", "content": "You are terse."},
                            {"role": "user", "content": format!("conversation {conversation}")},
                            {"role": "assistant", "content": format!("turn {turn}")},
                        ],
                    }),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        let after: Vec<usize> = futures_util::future::join_all(replicas.iter().map(received)).await;
        let gained: Vec<usize> = after.iter().zip(&before).map(|(a, b)| a - b).collect();
        assert!(
            gained.contains(&4),
            "turns split across replicas: {gained:?}"
        );
        spread |= 1 << gained.iter().position(|&g| g == 4).unwrap();
    }
    assert_eq!(
        received(&service).await,
        0,
        "keyed requests reached the shared URL"
    );
    assert!(
        spread.count_ones() > 1,
        "every conversation landed on the same replica"
    );
}

#[tokio::test]
async fn a_refusing_replica_hands_its_conversations_to_the_next_one() {
    let service = upstream().await;
    let live = upstream().await;
    let urls = vec![refused_url(), format!("{}/v1", live.uri())];
    let app = router(&format!("{}/v1", service.uri()), &urls);

    for session in 0..20 {
        let response = app
            .clone()
            .oneshot(request(Some(&format!("session-{session}")), chat("hi")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    // About half the sessions rank the refusing replica first; they all move
    // to the live replica rather than the shared URL.
    assert_eq!(received(&live).await, 20);
    assert_eq!(received(&service).await, 0);
}

#[tokio::test]
async fn the_provider_url_is_the_last_resort_and_serves_unkeyed_requests() {
    let service = upstream().await;
    let app = router(&format!("{}/v1", service.uri()), &[refused_url()]);

    let keyed = app
        .clone()
        .oneshot(request(Some("session"), chat("hi")))
        .await
        .unwrap();
    assert_eq!(keyed.status(), StatusCode::OK);
    let unkeyed = app
        .clone()
        .oneshot(request(None, json!({"model": "test-model"})))
        .await
        .unwrap();
    assert_eq!(unkeyed.status(), StatusCode::OK);
    assert_eq!(received(&service).await, 2);
}

#[tokio::test]
async fn a_listed_status_moves_the_request_to_the_next_replica() {
    let service = upstream().await;
    let draining = refusing(503).await;
    let live = upstream().await;
    let urls = vec![
        format!("{}/v1", draining.uri()),
        format!("{}/v1", live.uri()),
    ];
    let app = router_with(
        &format!("{}/v1", service.uri()),
        json!({"urls": urls, "failover_on_status": [503]}),
    );

    for session in 0..20 {
        let response = app
            .clone()
            .oneshot(request(Some(&format!("session-{session}")), chat("hi")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    assert_eq!(received(&live).await, 20);
    assert_eq!(received(&service).await, 0);
    assert!(
        received(&draining).await > 0,
        "no session ranked the draining replica first"
    );
}

#[tokio::test]
async fn an_unlisted_status_is_not_retried_elsewhere() {
    let service = upstream().await;
    let draining = refusing(503).await;
    let app = router(
        &format!("{}/v1", service.uri()),
        &[format!("{}/v1", draining.uri())],
    );

    let response = app
        .clone()
        .oneshot(request(Some("session"), chat("hi")))
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::OK);
    assert_eq!(received(&draining).await, 1);
    assert_eq!(received(&service).await, 0);
}

#[test]
fn a_replica_on_another_path_is_rejected_at_load() {
    let config = json!({"targets": {"test-model": {
        "url": "http://service.test.invalid:8000/v1",
        "replicas": {"urls": ["http://replica.test.invalid:8000/v2"]},
    }}});
    assert!(Targets::from_config(serde_json::from_value(config).unwrap()).is_err());
}
