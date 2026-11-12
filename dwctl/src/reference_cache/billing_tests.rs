//! Assured billing through the cache layer, and the translator stability it depends on.
//!
//! The first half drives the real stack the gateway runs (protocol translation outside
//! the cache layer, the cache layer with the reference cache inside, a stand-in
//! upstream) and checks what the customer is told and what analytics is handed. The
//! second half checks, without a database, that each translator renders turn N+1 so
//! that turn N's prefix hashes are a prefix of it: the property the reference cache
//! needs to match a Messages or Responses conversation at all.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Extensions, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use bytes::Bytes;
use serde_json::{Value, json};
use sqlx::PgPool;

use super::tests::{Fixture, fixture_with};
use super::*;
use crate::inference::response_store::{ResponseContext, ResponseStore, StoreError};
use crate::inference::translation::anthropic::AnthropicMessages;
use crate::inference::translation::middleware::translation_middleware;
use crate::inference::translation::responses::OpenResponses;
use crate::inference::translation::responses::hydrate::hydrate_previous_response;
use crate::inference::translation::{ProtocolTranslator, TranslationRegistry};
use crate::prompt_cache::{
    CacheBilling, CacheLayerState, Classifier, ModelConfigResolver, PostgresIndex, PrincipalResolver, UpstreamCachedTokens,
    cache_middleware,
};
use crate::test::utils::{create_test_endpoint, create_test_model};

const MODEL: &str = "acme/chat-model";
const SYSTEM: &str = "You are a careful assistant for a fictional hardware store.";
const USER_1: &str = "Which of the listed drills is the lightest?";
const ASSISTANT_1: &str = "The compact model, at 1.1 kg.";
const USER_2: &str = "And the cheapest?";

fn policies() -> (TierPolicy, TelemetryPolicy) {
    (
        TierPolicy::from_config(&["5m".into(), "1h".into(), "24h".into()], "5m"),
        TelemetryPolicy::from_config(false, &[]),
    )
}

fn hashes(chat_body: &[u8]) -> Vec<Vec<u8>> {
    let (tier, telemetry) = policies();
    boundaries(chat_body, &tier, &telemetry).unwrap().hashes
}

// ===== Through the cache layer =====

/// What the stand-in upstream answers next.
#[derive(Clone, Copy)]
struct Reply {
    prompt_tokens: u64,
    engine_cached: u64,
}

#[derive(Clone)]
struct Upstream(Arc<Mutex<Reply>>);

impl Upstream {
    fn set(&self, prompt_tokens: u64, engine_cached: u64) {
        *self.0.lock().unwrap() = Reply {
            prompt_tokens,
            engine_cached,
        };
    }
}

/// A chat completion, streamed when the (translated) request asks for it, with the
/// configured usage.
async fn upstream(State(up): State<Upstream>, req: Request) -> Response {
    assert_eq!(req.uri().path(), "/v1/chat/completions", "translated before it gets here");
    let body: Value = serde_json::from_slice(&axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap()).unwrap();
    let Reply {
        prompt_tokens,
        engine_cached,
    } = *up.0.lock().unwrap();
    let usage = json!({
        "prompt_tokens": prompt_tokens, "completion_tokens": 2, "total_tokens": prompt_tokens + 2,
        "prompt_tokens_details": {"cached_tokens": engine_cached}
    });
    if body["stream"] == json!(true) {
        let frames = [
            json!({"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": MODEL,
                   "choices": [{"index": 0, "delta": {"role": "assistant", "content": "ok"}, "finish_reason": null}]}),
            json!({"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": MODEL,
                   "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
            json!({"id": "c1", "object": "chat.completion.chunk", "created": 1, "model": MODEL, "choices": [], "usage": usage}),
        ];
        let sse: String = frames.iter().map(|f| format!("data: {f}\n\n")).collect::<String>() + "data: [DONE]\n\n";
        ([(header::CONTENT_TYPE, "text/event-stream")], Body::from(sse)).into_response()
    } else {
        axum::Json(json!({
            "id": "c1", "object": "chat.completion", "created": 1, "model": MODEL,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": usage,
        }))
        .into_response()
    }
}

type Observed = Arc<Mutex<Option<Extensions>>>;

async fn observe(State(slot): State<Observed>, req: Request, next: Next) -> Response {
    let response = next.run(req).await;
    *slot.lock().unwrap() = Some(response.extensions().clone());
    response
}

struct Stack {
    f: Fixture,
    server: axum_test::TestServer,
    upstream: Upstream,
    observed: Observed,
}

/// What one request was billed, as analytics reads it.
#[derive(Debug)]
struct Billed {
    text: String,
    billed: crate::prompt_cache::BilledCache,
    engine: Option<u64>,
}

impl Stack {
    /// The gateway's stack for a fresh user with `terms` on a cache-tariffed [`MODEL`].
    /// The tokenizer answers one 1_500-token segment, for marked requests.
    async fn new(pool: PgPool, terms: Option<CacheAssurance>) -> Self {
        Self::with_orgs(pool, false, terms).await
    }

    /// As [`Self::new`], with the user also in the `orgs` list when `allowlisted`.
    async fn with_orgs(pool: PgPool, allowlisted: bool, terms: Option<CacheAssurance>) -> Self {
        let f = fixture_with(pool.clone(), allowlisted, 0, terms).await;
        let endpoint = create_test_endpoint(&pool, "ep", f.principal).await;
        let model_id = create_test_model(&pool, "m", MODEL, endpoint, f.principal).await;
        sqlx::query!(
            r#"INSERT INTO model_cache_tariffs
                 (deployed_model_id, write_multiplier_5m, write_multiplier_1h, write_multiplier_24h, min_prefix_tokens)
               VALUES ($1, 1.25, 2.0, 2.5, 1024)"#,
            model_id
        )
        .execute(&pool)
        .await
        .unwrap();

        let tok = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v1/models"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
                "models": [{"alias": MODEL, "hf_repo": "o/m", "tokenizer_version": "sha256:t1"}]
            })))
            .mount(&tok)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/tokenize"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
                "virtual_model": MODEL, "tokenizer_version": "sha256:t1",
                "segment_counts": [1500], "cumulative": [1500], "total": 1500
            })))
            .mount(&tok)
            .await;
        // The server outlives this function; keep the mock alive with it.
        let tok_uri = tok.uri();
        std::mem::forget(tok);

        let (tier, telemetry) = policies();
        let classifier = Classifier::new(
            PrincipalResolver::new(pool.clone()),
            ModelConfigResolver::new(pool.clone()),
            crate::prompt_cache::TokenizerClient::new(tok_uri),
            Arc::new(PostgresIndex::new(pool.clone(), 1)),
            tier,
            telemetry,
            false,
        );
        let up = Upstream(Arc::new(Mutex::new(Reply {
            prompt_tokens: 0,
            engine_cached: 0,
        })));
        let observed: Observed = Default::default();
        let translators: Vec<Arc<dyn ProtocolTranslator>> = vec![Arc::new(AnthropicMessages::new(true)), Arc::new(OpenResponses::new())];
        let app = Router::new()
            .route("/v1/chat/completions", post(upstream))
            .route("/v1/messages", post(upstream))
            .route("/v1/responses", post(upstream))
            .with_state(up.clone())
            .layer(from_fn_with_state(
                CacheLayerState::new(classifier, usize::MAX, Duration::from_secs(5)).with_reference_cache(f.cache.clone()),
                cache_middleware,
            ))
            .layer(from_fn_with_state(TranslationRegistry::new(translators), translation_middleware))
            .layer(from_fn_with_state(observed.clone(), observe));
        Self {
            f,
            server: axum_test::TestServer::new(app).unwrap(),
            upstream: up,
            observed,
        }
    }

    async fn send(&self, path: &str, body: &Value, prompt_tokens: u64, engine_cached: u64) -> Billed {
        self.upstream.set(prompt_tokens, engine_cached);
        let r = self
            .server
            .post(path)
            .add_header("authorization", format!("Bearer {}", self.f.secret))
            .json(body)
            .await;
        r.assert_status_ok();
        let ext = self.observed.lock().unwrap().take().expect("the response passed the observer");
        Billed {
            text: r.text(),
            billed: ext.get::<CacheBilling>().and_then(CacheBilling::get).unwrap_or_default(),
            engine: ext.get::<UpstreamCachedTokens>().and_then(UpstreamCachedTokens::get),
        }
    }
}

fn chat(messages: &[(&str, &str)], stream: bool) -> Value {
    let messages: Vec<Value> = messages.iter().map(|(r, c)| json!({"role": r, "content": c})).collect();
    json!({"model": MODEL, "messages": messages, "stream": stream})
}

/// The chat usage the customer saw: the body's `usage`, or the last streamed one.
fn chat_usage(text: &str) -> Value {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        return v["usage"].clone();
    }
    text.lines()
        .rev()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .filter_map(|v| v.get("usage").filter(|u| !u.is_null()).cloned())
        .next()
        .expect("a usage frame")
}

/// The chat body the cache layer sees for a native request (markers never enter the
/// hashes, so the request as sent hashes the same).
fn chat_bytes(v: &Value) -> Bytes {
    Bytes::from(serde_json::to_vec(v).unwrap())
}

async fn assured_chat_round_trip(pool: PgPool, stream: bool) {
    let s = Stack::new(pool, Some(CacheAssurance::GapShare(0.5))).await;
    let turn_1 = chat(&[("system", SYSTEM), ("user", USER_1)], stream);
    let r1 = s.send("/v1/chat/completions", &turn_1, 1_200, 0).await;
    assert_eq!(r1.billed.reference, Some(0), "a cold cache is a recorded miss");
    assert_eq!((r1.billed.stats.read, r1.billed.read_source), (0, None));
    s.f.wait_for(&chat_bytes(&turn_1), true).await;

    let turn_2 = chat(
        &[("system", SYSTEM), ("user", USER_1), ("assistant", ASSISTANT_1), ("user", USER_2)],
        stream,
    );
    let r2 = s.send("/v1/chat/completions", &turn_2, 1_500, 300).await;
    // The reference saw turn 1's whole prompt; the engine only 300 of it. Half the gap
    // of 900 is billed as cached.
    assert_eq!(r2.billed.reference, Some(1_200));
    assert_eq!(r2.billed.stats.read, 750);
    assert_eq!(r2.billed.read_source, Some(CacheReadSource::Reference));
    assert_eq!(r2.engine, Some(300), "the physical hit stays visible");
    let usage = chat_usage(&r2.text);
    assert_eq!(
        usage["prompt_tokens_details"]["cached_tokens"], 750,
        "the customer is told what was billed"
    );
    assert_eq!(usage["cache_read_input_tokens"], 750);
    assert_eq!(usage["cache_creation_input_tokens"], 0);
    assert_eq!(usage["prompt_tokens"], 1_500);
}

#[dwctl_test_macros::test]
async fn an_unmarked_request_is_billed_from_the_reference(pool: PgPool) {
    assured_chat_round_trip(pool, false).await;
}

#[dwctl_test_macros::test]
async fn an_unmarked_stream_is_billed_from_the_reference_at_its_usage_frame(pool: PgPool) {
    assured_chat_round_trip(pool, true).await;
}

/// Without terms the estimate is recorded and the engine billed, as before.
#[dwctl_test_macros::test]
async fn without_terms_the_estimate_is_only_recorded(pool: PgPool) {
    let s = Stack::with_orgs(pool, true, None).await;
    let turn_1 = chat(&[("system", SYSTEM), ("user", USER_1)], false);
    s.send("/v1/chat/completions", &turn_1, 1_200, 0).await;
    s.f.wait_for(&chat_bytes(&turn_1), true).await;
    let turn_2 = chat(
        &[("system", SYSTEM), ("user", USER_1), ("assistant", ASSISTANT_1), ("user", USER_2)],
        false,
    );
    let r2 = s.send("/v1/chat/completions", &turn_2, 1_500, 300).await;
    assert_eq!(r2.billed.reference, Some(1_200));
    assert_eq!((r2.billed.stats.read, r2.billed.read_source), (300, Some(CacheReadSource::Engine)));
}

/// A marked request from an assured organisation is the module's alone: the reference
/// is recorded but never billed.
#[dwctl_test_macros::test]
async fn a_marked_request_stays_on_the_module(pool: PgPool) {
    let s = Stack::new(pool, Some(CacheAssurance::GapShare(1.0))).await;
    let turn_1 = chat(&[("system", SYSTEM), ("user", USER_1)], false);
    s.send("/v1/chat/completions", &turn_1, 1_200, 0).await;
    s.f.wait_for(&chat_bytes(&turn_1), true).await;

    let mut marked = chat(
        &[("system", SYSTEM), ("user", USER_1), ("assistant", ASSISTANT_1), ("user", USER_2)],
        false,
    );
    marked["messages"][0]["content"] = json!([{"type": "text", "text": SYSTEM, "cache_control": {"type": "ephemeral"}}]);
    let r = s.send("/v1/chat/completions", &marked, 1_500, 300).await;
    assert_eq!(r.billed.reference, Some(1_200), "recorded all the same");
    assert_eq!(r.billed.stats.read, 0, "the module index is cold; the engine's 300 are ignored");
    assert_eq!(r.billed.stats.creation_5m, 1_500, "the module wrote the marked prefix");
    assert_ne!(r.billed.read_source, Some(CacheReadSource::Reference));
}

/// A lookup that misses the join deadline bills the engine's hit, records no estimate,
/// and still writes the request's entries once it finishes.
#[dwctl_test_macros::test]
async fn a_late_lookup_falls_back_to_the_engine(pool: PgPool) {
    let s = Stack::new(pool, Some(CacheAssurance::GapShare(0.5))).await;
    let mut blocker = s.f.pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE reference_cache_entries IN EXCLUSIVE MODE")
        .execute(&mut *blocker)
        .await
        .unwrap();
    for stream in [false, true] {
        let turn = chat(&[("system", SYSTEM), ("user", if stream { USER_2 } else { USER_1 })], stream);
        let r = s.send("/v1/chat/completions", &turn, 1_200, 300).await;
        assert_eq!(r.billed.reference, None, "stream: {stream}");
        assert_eq!(r.billed.stats.read, 300);
        assert_eq!(r.billed.read_source, Some(CacheReadSource::Engine));
        assert_eq!(chat_usage(&r.text)["prompt_tokens_details"]["cached_tokens"], 300);
    }
    assert_eq!(s.f.rows().await, 0, "nothing written while the table is held");
    blocker.rollback().await.unwrap();
    s.f.wait_for(&chat_bytes(&chat(&[("system", SYSTEM), ("user", USER_1)], false)), true)
        .await;
    s.f.wait_for(&chat_bytes(&chat(&[("system", SYSTEM), ("user", USER_2)], true)), true)
        .await;
}

/// A stream's prefixes are visible from its first output, while it is still open.
#[dwctl_test_macros::test]
async fn a_stream_writes_at_its_first_output(pool: PgPool) {
    use futures::StreamExt;
    use tower::ServiceExt;

    let s = Stack::new(pool.clone(), Some(CacheAssurance::GapShare(0.5))).await;
    let (chunks_tx, chunks_rx) = futures::channel::mpsc::unbounded::<Result<Bytes, std::io::Error>>();
    let chunks_rx = Arc::new(Mutex::new(Some(chunks_rx)));
    let (tier, telemetry) = policies();
    let classifier = Classifier::new(
        PrincipalResolver::new(pool.clone()),
        ModelConfigResolver::new(pool.clone()),
        crate::prompt_cache::TokenizerClient::new("http://127.0.0.1:1"),
        Arc::new(PostgresIndex::new(pool.clone(), 1)),
        tier,
        telemetry,
        false,
    );
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(move || async move {
                let rx = chunks_rx.lock().unwrap().take().expect("one request");
                ([(header::CONTENT_TYPE, "text/event-stream")], Body::from_stream(rx)).into_response()
            }),
        )
        .layer(from_fn_with_state(
            CacheLayerState::new(classifier, usize::MAX, Duration::from_secs(5)).with_reference_cache(s.f.cache.clone()),
            cache_middleware,
        ));
    let turn = chat(&[("system", SYSTEM), ("user", USER_1)], true);
    let request = axum::http::Request::post("/v1/chat/completions")
        .header("authorization", format!("Bearer {}", s.f.secret))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&turn).unwrap()))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let mut forwarded = response.into_body().into_data_stream();
    let send = |c: &'static str| chunks_tx.unbounded_send(Ok(Bytes::from_static(c.as_bytes()))).unwrap();

    send(": keep-alive\n\n");
    forwarded.next().await.unwrap().unwrap();
    assert_eq!(s.f.rows().await, 0, "nothing before output starts");
    send("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"}}]}\n\n");
    forwarded.next().await.unwrap().unwrap();
    s.f.wait_for(&chat_bytes(&turn), false).await;
    assert_eq!(s.f.rows().await, 2, "both boundaries, while the stream is still open");

    send("data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1200,\"completion_tokens\":1,\"total_tokens\":1201}}\n\n");
    send("data: [DONE]\n\n");
    drop(chunks_tx);
    while forwarded.next().await.is_some() {}
    s.f.wait_for(&chat_bytes(&turn), true).await;
}

fn translate(translator: &dyn ProtocolTranslator, path: &str, body: &Value) -> Bytes {
    let (parts, ()) = axum::http::Request::post(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(())
        .unwrap()
        .into_parts();
    translator
        .translate_request(&parts, Bytes::from(serde_json::to_vec(body).unwrap()))
        .unwrap_or_else(|e| panic!("translate {path}: {e:?}"))
        .body
}

/// A Messages conversation writes entries and its next turn matches them, like a chat
/// one: the cache layer hashes the translated body.
#[dwctl_test_macros::test]
async fn messages_requests_write_and_match_entries(pool: PgPool) {
    let s = Stack::new(pool, Some(CacheAssurance::GapShare(0.5))).await;
    let messages = AnthropicMessages::new(true);
    for stream in [false, true] {
        // A conversation of its own per pass: the second must not match the first's system prompt.
        let (system, user_1) = if stream {
            ("A second system prompt.", USER_2)
        } else {
            (SYSTEM, USER_1)
        };
        let turn_1 = json!({"model": MODEL, "max_tokens": 64, "stream": stream, "system": system,
                            "messages": [{"role": "user", "content": user_1}]});
        let r1 = s.send("/v1/messages", &turn_1, 1_200, 0).await;
        assert_eq!(r1.billed.reference, Some(0));
        s.f.wait_for(&translate(&messages, "/v1/messages", &turn_1), true).await;

        let turn_2 = json!({"model": MODEL, "max_tokens": 64, "stream": stream, "system": system, "messages": [
            {"role": "user", "content": user_1},
            {"role": "assistant", "content": [{"type": "text", "text": ASSISTANT_1}]},
            {"role": "user", "content": "Thanks."},
        ]});
        let r2 = s.send("/v1/messages", &turn_2, 1_500, 0).await;
        assert_eq!(r2.billed.reference, Some(1_200), "stream: {stream}");
        assert_eq!(
            (r2.billed.stats.read, r2.billed.read_source),
            (600, Some(CacheReadSource::Reference))
        );
        assert!(r2.text.contains("\"cache_read_input_tokens\":600"), "{}", r2.text);
    }
}

struct StaticStore(ResponseContext);

#[async_trait]
impl ResponseStore for StaticStore {
    async fn store(&self, _response: &Value) -> Result<String, StoreError> {
        unreachable!("hydration only reads")
    }

    async fn get_context(&self, _response_id: &str, _owner: &str) -> Result<Option<ResponseContext>, StoreError> {
        Ok(Some(self.0.clone()))
    }
}

fn chat_completion(message: Value) -> Bytes {
    Bytes::from(
        serde_json::to_vec(&json!({
            "id": "c1", "object": "chat.completion", "created": 1, "model": MODEL,
            "choices": [{"index": 0, "message": message, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}
        }))
        .unwrap(),
    )
}

/// Hydrate `request` against a stored previous turn, as the inference middleware does.
async fn hydrated(request: Value, previous_request: &Value, previous_reply: Value) -> Value {
    let request_bytes = Bytes::from(serde_json::to_vec(previous_request).unwrap());
    let response = OpenResponses::new()
        .translate_response(&request_bytes, Some("resp_previous"), chat_completion(previous_reply))
        .unwrap();
    let store = StaticStore(ResponseContext {
        input: previous_request["input"].clone(),
        response: serde_json::from_slice(&response).unwrap(),
    });
    let mut request = request;
    if hydrate_previous_response(&store, "owner", &mut request).await.is_err() {
        panic!("hydration failed");
    }
    request
}

/// A Responses conversation continued by `previous_response_id` (hydrated in the outer
/// inference middleware) writes entries and its next turn matches them.
#[dwctl_test_macros::test]
async fn responses_requests_write_and_match_entries(pool: PgPool) {
    let s = Stack::new(pool, Some(CacheAssurance::GapShare(0.5))).await;
    let responses = OpenResponses::new();
    for stream in [false, true] {
        let (system, user_1) = if stream {
            ("A second system prompt.", USER_2)
        } else {
            (SYSTEM, USER_1)
        };
        let turn_1 = json!({"model": MODEL, "instructions": system, "input": user_1, "stream": stream});
        let r1 = s.send("/v1/responses", &turn_1, 1_200, 0).await;
        assert_eq!(r1.billed.reference, Some(0));
        s.f.wait_for(&translate(&responses, "/v1/responses", &turn_1), true).await;

        let turn_2 = hydrated(
            json!({"model": MODEL, "instructions": system, "input": "Thanks.", "stream": stream, "previous_response_id": "resp_previous"}),
            &turn_1,
            json!({"role": "assistant", "content": ASSISTANT_1}),
        )
        .await;
        let r2 = s.send("/v1/responses", &turn_2, 1_500, 0).await;
        assert_eq!(r2.billed.reference, Some(1_200), "stream: {stream}");
        assert_eq!(
            (r2.billed.stats.read, r2.billed.read_source),
            (600, Some(CacheReadSource::Reference))
        );
    }
}

// ===== Translator stability =====

/// Turn `next` repeats every boundary of turn `prev`, in order.
fn assert_prefix(prev: &[u8], next: &[u8], what: &str) {
    let (p, n) = (hashes(prev), hashes(next));
    assert!(!p.is_empty(), "{what}: no boundaries");
    assert!(n.len() > p.len(), "{what}: the next turn adds blocks");
    assert_eq!(&n[..p.len()], &p[..], "{what}: turn N's prefix is not a prefix of turn N+1");
}

#[test]
fn messages_turns_translate_to_a_stable_prefix() {
    let messages = AnthropicMessages::new(true);
    let tools = json!([{"name": "find_drill", "description": "Look a drill up.",
                        "input_schema": {"type": "object", "properties": {"q": {"type": "string"}}, "required": ["q"]}}]);
    let system = json!([{"type": "text", "text": SYSTEM}, {"type": "text", "text": "Prices are in euros."}]);
    let history = [
        json!({"role": "user", "content": USER_1}),
        json!({"role": "assistant", "content": [
            {"type": "text", "text": "Let me look."},
            {"type": "tool_use", "id": "toolu_1", "name": "find_drill", "input": {"q": "lightest"}}
        ]}),
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": "compact: 1.1 kg"}
        ]}),
        json!({"role": "assistant", "content": [{"type": "text", "text": ASSISTANT_1}]}),
        json!({"role": "user", "content": [{"type": "text", "text": USER_2}]}),
    ];
    let turn = |n: usize| {
        translate(
            &messages,
            "/v1/messages",
            &json!({"model": MODEL, "max_tokens": 64, "system": system, "tools": tools, "messages": history[..n]}),
        )
    };
    for (n, next) in [(1, 3), (3, 5)] {
        assert_prefix(&turn(n), &turn(next), &format!("messages {n} -> {next}"));
    }
}

#[tokio::test]
async fn responses_turns_with_hydration_translate_to_a_stable_prefix() {
    let responses = OpenResponses::new();
    let tools = json!([{"type": "function", "name": "find_drill", "description": "Look a drill up.",
                        "parameters": {"type": "object", "properties": {"q": {"type": "string"}}, "required": ["q"]}}]);
    let body = |input: Value, previous: Option<&str>| {
        let mut v = json!({"model": MODEL, "instructions": SYSTEM, "tools": tools, "input": input});
        if let Some(id) = previous {
            v["previous_response_id"] = json!(id);
        }
        v
    };

    // Turn 1: a question; the model answers with a tool call.
    let turn_1 = body(json!(USER_1), None);
    let reply_1 = json!({"role": "assistant", "content": null, "tool_calls": [
        {"id": "call_1", "type": "function", "function": {"name": "find_drill", "arguments": "{\"q\":\"lightest\"}"}}
    ]});
    // Turn 2: the tool's output, continuing turn 1; the model answers in text.
    let turn_2 = hydrated(
        body(
            json!([{"type": "function_call_output", "call_id": "call_1", "output": "compact: 1.1 kg"}]),
            Some("resp_1"),
        ),
        &turn_1,
        reply_1,
    )
    .await;
    // Turn 3: a follow-up, continuing the (already hydrated) turn 2.
    let turn_3 = hydrated(
        body(json!(USER_2), Some("resp_2")),
        &turn_2,
        json!({"role": "assistant", "content": ASSISTANT_1}),
    )
    .await;

    let [t1, t2, t3] = [&turn_1, &turn_2, &turn_3].map(|t| translate(&responses, "/v1/responses", t));
    assert_prefix(&t1, &t2, "responses 1 -> 2 (tool call hydrated)");
    assert_prefix(&t2, &t3, "responses 2 -> 3 (text hydrated)");

    // The same conversation sent in full each time, without previous_response_id,
    // reaches the same boundaries as the hydrated one.
    let full_3 = body(turn_3["input"].clone(), None);
    assert_eq!(
        hashes(&translate(&responses, "/v1/responses", &full_3)),
        hashes(&t3),
        "hydration renders exactly what a client resending the history would"
    );
}
