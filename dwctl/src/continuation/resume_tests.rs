//! End-to-end tests for the resume middleware against a scripted fake inner
//! service.
//!
//! The fake plays the role of everything below this layer (error enrichment →
//! onwards → upstream): it serves `/chat/completions` with a scripted death and
//! `/completions` with scripted resume legs, and records exactly what the resume
//! leg asked for. Death modes mirror the fault-injection matrix
//! (`continuation-fault-injection-handover.md` §modes 1-9), so a mode that gets
//! reproduced in staging has a local twin here.
//!
//! Everything is deterministic: no sleeps, no polling for wall-clock effects.
//! The stall test is the sole exception and uses a 1s configured deadline, which
//! is the behaviour under test rather than a wait for something else to happen.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router, middleware};
use bytes::Bytes;
use futures::StreamExt;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use wiremock::matchers::{method as wm_method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::config::ContinuationConfig;

use super::layer::{ContinuationState, continuation_middleware};
use super::render::RenderClient;
use super::{ContinuationRoutes, InflightLimiter, PurposeResolver};

const MODEL: &str = "dsv4-flash";
const KEY: &str = "dw-continuation-global-key";

// ── the scripted fake inner service ──────────────────────────────────────────

/// One scripted stream item.
#[derive(Clone, Debug)]
enum Chunk {
    /// Raw SSE bytes, sent as one body chunk.
    Data(String),
    /// A transport failure mid-stream (hyper reset / incomplete body).
    Reset,
    /// Send nothing, ever, without closing — the stall mode.
    Hang,
    /// A real pause (ms) before the next item — how slow time-to-first-token
    /// is scripted. This is scripted upstream latency (the thing under test),
    /// not a poll-for-condition sleep.
    Delay(u64),
}

fn frame(value: Value) -> Chunk {
    Chunk::Data(format!("data: {value}\n\n"))
}

fn content(id: &str, text: &str) -> Chunk {
    frame(json!({
        "id": id, "object": "chat.completion.chunk", "created": 1_700_000_000, "model": MODEL,
        "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
    }))
}

fn leg_text(text: &str, finish: Option<&str>) -> Chunk {
    frame(json!({
        "id": "cmpl-leg", "object": "text_completion", "created": 1_800_000_000, "model": "continuation-composite",
        "choices": [{"text": text, "index": 0, "finish_reason": finish}]
    }))
}

fn leg_usage(prompt: u64, completion: u64) -> Chunk {
    frame(json!({
        "id": "cmpl-leg", "object": "text_completion", "created": 1_800_000_000, "model": "continuation-composite",
        "choices": [],
        "usage": {"prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": prompt + completion,
                  "prompt_tokens_details": {"cached_tokens": 900}}
    }))
}

fn done() -> Chunk {
    Chunk::Data("data: [DONE]\n\n".to_string())
}

/// A recorded resume-leg request.
#[derive(Clone, Debug)]
struct Received {
    path: String,
    headers: HeaderMap,
    body: Value,
}

#[derive(Clone)]
struct Fake {
    /// The scripted `/chat/completions` (leg 1) response.
    leg_one: Arc<Mutex<Vec<Chunk>>>,
    /// Scripted `/completions` responses, consumed in order. An exhausted queue
    /// answers 503 — the "no leg available" case.
    legs: Arc<Mutex<Vec<Vec<Chunk>>>>,
    received: Arc<Mutex<Vec<Received>>>,
}

impl Fake {
    fn new(leg_one: Vec<Chunk>, legs: Vec<Vec<Chunk>>) -> Self {
        Self {
            leg_one: Arc::new(Mutex::new(leg_one)),
            legs: Arc::new(Mutex::new(legs.into_iter().rev().collect())),
            received: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn resume_requests(&self) -> Vec<Received> {
        self.received.lock().unwrap().clone()
    }

    fn router(&self) -> Router {
        Router::new()
            .route("/chat/completions", post(leg_one_handler))
            .route("/completions", post(leg_two_handler))
            .with_state(self.clone())
    }
}

fn sse_stream(chunks: Vec<Chunk>) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> {
    futures::stream::iter(chunks).then(|c| async move {
        match c {
            Chunk::Data(s) => Ok(Bytes::from(s)),
            Chunk::Reset => Err(std::io::Error::other("upstream connection reset")),
            Chunk::Delay(ms) => {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                Ok(Bytes::new())
            }
            Chunk::Hang => unreachable!("Hang is expanded before streaming"),
        }
    })
}

fn sse_response(chunks: Vec<Chunk>) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(sse_stream(chunks)))
        .unwrap()
}

/// Split a script at its first `Hang`: the prefix is streamed, then the body
/// never yields again (and never closes).
fn stream_script(chunks: Vec<Chunk>) -> Response {
    match chunks.iter().position(|c| matches!(c, Chunk::Hang)) {
        None => sse_response(chunks),
        Some(at) => {
            let head: Vec<Chunk> = chunks[..at].to_vec();
            let stream = sse_stream(head).chain(futures::stream::pending());
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }
    }
}

async fn leg_one_handler(State(fake): State<Fake>) -> Response {
    let chunks = fake.leg_one.lock().unwrap().clone();
    stream_script(chunks)
}

async fn leg_two_handler(State(fake): State<Fake>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    fake.received.lock().unwrap().push(Received {
        path: "/completions".to_string(),
        headers,
        body,
    });
    let next = fake.legs.lock().unwrap().pop();
    match next {
        Some(chunks) => stream_script(chunks),
        None => (StatusCode::SERVICE_UNAVAILABLE, "no continuation capacity").into_response(),
    }
}

// ── harness ──────────────────────────────────────────────────────────────────

async fn render_stub(token_ids: Vec<u32>, total: u32, continuation_tokens: u32) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(wm_method("POST"))
        .and(wm_path("/v1/render"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "virtual_model": MODEL,
            "tokenizer_version": "sha256:t",
            "template_version": "sha256:x",
            "token_ids": token_ids,
            "total": total,
            "continuation_tokens": continuation_tokens
        })))
        .mount(&server)
        .await;
    server
}

fn test_config() -> ContinuationConfig {
    ContinuationConfig {
        enabled: true,
        max_attempts: 2,
        ..ContinuationConfig::default()
    }
}

fn state(pool: PgPool, fake: &Fake, tokenizer_url: String, cfg: ContinuationConfig) -> ContinuationState {
    ContinuationState {
        tokenizer: RenderClient::new(tokenizer_url, Duration::from_secs(5)),
        inflight: Arc::new(InflightLimiter::new(cfg.max_inflight_per_model)),
        cfg: Arc::new(cfg),
        key_secret: Arc::from(KEY),
        resume_target: fake.router(),
        routes: Arc::new(ContinuationRoutes::with_models([MODEL.to_string()])),
        purposes: PurposeResolver::new(pool),
        body_limit: 8 * 1024 * 1024,
    }
}

fn app(fake: &Fake, state: ContinuationState) -> Router {
    fake.router().layer(middleware::from_fn_with_state(state, continuation_middleware))
}

fn chat_request(body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn streaming_body() -> Value {
    json!({
        "model": MODEL,
        "stream": true,
        "max_tokens": 500,
        "temperature": 0.7,
        "messages": [{"role": "user", "content": "hello"}]
    })
}

/// Every `data:` payload the client received, in order. A trailing transport
/// error is expected on pass-through cases (the death reaches the client
/// verbatim), so bytes are taken up to it rather than unwrapped.
async fn collect_payloads(response: Response) -> Vec<String> {
    let mut body = response.into_body().into_data_stream();
    let mut bytes = Vec::new();
    while let Some(Ok(chunk)) = body.next().await {
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8_lossy(&bytes)
        .split("\n\n")
        .filter_map(|event| event.lines().find_map(|l| l.strip_prefix("data:")).map(|d| d.trim().to_string()))
        .collect()
}

fn parsed(payloads: &[String]) -> Vec<Value> {
    payloads
        .iter()
        .filter(|p| *p != "[DONE]")
        .map(|p| serde_json::from_str(p).unwrap())
        .collect()
}

fn contents(frames: &[Value]) -> String {
    frames.iter().filter_map(|f| f["choices"][0]["delta"]["content"].as_str()).collect()
}

fn usage_frames(frames: &[Value]) -> Vec<&Value> {
    frames.iter().filter(|f| f.get("usage").is_some_and(|u| !u.is_null())).collect()
}

// ── mode 1: cut between frames → resumed ─────────────────────────────────────

/// The headline case. Leg 1 dies after two content deltas with no finish_reason
/// and no `[DONE]`; the resume leg finishes the sentence. The client sees one
/// continuous stream: both legs' text, exactly one usage frame, one `[DONE]`.
#[sqlx::test]
async fn a_cut_stream_is_resumed_into_one_seamless_response(pool: PgPool) {
    let fake = Fake::new(
        vec![content("chatcmpl-1", "Hello"), content("chatcmpl-1", ", wor")],
        vec![vec![leg_text("ld!", Some("stop")), leg_usage(1012, 8), done()]],
    );
    let tokenizer = render_stub(vec![1, 2, 3], 1012, 12).await;
    let st = state(pool, &fake, tokenizer.uri(), test_config());

    let response = app(&fake, st).oneshot(chat_request(streaming_body())).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let payloads = collect_payloads(response).await;
    let frames = parsed(&payloads);

    assert_eq!(contents(&frames), "Hello, world!", "the client sees one uninterrupted generation");
    assert_eq!(payloads.last().unwrap(), "[DONE]", "the stream terminates properly");

    // Every frame the client received is a chat chunk on the ORIGINAL id — the
    // resume leg's completions envelope never leaks.
    for f in &frames {
        assert_eq!(f["object"], "chat.completion.chunk");
        assert_eq!(f["id"], "chatcmpl-1");
        assert_eq!(f["model"], MODEL);
    }
    assert_eq!(
        frames.iter().filter(|f| f["choices"][0]["finish_reason"] == "stop").count(),
        1,
        "exactly one finish_reason, from the leg that actually finished"
    );

    // ── the billing-critical assertion ──
    let usage = usage_frames(&frames);
    assert_eq!(usage.len(), 1, "outlet and the cache layer must see exactly ONE usage frame");
    let usage = &usage[0]["usage"];
    // seg = 12 (generated before the leg), leg prompt = 1012, leg completion = 8.
    assert_eq!(
        usage["prompt_tokens"], 1000,
        "the customer pays for their prompt once, not once per leg"
    );
    assert_eq!(usage["completion_tokens"], 20, "12 tokens from leg 1 + 8 from the resume leg");
    assert_eq!(usage["total_tokens"], 1020);
    assert!(
        usage.get("prompt_tokens_details").is_none(),
        "the leg's own cache accounting describes the re-prefill, not the customer's request"
    );
}

/// Batch loopback: fusillade marks stream-intent with `x-fusillade-stream`
/// while the BODY still says nothing about streaming (the outbound middleware
/// forces `stream: true` below this layer). The header alone must arm the tee,
/// or batch-origin streams — the largest death population — can never resume.
#[sqlx::test]
async fn a_fusillade_stream_header_arms_the_tee_without_body_stream(pool: PgPool) {
    let fake = Fake::new(
        vec![content("chatcmpl-1", "Hello"), content("chatcmpl-1", ", wor")],
        vec![vec![leg_text("ld!", Some("stop")), leg_usage(1012, 8), done()]],
    );
    let tokenizer = render_stub(vec![1, 2, 3], 1012, 12).await;
    let mut cfg = test_config();
    cfg.origins.batch = true;
    let st = state(pool, &fake, tokenizer.uri(), cfg);

    let mut body = streaming_body();
    body.as_object_mut().unwrap().remove("stream");
    let request = Request::builder()
        .method("POST")
        .uri("/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-fusillade-stream", "true")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();

    let response = app(&fake, st).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let payloads = collect_payloads(response).await;
    let frames = parsed(&payloads);
    assert_eq!(
        contents(&frames),
        "Hello, world!",
        "a batch-origin death is rescued even though the body carried no stream flag"
    );
    assert_eq!(payloads.last().unwrap(), "[DONE]");
    assert_eq!(usage_frames(&frames).len(), 1);
}

/// What the resume leg actually asked for. Everything here is load-bearing: the
/// token-id prompt (no re-templating downstream), the global key (immune to the
/// customer's credit state), the priority hint (jump the dynamo queue), usage
/// reporting (the merge depends on it) and the decremented cap.
#[sqlx::test]
async fn the_resume_leg_asks_for_exactly_the_right_thing(pool: PgPool) {
    let fake = Fake::new(
        vec![content("chatcmpl-1", "Hello")],
        vec![vec![leg_text(" there", Some("stop")), leg_usage(1012, 8), done()]],
    );
    let tokenizer = render_stub(vec![101, 202, 303], 1012, 12).await;
    let st = state(pool, &fake, tokenizer.uri(), test_config());

    let response = app(&fake, st).oneshot(chat_request(streaming_body())).await.unwrap();
    collect_payloads(response).await;

    let requests = fake.resume_requests();
    assert_eq!(requests.len(), 1, "one death, one leg");
    let leg = &requests[0];
    assert_eq!(leg.path, "/completions", "a chat request resumes on the completions endpoint");
    assert_eq!(
        leg.headers.get(header::AUTHORIZATION).unwrap(),
        format!("Bearer {KEY}").as_str(),
        "resume legs authenticate as the global continuation key"
    );
    assert_eq!(leg.body["prompt"], json!([101, 202, 303]), "the rendered token ids, verbatim");
    assert_eq!(leg.body["model"], MODEL);
    assert_eq!(leg.body["stream"], true);
    assert_eq!(leg.body["stream_options"]["include_usage"], true);
    assert_eq!(leg.body["priority"], 100);
    assert_eq!(leg.body["max_tokens"], 488, "500 requested minus the 12 already generated");
    assert_eq!(leg.body["temperature"], 0.7, "sampling parameters carry over");
    assert!(leg.body.get("messages").is_none(), "a completions leg never carries chat messages");
}

/// The inner-router path shape, discovered rather than assumed: dwctl nests the
/// AI routes under `/ai/v1`, so this layer sees the STRIPPED path and the resume
/// leg must be dispatched with the stripped path too. Asserted through a real
/// nest so a future change to the nesting breaks this test rather than
/// production.
#[sqlx::test]
async fn the_resume_leg_re_enters_at_the_path_the_inner_router_expects(pool: PgPool) {
    let fake = Fake::new(
        vec![content("chatcmpl-1", "Hello")],
        vec![vec![leg_text("!", Some("stop")), leg_usage(1012, 8), done()]],
    );
    let tokenizer = render_stub(vec![1], 1012, 12).await;
    let st = state(pool, &fake, tokenizer.uri(), test_config());
    let nested = Router::new().nest("/ai/v1", app(&fake, st));

    let request = Request::builder()
        .method("POST")
        .uri("/ai/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&streaming_body()).unwrap()))
        .unwrap();
    let payloads = collect_payloads(nested.oneshot(request).await.unwrap()).await;

    assert_eq!(contents(&parsed(&payloads)), "Hello!", "the resume leg was routed and spliced");
    assert_eq!(fake.resume_requests()[0].path, "/completions");
}

// ── modes 3, 5, 9: other resumable deaths ────────────────────────────────────

/// A transport reset (mode 3) and an error envelope inside a 200 stream (mode 5,
/// the OpenRouter family) and dynamo's worker-cancellation 499 (mode 9) all
/// resume, and none of them leaks its death to the client.
#[sqlx::test]
async fn resumable_death_signatures_all_recover_without_leaking_the_error(pool: PgPool) {
    let deaths = vec![
        ("transport reset", vec![content("chatcmpl-1", "Hi"), Chunk::Reset]),
        (
            "error envelope in a 200",
            vec![
                content("chatcmpl-1", "Hi"),
                frame(json!({"error": {"code": 502, "message": "Provider returned error"}})),
            ],
        ),
        (
            "dynamo worker cancellation",
            vec![
                content("chatcmpl-1", "Hi"),
                frame(json!({"error": {"code": 499, "message": "CancelledError: ", "type": "request_cancelled"}})),
            ],
        ),
    ];

    for (name, leg_one) in deaths {
        let fake = Fake::new(leg_one, vec![vec![leg_text("!", Some("stop")), leg_usage(1002, 2), done()]]);
        let tokenizer = render_stub(vec![1], 1002, 2).await;
        let st = state(pool.clone(), &fake, tokenizer.uri(), test_config());

        let payloads = collect_payloads(app(&fake, st).oneshot(chat_request(streaming_body())).await.unwrap()).await;
        let frames = parsed(&payloads);
        assert_eq!(contents(&frames), "Hi!", "case: {name}");
        assert!(
            frames.iter().all(|f| f.get("error").is_none()),
            "case: {name} — a death we recovered from must never reach the client"
        );
        assert_eq!(usage_frames(&frames).len(), 1, "case: {name}");
    }
}

/// Mode 4: the upstream stops sending without closing. The stall deadline turns
/// silence into a death at the last frame boundary, and the stream is resumed.
#[sqlx::test]
async fn a_stalled_stream_is_resumed_at_the_last_frame_boundary(pool: PgPool) {
    let fake = Fake::new(
        vec![content("chatcmpl-1", "Hel"), Chunk::Hang],
        vec![vec![leg_text("lo", Some("stop")), leg_usage(1003, 2), done()]],
    );
    let tokenizer = render_stub(vec![1], 1003, 3).await;
    let cfg = ContinuationConfig {
        // The behaviour under test: silence for this long is a death.
        resume_deadline_secs: 1,
        ..test_config()
    };
    let st = state(pool, &fake, tokenizer.uri(), cfg);

    let payloads = collect_payloads(app(&fake, st).oneshot(chat_request(streaming_body())).await.unwrap()).await;
    assert_eq!(contents(&parsed(&payloads)), "Hello");
}

/// A healthy stream whose first token takes longer than the resume deadline
/// must NOT be severed: pre-first-token silence is admission-queue/prefill
/// time, which the platform never bounds (there is no first-token deadline
/// anywhere, and nothing has been generated to resume). Before this fix the
/// stall timer wrapped the FIRST read too and turned every slow-TTFT stream
/// into a fabricated empty 200 at exactly the deadline.
#[sqlx::test]
async fn a_slow_first_token_is_never_severed(pool: PgPool) {
    let finished = frame(json!({
        "id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1_700_000_000, "model": MODEL,
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
    }));
    let usage = frame(json!({
        "id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1_700_000_000, "model": MODEL,
        "choices": [], "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
    }));
    let fake = Fake::new(
        // First token arrives well past the 1s deadline, then a normal close.
        vec![Chunk::Delay(1_400), content("chatcmpl-1", "Hello"), finished, usage, done()],
        vec![],
    );
    let tokenizer = render_stub(vec![1], 4, 1).await;
    let cfg = ContinuationConfig {
        resume_deadline_secs: 1,
        ..test_config()
    };
    let st = state(pool, &fake, tokenizer.uri(), cfg);

    let payloads = collect_payloads(app(&fake, st).oneshot(chat_request(streaming_body())).await.unwrap()).await;
    assert_eq!(contents(&parsed(&payloads)), "Hello", "the late stream arrives intact");
    assert!(fake.resume_requests().is_empty(), "a healthy slow stream is not a death");
    assert_eq!(payloads.last().unwrap(), "[DONE]");
}

/// An unparseable `data:` frame on leg 1 is forwarded to the client — we never
/// re-serialize their stream — but it DISARMS resume.
///
/// The accumulator could not ingest it, so our record of "what has been said so
/// far" is missing whatever that frame carried. Resuming from an incomplete
/// prefix would silently drop content the client already saw and stitch the
/// continuation onto the wrong place. Before this fix the frame was skipped in
/// silence and the resume went ahead anyway.
#[sqlx::test]
async fn an_unparseable_frame_reaches_the_client_and_disarms_resume(pool: PgPool) {
    let fake = Fake::new(
        vec![
            content("chatcmpl-1", "Hello"),
            // Well-formed SSE framing, malformed JSON payload.
            Chunk::Data("data: {\"choices\": [ NOPE\n\n".to_string()),
            Chunk::Reset,
        ],
        // A leg is scripted, so "no leg was dispatched" is a real assertion
        // rather than an artefact of having nothing to dispatch.
        vec![vec![leg_text(", world!", Some("stop")), leg_usage(1010, 3), done()]],
    );
    let tokenizer = render_stub(vec![1, 2, 3], 1010, 5).await;
    let st = state(pool, &fake, tokenizer.uri(), test_config());

    let payloads = collect_payloads(app(&fake, st).oneshot(chat_request(streaming_body())).await.unwrap()).await;

    // 1. The malformed frame still reached the client, byte for byte.
    assert!(
        payloads.iter().any(|p| p.contains("NOPE")),
        "the client's bytes are forwarded whatever we can make of them: {payloads:?}"
    );
    // 2. ...and the content before it did too. (Parsed selectively: `parsed`
    //    would unwrap the malformed payload this test exists to send.)
    let well_formed: Vec<Value> = payloads.iter().filter_map(|p| serde_json::from_str(p).ok()).collect();
    assert_eq!(contents(&well_formed), "Hello");
    // 3. But the stream is no longer resumable: no leg was dispatched, so the
    //    client sees the truncation rather than a silently wrong continuation.
    assert!(
        fake.resume_requests().is_empty(),
        "a stream we cannot fully reconstruct must not be resumed"
    );
}

// ── chain and exhaustion ─────────────────────────────────────────────────────

/// A resume leg that itself dies re-enters the same flow: its output is appended
/// to the same accumulated generation and the next leg continues from there.
#[sqlx::test]
async fn a_resume_leg_that_dies_is_itself_resumed(pool: PgPool) {
    let fake = Fake::new(
        vec![content("chatcmpl-1", "one ")],
        vec![
            // Leg 2 delivers a word, then dies (no finish_reason, no [DONE]).
            vec![leg_text("two ", None)],
            // Leg 3 finishes the job.
            vec![leg_text("three", Some("stop")), leg_usage(1020, 5), done()],
        ],
    );
    let tokenizer = render_stub(vec![1, 2], 1020, 15).await;
    let st = state(pool, &fake, tokenizer.uri(), test_config());

    let payloads = collect_payloads(app(&fake, st).oneshot(chat_request(streaming_body())).await.unwrap()).await;
    let frames = parsed(&payloads);
    assert_eq!(contents(&frames), "one two three", "each leg appends to the same generation");
    assert_eq!(fake.resume_requests().len(), 2, "two legs, both dispatched from the same chain");
    assert_eq!(usage_frames(&frames).len(), 1, "still exactly one terminal usage frame");
    assert_eq!(
        usage_frames(&frames)[0]["usage"]["completion_tokens"],
        20,
        "15 accumulated + 5 from the final leg"
    );
}

/// The attempt budget is per logical stream. When it runs out the client sees
/// exactly what an unresumed death would have given them — no fabricated usage,
/// no invented finish.
#[sqlx::test]
async fn an_exhausted_chain_ends_the_stream_like_an_unresumed_death(pool: PgPool) {
    let fake = Fake::new(
        vec![
            content("chatcmpl-1", "partial"),
            frame(json!({"error": {"code": 502, "message": "upstream died"}})),
        ],
        // Both scripted legs die immediately; max_attempts is 2.
        vec![vec![leg_text("", None)], vec![leg_text("", None)]],
    );
    let tokenizer = render_stub(vec![1], 1007, 7).await;
    let st = state(pool, &fake, tokenizer.uri(), test_config());

    let payloads = collect_payloads(app(&fake, st).oneshot(chat_request(streaming_body())).await.unwrap()).await;
    let frames = parsed(&payloads);

    assert_eq!(fake.resume_requests().len(), 2, "the attempt cap bounds the legs");
    assert_eq!(contents(&frames), "partial", "the client keeps everything it already received");
    assert!(
        frames.iter().any(|f| f.get("error").is_some()),
        "having failed to save the stream, the original death is surfaced as it is today"
    );
    assert!(usage_frames(&frames).is_empty(), "no usage is synthesized for a failed resume");
    assert!(
        !payloads.iter().any(|p| p == "[DONE]"),
        "a failed stream is not dressed up as a complete one"
    );
}

/// A leg that cannot even be dispatched (the target is out of capacity) consumes
/// an attempt rather than hanging the client.
#[sqlx::test]
async fn a_leg_the_target_refuses_consumes_an_attempt(pool: PgPool) {
    // No scripted legs at all: the fake answers 503.
    let fake = Fake::new(vec![content("chatcmpl-1", "partial"), Chunk::Reset], vec![]);
    let tokenizer = render_stub(vec![1], 1007, 7).await;
    let st = state(pool, &fake, tokenizer.uri(), test_config());

    let payloads = collect_payloads(app(&fake, st).oneshot(chat_request(streaming_body())).await.unwrap()).await;
    assert_eq!(contents(&parsed(&payloads)), "partial");
    assert_eq!(fake.resume_requests().len(), 2, "both attempts were spent on the refusing target");
}

/// A tokenizer-svc outage costs attempts, not correctness: no leg is dispatched
/// without a rendered prefix.
#[sqlx::test]
async fn a_render_failure_never_dispatches_a_leg(pool: PgPool) {
    let fake = Fake::new(
        vec![content("chatcmpl-1", "partial"), Chunk::Reset],
        vec![vec![leg_text("!", Some("stop")), leg_usage(1, 1), done()]],
    );
    let tokenizer = MockServer::start().await;
    Mock::given(wm_method("POST"))
        .and(wm_path("/v1/render"))
        .respond_with(ResponseTemplate::new(503).set_body_string("tokenizer down"))
        .mount(&tokenizer)
        .await;
    let st = state(pool, &fake, tokenizer.uri(), test_config());

    let payloads = collect_payloads(app(&fake, st).oneshot(chat_request(streaming_body())).await.unwrap()).await;
    assert_eq!(contents(&parsed(&payloads)), "partial");
    assert!(
        fake.resume_requests().is_empty(),
        "a leg without a rendered token-id prefix would be a guess, not a resume"
    );
}

// ── non-resumable deaths ─────────────────────────────────────────────────────

/// Mode 6 (ruling 2026-08-13): a 4xx envelope AFTER partial output resumes. A
/// genuine input rejection lands as the FIRST frame; one that arrives after
/// accepted, partially-streamed output is a proxy/downstream fault wearing a
/// client status. Genuine bad input self-corrects — the leg is rejected too,
/// attempts exhaust, and the original error surfaces (the exhausted-chain
/// test pins that path).
#[sqlx::test]
async fn a_4xx_envelope_after_partial_output_is_resumed(pool: PgPool) {
    let death = json!({"error": {"code": 400, "message": "input too long", "type": "invalid_request_error"}});
    let fake = Fake::new(
        vec![content("chatcmpl-1", "partial"), frame(death)],
        vec![vec![leg_text("!", Some("stop")), leg_usage(1007, 1), done()]],
    );
    let tokenizer = render_stub(vec![1], 1007, 7).await;
    let st = state(pool, &fake, tokenizer.uri(), test_config());

    let payloads = collect_payloads(app(&fake, st).oneshot(chat_request(streaming_body())).await.unwrap()).await;
    let frames = parsed(&payloads);
    assert_eq!(fake.resume_requests().len(), 1, "a post-partial-output 4xx dispatches a resume leg");
    assert_eq!(contents(&frames), "partial!");
    assert!(
        frames.iter().all(|f| f.get("error").is_none_or(Value::is_null)),
        "the rescued client never sees the envelope"
    );
}

/// A 4xx envelope as the FIRST frame is the genuine-rejection shape: nothing
/// was generated, so there is no prefix — the error surfaces unchanged and no
/// leg is ever dispatched (resume-from-zero is a plain retry, not this
/// feature's job).
#[sqlx::test]
async fn a_first_frame_4xx_surfaces_with_no_resume(pool: PgPool) {
    let death = json!({"error": {"code": 400, "message": "input too long", "type": "invalid_request_error"}});
    let fake = Fake::new(vec![frame(death)], vec![]);
    let tokenizer = render_stub(vec![1], 1, 1).await;
    let st = state(pool, &fake, tokenizer.uri(), test_config());

    let payloads = collect_payloads(app(&fake, st).oneshot(chat_request(streaming_body())).await.unwrap()).await;
    assert!(fake.resume_requests().is_empty(), "nothing to resume from");
    assert_eq!(
        parsed(&payloads).last().unwrap()["error"]["code"],
        400,
        "the client sees the rejection exactly as it does today"
    );
}

/// A death we refuse to resume surfaces WITH leg 1's trailing frames: the
/// close is byte-identical to a stream this layer never touched. Before this
/// fix the loop broke without draining, eating the trailing `[DONE]`.
#[sqlx::test]
async fn a_refused_death_drains_leg_ones_trailer(pool: PgPool) {
    let death = json!({"error": {"code": 499, "message": "client disconnected", "type": "client_disconnected"}});
    let fake = Fake::new(vec![content("chatcmpl-1", "partial"), frame(death), done()], vec![]);
    let tokenizer = render_stub(vec![1], 1, 1).await;
    let st = state(pool, &fake, tokenizer.uri(), test_config());

    let payloads = collect_payloads(app(&fake, st).oneshot(chat_request(streaming_body())).await.unwrap()).await;
    assert!(fake.resume_requests().is_empty(), "client_disconnected is never resumed");
    assert_eq!(
        parsed(&payloads).last().unwrap()["error"]["type"],
        "client_disconnected",
        "the refused frame surfaces"
    );
    assert_eq!(payloads.last().unwrap(), "[DONE]", "the trailing [DONE] still reaches the client");
}

/// Modes 7/8: the generation finished but its trailer was lost. Nothing needs
/// resuming — the missing usage frame is synthesized from a render so the
/// request still bills, and `[DONE]` is supplied.
#[sqlx::test]
async fn a_finished_stream_with_a_lost_trailer_is_completed_not_resumed(pool: PgPool) {
    let finished = frame(json!({
        "id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1_700_000_000, "model": MODEL,
        "choices": [{"index": 0, "delta": {"content": "!"}, "finish_reason": "stop"}]
    }));
    let fake = Fake::new(vec![content("chatcmpl-1", "All done"), finished], vec![]);
    let tokenizer = render_stub(vec![1], 1009, 9).await;
    let st = state(pool, &fake, tokenizer.uri(), test_config());

    let payloads = collect_payloads(app(&fake, st).oneshot(chat_request(streaming_body())).await.unwrap()).await;
    let frames = parsed(&payloads);

    assert!(fake.resume_requests().is_empty(), "a finished generation is not resumed");
    assert_eq!(contents(&frames), "All done!");
    assert_eq!(payloads.last().unwrap(), "[DONE]", "the missing terminator is supplied");
    let usage = usage_frames(&frames);
    assert_eq!(usage.len(), 1, "the lost usage frame is reconstructed from the render");
    assert_eq!(usage[0]["usage"]["prompt_tokens"], 1000);
    assert_eq!(usage[0]["usage"]["completion_tokens"], 9);
}

/// A stream carrying deltas we cannot reconstruct byte-exactly (reasoning, tool
/// calls) disarms: it is forwarded untouched and never resumed. Guessing at the
/// prefix would condition the model on text the model never emitted.
#[sqlx::test]
async fn a_stream_with_unreconstructable_deltas_is_never_resumed(pool: PgPool) {
    let reasoning = frame(json!({
        "id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1_700_000_000, "model": MODEL,
        "choices": [{"index": 0, "delta": {"reasoning_content": "let me think"}, "finish_reason": null}]
    }));
    let fake = Fake::new(
        vec![content("chatcmpl-1", "visible"), reasoning, Chunk::Reset],
        vec![vec![leg_text("!", Some("stop")), leg_usage(1, 1), done()]],
    );
    let tokenizer = render_stub(vec![1], 1007, 7).await;
    let st = state(pool, &fake, tokenizer.uri(), test_config());

    let payloads = collect_payloads(app(&fake, st).oneshot(chat_request(streaming_body())).await.unwrap()).await;
    assert!(fake.resume_requests().is_empty());
    let frames = parsed(&payloads);
    assert_eq!(
        frames[1]["choices"][0]["delta"]["reasoning_content"], "let me think",
        "a disarmed stream is still forwarded byte-for-byte"
    );
}

/// The per-model in-flight cap: during an incident the resume budget is finite,
/// and deaths beyond it surface as plain errors instead of stampeding the
/// continuation provider.
#[sqlx::test]
async fn a_saturated_model_does_not_stampede_the_continuation_target(pool: PgPool) {
    let fake = Fake::new(
        vec![content("chatcmpl-1", "partial"), Chunk::Reset],
        vec![vec![leg_text("!", Some("stop")), leg_usage(1, 1), done()]],
    );
    let tokenizer = render_stub(vec![1], 1007, 7).await;
    let cfg = ContinuationConfig {
        max_inflight_per_model: 0,
        ..test_config()
    };
    let st = state(pool, &fake, tokenizer.uri(), cfg);

    let payloads = collect_payloads(app(&fake, st).oneshot(chat_request(streaming_body())).await.unwrap()).await;
    assert!(fake.resume_requests().is_empty(), "the cap is enforced before any work is done");
    assert_eq!(contents(&parsed(&payloads)), "partial");
}

/// Nobody is listening: when the client drops the response, the chain — which
/// lives entirely inside that response's body stream — goes with it. No resume
/// leg generates tokens into a closed socket.
#[sqlx::test]
async fn a_client_disconnect_cancels_the_chain_before_any_leg_is_dispatched(pool: PgPool) {
    let fake = Fake::new(
        vec![content("chatcmpl-1", "partial"), Chunk::Reset],
        vec![vec![leg_text("!", Some("stop")), leg_usage(1, 1), done()]],
    );
    let tokenizer = render_stub(vec![1], 1007, 7).await;
    let st = state(pool, &fake, tokenizer.uri(), test_config());

    let response = app(&fake, st).oneshot(chat_request(streaming_body())).await.unwrap();
    let mut body = response.into_body().into_data_stream();
    // Read the first frame, then hang up — exactly what a cancelled client does.
    let first = body.next().await.unwrap().unwrap();
    assert!(!first.is_empty());
    drop(body);

    assert!(
        fake.resume_requests().is_empty(),
        "the death is never even reached: the stream that would have driven the resume is gone"
    );
}

// ── eligibility matrix ───────────────────────────────────────────────────────

/// Each gate short-circuits into an untouched pass-through. The assertion in
/// every case is the same and is the one that matters: the dying stream reaches
/// the client unchanged and no resume work is done.
#[sqlx::test]
async fn ineligible_requests_pass_through_untouched(pool: PgPool) {
    let structured = {
        let mut b = streaming_body();
        b["response_format"] = json!({"type": "json_object"});
        b
    };
    let unknown_model = {
        let mut b = streaming_body();
        b["model"] = json!("some-other-model");
        b
    };
    let non_streaming = {
        let mut b = streaming_body();
        b["stream"] = json!(false);
        b
    };

    let cases: Vec<(&str, Value, ContinuationConfig)> = vec![
        ("structured output cannot survive a seam", structured, test_config()),
        ("a model with no continuation route", unknown_model, test_config()),
        ("a non-streaming request has no partial output", non_streaming, test_config()),
        (
            "an origin the operator has not enabled",
            streaming_body(),
            ContinuationConfig {
                origins: crate::config::ContinuationOriginsConfig {
                    realtime: false,
                    batch: false,
                    playground: false,
                },
                ..test_config()
            },
        ),
        (
            "a body too large to retain",
            streaming_body(),
            ContinuationConfig {
                max_buffer_bytes: 4,
                ..test_config()
            },
        ),
        (
            "the global kill switch",
            streaming_body(),
            ContinuationConfig {
                enabled: false,
                ..test_config()
            },
        ),
    ];

    for (name, body, cfg) in cases {
        let fake = Fake::new(
            vec![content("chatcmpl-1", "partial"), Chunk::Reset],
            vec![vec![leg_text("!", Some("stop")), leg_usage(1, 1), done()]],
        );
        let tokenizer = render_stub(vec![1], 1007, 7).await;
        let st = state(pool.clone(), &fake, tokenizer.uri(), cfg);

        let response = app(&fake, st).oneshot(chat_request(body)).await.unwrap();
        let payloads = collect_payloads(response).await;
        assert!(fake.resume_requests().is_empty(), "case: {name} — no resume must be attempted");
        assert_eq!(
            contents(&parsed(&payloads)),
            "partial",
            "case: {name} — the stream is passed through"
        );
    }
}

/// Failures before the first byte are error enrichment's business, not ours:
/// nothing was generated, so there is nothing to continue.
#[sqlx::test]
async fn non_2xx_and_non_streaming_responses_are_left_alone(pool: PgPool) {
    let fake = Fake::new(vec![], vec![]);
    let tokenizer = render_stub(vec![1], 1, 1).await;
    let st = state(pool, &fake, tokenizer.uri(), test_config());

    // An upstream that answers with a plain JSON error rather than a stream.
    let inner = Router::new()
        .route(
            "/chat/completions",
            post(|| async { (StatusCode::BAD_GATEWAY, Json(json!({"error": {"message": "upstream down"}}))) }),
        )
        .layer(middleware::from_fn_with_state(st, continuation_middleware));

    let response = inner.oneshot(chat_request(streaming_body())).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        body["error"]["message"], "upstream down",
        "the response is byte-identical to today's"
    );
    assert!(fake.resume_requests().is_empty());
}

/// Review-sweep item: the resume leg must re-enter at THIS layer's inner
/// service, not at the top of the stack. If the capture point ever moved above
/// outlet or the cache layer, every resumed request would produce a second
/// analytics row, a second billing record and a second cache classify. The
/// counting layer here stands in for those: it must see the customer's request
/// exactly once, however many legs it took to serve it.
#[sqlx::test]
async fn a_resume_leg_never_re_enters_the_layers_above_this_one(pool: PgPool) {
    let fake = Fake::new(
        vec![content("chatcmpl-1", "one "), Chunk::Reset],
        vec![vec![leg_text("two", Some("stop")), leg_usage(1004, 2), done()]],
    );
    let tokenizer = render_stub(vec![1, 2], 1004, 4).await;
    let st = state(pool, &fake, tokenizer.uri(), test_config());

    // Stands in for outlet / the cache layer: everything OUTER to continuation.
    let outer_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&outer_calls);
    let app = app(&fake, st).layer(middleware::from_fn(move |req: Request<Body>, next: axum::middleware::Next| {
        let counter = Arc::clone(&counter);
        async move {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            next.run(req).await
        }
    }));

    let payloads = collect_payloads(app.oneshot(chat_request(streaming_body())).await.unwrap()).await;
    assert_eq!(contents(&parsed(&payloads)), "one two", "the stream was resumed");
    assert_eq!(fake.resume_requests().len(), 1, "a leg was dispatched");
    assert_eq!(
        outer_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the layers above continuation see ONE logical request — no double billing, logging or caching"
    );
}

/// The per-route config on the completions component reaches BOTH of its
/// consumers on a live resume: the render call (which templates the prefix) and
/// the leg body (whose token ids must match what the provider will actually
/// see).
///
/// This is what makes the canary correct: DeepSeek-V4-Flash is served in CHAT
/// mode, while tokenizer-svc renders that family in thinking mode by default.
/// Without the route's kwargs the render would open a `<think>` the leg never
/// had — and the reconstructor would close one the model never opened.
#[sqlx::test]
async fn the_route_config_reaches_the_render_call_and_the_leg_body(pool: PgPool) {
    let fake = Fake::new(
        vec![content("chatcmpl-1", "Hello"), Chunk::Reset],
        vec![vec![leg_text(", world!", Some("stop")), leg_usage(1010, 3), done()]],
    );

    // A render stub that records what it was asked for.
    let rendered = Arc::new(Mutex::new(Vec::<Value>::new()));
    let sink = Arc::clone(&rendered);
    let tokenizer = MockServer::start().await;
    Mock::given(wm_method("POST"))
        .and(wm_path("/v1/render"))
        .respond_with(move |req: &wiremock::Request| {
            sink.lock().unwrap().push(serde_json::from_slice(&req.body).unwrap());
            ResponseTemplate::new(200).set_body_json(json!({
                "virtual_model": MODEL,
                "token_ids": [7, 1, 2, 3],
                "total": 1010,
                "continuation_tokens": 5
            }))
        })
        .mount(&tokenizer)
        .await;

    let mut st = state(pool, &fake, tokenizer.uri(), test_config());
    st.routes = Arc::new(ContinuationRoutes::with_routes([(
        MODEL.to_string(),
        crate::continuation::RouteInfo {
            render_kwargs: Some(json!({"thinking_mode": "chat"})),
            strip_leading_bos: true,
        },
    )]));

    let payloads = collect_payloads(app(&fake, st).oneshot(chat_request(streaming_body())).await.unwrap()).await;
    assert_eq!(contents(&parsed(&payloads)), "Hello, world!", "the stream was resumed");

    // 1. The render call carries the route's kwargs.
    let render_requests = rendered.lock().unwrap().clone();
    assert_eq!(render_requests.len(), 1);
    assert_eq!(
        render_requests[0]["chat_template_kwargs"],
        json!({"thinking_mode": "chat"}),
        "the prefix must be rendered the way the route serves the model"
    );
    assert_eq!(render_requests[0]["continuation_text"], "Hello");

    // 2. The leg's prompt is the rendered prefix VERBATIM — leading token and
    //    all — even though this route says the provider prepends its own BOS.
    //    `strip_leading_bos` is carried, not applied: BOS-prepending is a
    //    property of the MEMBER that ends up serving the leg, and this body is
    //    built once, before onwards picks one out of the completions pool. Its
    //    first member is on-prem and does NOT prepend, so a pre-stripped prompt
    //    would reach dynamo a token short. The strip belongs in onwards'
    //    per-member forwarding; see `RouteInfo::strip_leading_bos`.
    let leg = fake.resume_requests();
    assert_eq!(leg.len(), 1);
    assert_eq!(leg[0].body["prompt"], json!([7, 1, 2, 3]));
}

/// The client's own `chat_template_kwargs` describe how leg 1 was actually
/// templated downstream, so they win over the route's defaults key by key —
/// reproducing leg 1's prompt is the whole objective.
#[sqlx::test]
async fn request_template_kwargs_override_the_route_defaults_on_a_live_resume(pool: PgPool) {
    let fake = Fake::new(
        vec![content("chatcmpl-1", "Hi"), Chunk::Reset],
        vec![vec![leg_text("!", Some("stop")), leg_usage(1002, 1), done()]],
    );
    let rendered = Arc::new(Mutex::new(Vec::<Value>::new()));
    let sink = Arc::clone(&rendered);
    let tokenizer = MockServer::start().await;
    Mock::given(wm_method("POST"))
        .and(wm_path("/v1/render"))
        .respond_with(move |req: &wiremock::Request| {
            sink.lock().unwrap().push(serde_json::from_slice(&req.body).unwrap());
            ResponseTemplate::new(200).set_body_json(json!({
                "token_ids": [4, 5], "total": 1002, "continuation_tokens": 2
            }))
        })
        .mount(&tokenizer)
        .await;

    let mut st = state(pool, &fake, tokenizer.uri(), test_config());
    st.routes = Arc::new(ContinuationRoutes::with_routes([(
        MODEL.to_string(),
        crate::continuation::RouteInfo {
            render_kwargs: Some(json!({"thinking_mode": "chat", "tool_style": "dsml"})),
            strip_leading_bos: false,
        },
    )]));

    let mut body = streaming_body();
    body["chat_template_kwargs"] = json!({"thinking_mode": "thinking"});
    collect_payloads(app(&fake, st).oneshot(chat_request(body)).await.unwrap()).await;

    let render_requests = rendered.lock().unwrap().clone();
    assert_eq!(
        render_requests[0]["chat_template_kwargs"],
        json!({"thinking_mode": "thinking", "tool_style": "dsml"}),
        "the client's value wins; the route's other keys survive"
    );
    // Nothing stripped: the full rendered prefix goes to the provider.
    assert_eq!(fake.resume_requests()[0].body["prompt"], json!([4, 5]));
}
