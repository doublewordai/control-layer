//! Golden equivalence transcripts for the `/ai/v1` inference pipeline.
//!
//! Each test drives one request shape through the FULL real stack (edge
//! middleware, translation, onwards, wiremock upstream) and snapshots a
//! transcript of every observable side effect the middleware pipeline
//! produces:
//!
//! * the **client-visible response** (status + body / SSE text),
//! * the **upstream-bound request** as the wiremock provider received it —
//!   method, path, control headers, and the RAW body string (byte-level, so
//!   key order and field injection are locked),
//! * the **outlet capture** (`outlet.http_requests` / `outlet.http_responses`)
//!   — what request logging persisted,
//! * the **fusillade row** (`fusillade.requests` + `request_templates`) — what
//!   the responses store persisted.
//!
//! ## Purpose
//!
//! These are refactor safety-nets, not behavior specs. The parse-once
//! pipeline refactor (params struct after `inference_middleware`, shared
//! canonical body after translation, single serialize in `outbound_request`)
//! must reproduce these transcripts byte-for-byte, or show an intentional,
//! reviewed diff. Some captured behavior is known-accidental (e.g. client
//! supplied `completion_id`/`id` fields are scrubbed from the STORED body but
//! currently forwarded upstream on the realtime path — see
//! `golden_chat_client_supplied_ids`); the snapshot locks it so a change is an
//! explicit decision instead of a silent side effect.
//!
//! ## Updating snapshots
//!
//! ```sh
//! INSTA_UPDATE=always cargo test golden_ --lib
//! # review the .snap diffs like any other code change
//! ```
//!
//! ## Out of scope (follow-ups)
//!
//! * flex / background-tier daemon dispatch (needs the batch daemon running),
//! * the prompt-cache layer (disabled in the test config; covered separately
//!   by `cache_classifier`),
//! * server-side tool injection (needs seeded tool sources),
//! * non-strict onwards mode (the catch-all path).

use crate::api::models::users::Role;
use crate::test::utils::{add_auth_headers, create_test_admin_user, create_test_config, create_test_user};
use axum_test::TestServer;
use sqlx::PgPool;

const MODEL: &str = "gpt-4o";

/// Everything a corpus case needs to fire requests and capture stores.
struct Fixture {
    server: TestServer,
    mock: wiremock::MockServer,
    pool: PgPool,
    api_key: String,
    _bg: crate::BackgroundServices,
}

/// Build the full app against a wiremock upstream: strict onwards, request
/// logging ON (so outlet rows are captured), response writer flushing every
/// record. Seeds endpoint/model/group/credits/key via the admin API exactly
/// like production provisioning would.
async fn setup(pool: PgPool) -> Fixture {
    let mock = wiremock::MockServer::start().await;

    let mut config = create_test_config();
    config.onwards.strict_mode = true;
    config.background_services.onwards_sync.enabled = true;
    config.enable_request_logging = true;
    // Flush each completed realtime record immediately so fusillade-row
    // polling is deterministic.
    config.background_services.task_workers.response_writer_batch_size = 1;

    let app = crate::Application::new_with_pool(config, Some(pool.clone()), None)
        .await
        .expect("Failed to create application");
    let (server, bg) = app.into_test_server();

    let admin = create_test_admin_user(&pool, Role::PlatformManager).await;
    let h = add_auth_headers(&admin);

    let endpoint: serde_json::Value = server
        .post("/admin/api/v1/endpoints")
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .json(&serde_json::json!({ "name": "golden-upstream", "url": mock.uri(), "auto_sync_models": false }))
        .await
        .json();
    let endpoint_id = endpoint["id"].as_str().expect("endpoint id");

    let model: serde_json::Value = server
        .post("/admin/api/v1/models")
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .json(&serde_json::json!({
            "type": "standard",
            "model_name": MODEL,
            "alias": MODEL,
            "hosted_on": endpoint_id,
            "open_responses_adapter": true
        }))
        .await
        .json();
    let deployment_id = model["id"].as_str().expect("model id");

    // Public group (all-zeros UUID) → model visible to every user.
    let group_id = "00000000-0000-0000-0000-000000000000";
    let assoc = server
        .post(&format!("/admin/api/v1/groups/{group_id}/models/{deployment_id}"))
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .await;
    assert!(assoc.status_code().is_success(), "group assoc failed: {}", assoc.text());

    let user = create_test_user(&pool, Role::StandardUser).await;
    let grant = server
        .post("/admin/api/v1/transactions")
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .json(&serde_json::json!({
            "user_id": user.id,
            "transaction_type": "admin_grant",
            "amount": 1000,
            "source_id": admin.id
        }))
        .await;
    assert!(grant.status_code().is_success(), "credit grant failed: {}", grant.text());

    let key: serde_json::Value = server
        .post(&format!("/admin/api/v1/users/{}/api-keys", user.id))
        .add_header(&h[0].0, &h[0].1)
        .add_header(&h[1].0, &h[1].1)
        .json(&serde_json::json!({ "purpose": "realtime", "name": "golden corpus key" }))
        .await
        .json();
    let api_key = key["key"].as_str().expect("api key").to_string();

    bg.sync_onwards_config(&pool).await.expect("onwards sync");

    Fixture {
        server,
        mock,
        pool,
        api_key,
        _bg: bg,
    }
}

/// POST a body to an AI path, polling through the async onwards-config
/// convergence window (404 = model not yet routable, 403 = key set not yet
/// synced) exactly like `zdr_sentinel::send_sentinel_request`. Returns the
/// first non-403/404 response.
async fn post_until_routable(f: &Fixture, path: &str, body: &serde_json::Value) -> (u16, String) {
    for attempt in 0..200 {
        let resp = f
            .server
            .post(path)
            .add_header("authorization", format!("Bearer {}", f.api_key))
            .json(body)
            .await;
        let status = resp.status_code().as_u16();
        if !matches!(status, 403 | 404) {
            return (status, resp.text());
        }
        assert!(attempt < 199, "model never became routable; last status {status}: {}", resp.text());
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    unreachable!()
}

/// Anthropic-shaped POST (x-api-key + anthropic-version instead of Bearer).
async fn post_anthropic_until_routable(f: &Fixture, body: &serde_json::Value) -> (u16, String) {
    for attempt in 0..200 {
        let resp = f
            .server
            .post("/ai/v1/messages")
            .add_header("x-api-key", &f.api_key)
            .add_header("anthropic-version", "2023-06-01")
            .json(body)
            .await;
        let status = resp.status_code().as_u16();
        if !matches!(status, 403 | 404) {
            return (status, resp.text());
        }
        assert!(attempt < 199, "model never became routable; last status {status}: {}", resp.text());
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    unreachable!()
}

// ─── Normalization ────────────────────────────────────────────────────────────

/// Scrub run-to-run volatility out of a transcript chunk so snapshots are
/// stable: UUIDs, ephemeral ports, epoch + ISO timestamps, and the per-run API
/// key secret. Deliberately does NOT touch structure or key order — byte-level
/// differences in the bodies are exactly what these tests exist to catch.
fn normalize(text: &str, api_key: &str) -> String {
    let uuid = regex::Regex::new(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}").unwrap();
    let port = regex::Regex::new(r"127\.0\.0\.1:\d+").unwrap();
    let iso_ts = regex::Regex::new(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(\.\d+)?([+-]\d{2}:?\d{2}|Z)?").unwrap();
    let epoch_fields =
        regex::Regex::new(r#""(created|created_at|completed_at|failed_at|cancelled_at|timestamp|expires_at)"\s*:\s*\d{9,}"#).unwrap();

    let text = text.replace(api_key, "<api-key>");
    let text = uuid.replace_all(&text, "<uuid>");
    let text = port.replace_all(&text, "127.0.0.1:<port>");
    // Output-item ids come from a process-global counter
    // (`translation/responses/response.rs::generate_item_id`), so their value
    // depends on how many responses this PROCESS translated before — differs
    // between `cargo test` (shared process) and nextest/CI (process per test).
    let item_id = regex::Regex::new(r"item_[0-9a-f]{16}").unwrap();
    let text = iso_ts.replace_all(&text, "<ts>");
    let text = item_id.replace_all(&text, "item_<n>");
    let text = epoch_fields.replace_all(&text, r#""$1":0"#).to_string();
    text
}

// ─── Capture ─────────────────────────────────────────────────────────────────

/// Upstream-facing control headers worth locking: correlation ids the outlet
/// handler reads, the model/endpoint hints, stream + ZDR markers, and auth.
const UPSTREAM_HEADERS: &[&str] = &[
    "authorization",
    "content-type",
    "x-fusillade-request-id",
    "x-onwards-response-id",
    "x-onwards-endpoint",
    "x-onwards-model",
    "x-fusillade-stream",
    "x-fusillade-batch-zdr",
];

/// Render every request the wiremock upstream received: method, path, the
/// header allowlist above, and the RAW body string (preserving byte order).
async fn upstream_transcript(f: &Fixture) -> String {
    let requests = f.mock.received_requests().await.unwrap_or_default();
    let mut out = String::new();
    for (i, req) in requests.iter().enumerate() {
        out.push_str(&format!("--- upstream request {i} ---\n"));
        out.push_str(&format!("{} {}\n", req.method, req.url.path()));
        for name in UPSTREAM_HEADERS {
            match req.headers.get(*name) {
                Some(v) => out.push_str(&format!("{name}: {}\n", v.to_str().unwrap_or("<non-utf8>"))),
                None => out.push_str(&format!("{name}: <absent>\n")),
            }
        }
        let body = String::from_utf8_lossy(&req.body);
        out.push_str(&format!("body: {body}\n"));
    }
    if requests.is_empty() {
        out.push_str("(no upstream requests)\n");
    }
    normalize(&out, &f.api_key)
}

/// Latest matching row of `table` as `to_jsonb` text, with volatile columns
/// stripped. Polls because outlet + the responses writer flush asynchronously.
async fn poll_row(pool: &PgPool, table: &str, needle: &str, strip: &[&str]) -> Option<serde_json::Value> {
    // `needle` is matched against the whole serialized row, so callers plant a
    // unique sentinel in each request body (readiness probes write rows too).
    let query =
        format!("SELECT to_jsonb(t)::text FROM {table} t WHERE to_jsonb(t)::text LIKE '%{needle}%' ORDER BY to_jsonb(t)->>'id' LIMIT 1");
    for _ in 0..200 {
        let row: Option<(String,)> = sqlx::query_as(&query).fetch_optional(pool).await.expect("poll row");
        if let Some((json,)) = row {
            let mut v: serde_json::Value = serde_json::from_str(&json).expect("row json");
            if let Some(obj) = v.as_object_mut() {
                for k in strip {
                    obj.remove(*k);
                }
            }
            return Some(v);
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    None
}

/// Columns that vary per run or carry no pipeline behavior. Everything else in
/// the row is snapshotted, so a schema change also surfaces here (that's
/// intentional — new persisted fields should be a reviewed diff).
const OUTLET_STRIP: &[&str] = &[
    "id",
    "correlation_id",
    "created_at",
    "timestamp",
    "duration_ms",
    "duration_to_first_byte_ms",
    "request_headers",
    "response_headers",
    "headers",
];

async fn outlet_transcript(f: &Fixture, request_needle: &str, response_needle: &str) -> String {
    let req = poll_row(&f.pool, "outlet.http_requests", request_needle, OUTLET_STRIP).await;
    let resp = poll_row(&f.pool, "outlet.http_responses", response_needle, OUTLET_STRIP).await;
    let mut out = String::new();
    out.push_str("--- outlet http_requests ---\n");
    out.push_str(
        &req.map(|v| serde_json::to_string_pretty(&v).unwrap())
            .unwrap_or_else(|| "(no row)".into()),
    );
    out.push_str("\n--- outlet http_responses ---\n");
    out.push_str(
        &resp
            .map(|v| serde_json::to_string_pretty(&v).unwrap())
            .unwrap_or_else(|| "(no row)".into()),
    );
    out.push('\n');
    normalize(&out, &f.api_key)
}

/// The fusillade requests row joined to its template: what the response store
/// persisted for this request. Polls for a TERMINAL state so the async writer
/// has flushed.
async fn fusillade_transcript(f: &Fixture, needle: &str) -> String {
    let query = "SELECT r.state, r.service_tier, t.model, t.method, t.path, t.endpoint, t.body \
         FROM fusillade.requests r JOIN fusillade.request_templates t ON t.id = r.template_id \
         WHERE t.body LIKE $1 AND r.state IN ('completed', 'failed', 'cancelled') \
         LIMIT 1";
    for _ in 0..200 {
        let row: Option<(String, Option<String>, String, String, String, String, String)> = sqlx::query_as(query)
            .bind(format!("%{needle}%"))
            .fetch_optional(&f.pool)
            .await
            .expect("poll fusillade row");
        if let Some((state, tier, model, method, path, endpoint, body)) = row {
            let out = format!(
                "--- fusillade row ---\nstate: {state}\nservice_tier: {}\nmodel: {model}\nmethod: {method}\npath: {path}\nendpoint: {endpoint}\nbody: {body}\n",
                tier.as_deref().unwrap_or("<null>")
            );
            return normalize(&out, &f.api_key);
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    "--- fusillade row ---\n(no terminal row)\n".into()
}

/// Compose the full transcript for one corpus case.
fn transcript(client_status: u16, client_body: &str, upstream: &str, outlet: &str, fusillade: &str, f: &Fixture) -> String {
    format!(
        "=== client ===\nstatus: {client_status}\nbody: {}\n\n=== upstream ===\n{upstream}\n=== outlet ===\n{outlet}\n=== fusillade ===\n{fusillade}",
        normalize(client_body, &f.api_key)
    )
}

// ─── Mock upstream responses ─────────────────────────────────────────────────

fn chat_completion_body() -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-golden", "object": "chat.completion", "created": 1, "model": MODEL,
        "choices": [{ "index": 0, "message": { "role": "assistant", "content": "Hello from the gateway" }, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14 }
    })
}

async fn mount_chat_blocking(mock: &wiremock::MockServer) {
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(chat_completion_body()))
        .mount(mock)
        .await;
}

const CHAT_SSE: &str = concat!(
    "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":1,\"total_tokens\":11}}\n\n",
    "data: [DONE]\n\n",
);

async fn mount_chat_streaming(mock: &wiremock::MockServer) {
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_raw(CHAT_SSE.as_bytes().to_vec(), "text/event-stream"))
        .mount(mock)
        .await;
}

// ─── Corpus cases ────────────────────────────────────────────────────────────

#[sqlx::test]
async fn golden_chat_completions_blocking(pool: PgPool) {
    let f = setup(pool).await;
    mount_chat_blocking(&f.mock).await;

    let sentinel = "golden-chat-blocking-sentinel";
    let (status, body) = post_until_routable(
        &f,
        "/ai/v1/chat/completions",
        &serde_json::json!({ "model": MODEL, "messages": [{ "role": "user", "content": sentinel }] }),
    )
    .await;

    let upstream = upstream_transcript(&f).await;
    let outlet = outlet_transcript(&f, sentinel, "Hello from the gateway").await;
    let fusillade = fusillade_transcript(&f, sentinel).await;
    insta::assert_snapshot!(transcript(status, &body, &upstream, &outlet, &fusillade, &f));
}

#[sqlx::test]
async fn golden_chat_completions_streaming(pool: PgPool) {
    let f = setup(pool).await;
    mount_chat_streaming(&f.mock).await;

    let sentinel = "golden-chat-streaming-sentinel";
    let (status, body) = post_until_routable(
        &f,
        "/ai/v1/chat/completions",
        &serde_json::json!({ "model": MODEL, "stream": true, "messages": [{ "role": "user", "content": sentinel }] }),
    )
    .await;

    // Locks the outbound_request invariant: upstream body must carry the
    // injected `stream_options: {"include_usage": true}`.
    let upstream = upstream_transcript(&f).await;
    let outlet = outlet_transcript(&f, sentinel, "chat.completion.chunk").await;
    let fusillade = fusillade_transcript(&f, sentinel).await;
    insta::assert_snapshot!(transcript(status, &body, &upstream, &outlet, &fusillade, &f));
}

/// Client-supplied completion/response id fields. `scrub_request_id_fields`
/// removes these from the STORED body; whether they reach the upstream on the
/// realtime path is exactly the kind of accidental behavior this corpus
/// exists to lock — the snapshot records the truth, whichever way it falls.
#[sqlx::test]
async fn golden_chat_client_supplied_ids(pool: PgPool) {
    let f = setup(pool).await;
    mount_chat_blocking(&f.mock).await;

    let sentinel = "golden-chat-scrub-sentinel";
    let (status, body) = post_until_routable(
        &f,
        "/ai/v1/chat/completions",
        &serde_json::json!({
            "model": MODEL,
            "id": "client-chosen-id",
            "completion_id": "client-chosen-completion-id",
            "messages": [{ "role": "user", "content": sentinel }]
        }),
    )
    .await;

    let upstream = upstream_transcript(&f).await;
    let outlet = outlet_transcript(&f, sentinel, "Hello from the gateway").await;
    let fusillade = fusillade_transcript(&f, sentinel).await;
    insta::assert_snapshot!(transcript(status, &body, &upstream, &outlet, &fusillade, &f));
}

#[sqlx::test]
async fn golden_responses_blocking(pool: PgPool) {
    let f = setup(pool).await;
    mount_chat_blocking(&f.mock).await;

    let sentinel = "golden-responses-blocking-sentinel";
    let (status, body) = post_until_routable(&f, "/ai/v1/responses", &serde_json::json!({ "model": MODEL, "input": sentinel })).await;

    let upstream = upstream_transcript(&f).await;
    let outlet = outlet_transcript(&f, sentinel, "Hello from the gateway").await;
    let fusillade = fusillade_transcript(&f, sentinel).await;
    insta::assert_snapshot!(transcript(status, &body, &upstream, &outlet, &fusillade, &f));
}

#[sqlx::test]
async fn golden_responses_streaming(pool: PgPool) {
    let f = setup(pool).await;
    mount_chat_streaming(&f.mock).await;

    let sentinel = "golden-responses-streaming-sentinel";
    let (status, body) = post_until_routable(
        &f,
        "/ai/v1/responses",
        &serde_json::json!({ "model": MODEL, "stream": true, "input": sentinel }),
    )
    .await;

    let upstream = upstream_transcript(&f).await;
    let outlet = outlet_transcript(&f, sentinel, "response").await;
    let fusillade = fusillade_transcript(&f, sentinel).await;
    insta::assert_snapshot!(transcript(status, &body, &upstream, &outlet, &fusillade, &f));
}

/// `background: true` on `/responses`: 202 immediately, then poll
/// `GET /ai/v1/responses/{id}` to the terminal object. Locks the async-tier
/// row lifecycle end to end.
#[sqlx::test]
async fn golden_responses_background(pool: PgPool) {
    let f = setup(pool).await;
    mount_chat_blocking(&f.mock).await;

    let sentinel = "golden-responses-background-sentinel";
    let (status, accepted_body) = post_until_routable(
        &f,
        "/ai/v1/responses",
        &serde_json::json!({ "model": MODEL, "background": true, "input": sentinel }),
    )
    .await;
    assert_eq!(status, 202, "background submit should 202: {accepted_body}");

    let accepted: serde_json::Value = serde_json::from_str(&accepted_body).expect("202 body json");
    let resp_id = accepted["id"].as_str().expect("resp id").to_string();

    // Poll the retrieval surface to the terminal object.
    let mut final_body = String::new();
    for _ in 0..200 {
        let r = f
            .server
            .get(&format!("/ai/v1/responses/{resp_id}"))
            .add_header("authorization", format!("Bearer {}", f.api_key))
            .await;
        let text = r.text();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        if matches!(v["status"].as_str(), Some("completed") | Some("failed")) {
            final_body = text;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(!final_body.is_empty(), "background response never reached a terminal status");

    let upstream = upstream_transcript(&f).await;
    let outlet = outlet_transcript(&f, sentinel, "Hello from the gateway").await;
    let fusillade = fusillade_transcript(&f, sentinel).await;
    let combined = format!("--- 202 accepted ---\n{accepted_body}\n--- final GET ---\n{final_body}");
    insta::assert_snapshot!(transcript(status, &combined, &upstream, &outlet, &fusillade, &f));
}

#[sqlx::test]
async fn golden_anthropic_messages_blocking(pool: PgPool) {
    let f = setup(pool).await;
    mount_chat_blocking(&f.mock).await;

    let sentinel = "golden-anthropic-blocking-sentinel";
    let (status, body) = post_anthropic_until_routable(
        &f,
        &serde_json::json!({
            "model": MODEL,
            "max_tokens": 64,
            "system": "be terse",
            "messages": [{ "role": "user", "content": sentinel }]
        }),
    )
    .await;

    let upstream = upstream_transcript(&f).await;
    let outlet = outlet_transcript(&f, sentinel, "Hello from the gateway").await;
    let fusillade = fusillade_transcript(&f, sentinel).await;
    insta::assert_snapshot!(transcript(status, &body, &upstream, &outlet, &fusillade, &f));
}

#[sqlx::test]
async fn golden_anthropic_messages_streaming(pool: PgPool) {
    let f = setup(pool).await;
    mount_chat_streaming(&f.mock).await;

    let sentinel = "golden-anthropic-streaming-sentinel";
    let (status, body) = post_anthropic_until_routable(
        &f,
        &serde_json::json!({
            "model": MODEL,
            "max_tokens": 64,
            "stream": true,
            "messages": [{ "role": "user", "content": sentinel }]
        }),
    )
    .await;

    let upstream = upstream_transcript(&f).await;
    let outlet = outlet_transcript(&f, sentinel, "message_start").await;
    let fusillade = fusillade_transcript(&f, sentinel).await;
    insta::assert_snapshot!(transcript(status, &body, &upstream, &outlet, &fusillade, &f));
}

const COMPLETIONS_SSE: &str = concat!(
    "data: {\"id\":\"cmpl-1\",\"object\":\"text_completion\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[{\"text\":\"Hello\",\"index\":0,\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"cmpl-1\",\"object\":\"text_completion\",\"created\":1,\"model\":\"gpt-4o\",\"choices\":[{\"text\":\"\",\"index\":0,\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1,\"total_tokens\":3}}\n\n",
    "data: [DONE]\n\n",
);

/// Legacy `POST /completions` is NOT intercepted by the inference middleware
/// (`should_intercept` misses it), but `outbound_request` still matches its
/// path and injects streaming usage flags. This case locks both facts:
/// the upstream body must show `stream_options`, and — proven deterministically
/// by waiting for a LATER chat request's fusillade row (the writer channel is
/// FIFO) — no fusillade row may exist for the legacy request.
#[sqlx::test]
async fn golden_legacy_completions_streaming(pool: PgPool) {
    let f = setup(pool).await;
    // Path-discriminated mocks: the legacy request reaches the upstream on
    // /completions, the barrier chat request on /chat/completions.
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_raw(COMPLETIONS_SSE.as_bytes().to_vec(), "text/event-stream"))
        .mount(&f.mock)
        .await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/chat/completions"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_raw(CHAT_SSE.as_bytes().to_vec(), "text/event-stream"))
        .mount(&f.mock)
        .await;

    let legacy_sentinel = "golden-legacy-completions-sentinel";
    let (status, body) = post_until_routable(
        &f,
        "/ai/v1/completions",
        &serde_json::json!({ "model": MODEL, "stream": true, "prompt": legacy_sentinel }),
    )
    .await;

    // Barrier request: once ITS row is flushed, the FIFO writer has processed
    // everything enqueued before it — so legacy-row absence is meaningful.
    let barrier_sentinel = "golden-legacy-barrier-sentinel";
    let (barrier_status, _) = post_until_routable(
        &f,
        "/ai/v1/chat/completions",
        &serde_json::json!({ "model": MODEL, "stream": true, "messages": [{ "role": "user", "content": barrier_sentinel }] }),
    )
    .await;
    assert_eq!(barrier_status, 200);
    let _barrier_row = fusillade_transcript(&f, barrier_sentinel).await;

    let legacy_row: Option<(String,)> = sqlx::query_as("SELECT t.body FROM fusillade.request_templates t WHERE t.body LIKE $1 LIMIT 1")
        .bind(format!("%{legacy_sentinel}%"))
        .fetch_optional(&f.pool)
        .await
        .expect("legacy row query");

    let upstream = upstream_transcript(&f).await;
    let outlet = outlet_transcript(&f, legacy_sentinel, "text_completion").await;
    let fusillade = format!(
        "--- fusillade row (legacy /completions) ---\n{}\n",
        match legacy_row {
            Some((b,)) => format!("UNEXPECTED ROW: {b}"),
            None => "(no row — legacy /completions is not intercepted)".to_string(),
        }
    );
    insta::assert_snapshot!(transcript(status, &body, &upstream, &outlet, &fusillade, &f));
}

#[sqlx::test]
async fn golden_embeddings_blocking(pool: PgPool) {
    let f = setup(pool).await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "list",
            "data": [{ "object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3] }],
            "model": MODEL,
            "usage": { "prompt_tokens": 3, "total_tokens": 3 }
        })))
        .mount(&f.mock)
        .await;

    let sentinel = "golden-embeddings-sentinel";
    let (status, body) = post_until_routable(&f, "/ai/v1/embeddings", &serde_json::json!({ "model": MODEL, "input": sentinel })).await;

    let upstream = upstream_transcript(&f).await;
    let outlet = outlet_transcript(&f, sentinel, "embedding").await;
    let fusillade = fusillade_transcript(&f, sentinel).await;
    insta::assert_snapshot!(transcript(status, &body, &upstream, &outlet, &fusillade, &f));
}
