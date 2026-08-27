//! The resume middleware: eligibility, request-context capture, and the response
//! tee loop that turns a dead stream into a live one.
//!
//! **Stack position** (see `build_router`): between the cache layer and error
//! enrichment, i.e. outer→inner `translation → inference → outlet → cache →
//! continuation → error_enrichment → image_normalizer → onwards`. Three
//! consequences the implementation leans on:
//!
//! - outlet and the cache layer see ONE logical stream with ONE terminal usage
//!   frame — the merged one we emit — so neither needs to know resume exists;
//! - the cache layer injects its `cache_*` fields into whatever terminal usage we
//!   emit, so our merged frame stays minimal and never touches cache fields;
//! - a resume leg dispatched into [`ContinuationState::resume_target`] (the
//!   router clone captured at exactly this point) re-enters BELOW outlet and the
//!   cache, so it produces no second analytics row and no second classify. That
//!   capture point is the whole reason this layer sits where it does.
//!
//! **The tee loop.** Frames from leg 1 are forwarded to the client BYTE-FOR-BYTE
//! (never re-serialized: a typed round-trip silently drops unknown fields — the
//! lesson from `http_responses.body`) while a copy of the generation accumulates
//! in the model's [`StreamAccumulator`]. When the stream dies, [`detect::classify`] decides
//! whether it is resumable; if so the chain loop in [`super::resume`] renders the
//! prefix, dispatches a `/v1/completions` leg on the global continuation key, and
//! its chunks are reframed onto the client's original envelope. The client sees
//! one uninterrupted stream with a small gap.
//!
//! Everything — including every resume leg — runs inside the response body
//! stream. If the client disconnects, that stream is dropped, which cancels the
//! resume in flight and releases the per-model slot. There is no path by which a
//! resume leg outlives the connection it exists to serve.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::StreamExt;
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::PgPool;
use tracing::{debug, warn};

use crate::config::ContinuationConfig;
use crate::db::models::api_keys::ApiKeyPurpose;
use crate::inference::store::ONWARDS_RESPONSE_ID_HEADER;
use crate::prompt_cache::sse::SseBufferedStream;

use super::accumulate::{self, StreamAccumulator};
use super::detect::{self, DeathEvent, Verdict};
use super::forward::ForwardParser;
use super::metrics;
use super::render::{RenderClient, RenderPrefix, RenderedPrefix};
use super::resume::{self, LegError, LegStream};
use super::rewrap::{self, DONE_FRAME};
use super::{ContinuationRoutes, InflightGuard, InflightLimiter, PurposeResolver, RouteInfo};

/// Everything the middleware needs at request time. Cloned per request (all
/// fields are cheap handles), built once in `build_router` behind
/// `continuation.enabled`.
#[derive(Clone)]
pub struct ContinuationState {
    pub cfg: Arc<ContinuationConfig>,
    /// Secret of the global hidden `continuation` key. Resume legs authenticate
    /// with it; its purpose is what lets them carry a scheduling `priority`
    /// (onwards strips that field from every other caller). Routing is by
    /// request path, not by purpose.
    pub key_secret: Arc<str>,
    pub tokenizer: RenderClient,
    /// The router clone taken at this layer's own insertion point — the resume
    /// leg's entry into the stack. See the module docs.
    pub resume_target: Router,
    pub routes: Arc<ContinuationRoutes>,
    pub purposes: PurposeResolver,
    pub inflight: Arc<InflightLimiter>,
    /// Bound for buffering the request body, set to the same limit onwards
    /// enforces so this layer is never more restrictive than the entry point.
    pub body_limit: usize,
}

impl ContinuationState {
    /// Build the state and start the route poller.
    ///
    /// `tokenizer_url` is `continuation.tokenizer_url` falling back to
    /// `cache.tokenizer_url` — the same service serves both layers.
    pub async fn build(
        cfg: &ContinuationConfig,
        cache_tokenizer_url: &str,
        pool: PgPool,
        resume_target: Router,
        body_limit: usize,
    ) -> anyhow::Result<Self> {
        let key_secret = super::provision_global_key(&pool).await?;
        let tokenizer_url = cfg.tokenizer_url.clone().unwrap_or_else(|| cache_tokenizer_url.to_string());
        let routes = Arc::new(ContinuationRoutes::new());
        // Seed synchronously so the first request after boot sees the real set
        // rather than waiting up to one poll interval.
        if let Err(e) = routes.refresh(&pool).await {
            crate::background_error!(
                crate::metrics::errors::component::CONTINUATION,
                "route_seed",
                Warning,
                error = %e,
                "Initial continuation route load failed; the poller will retry"
            );
        }
        Arc::clone(&routes).spawn_poller(pool.clone());

        Ok(Self {
            cfg: Arc::new(cfg.clone()),
            key_secret: Arc::from(key_secret),
            tokenizer: RenderClient::new(tokenizer_url, Duration::from_secs(cfg.resume_deadline_secs)),
            resume_target,
            routes,
            purposes: PurposeResolver::new(pool),
            inflight: Arc::new(InflightLimiter::new(cfg.max_inflight_per_model)),
            body_limit,
        })
    }

    fn stall_timeout(&self) -> Duration {
        Duration::from_secs(self.cfg.stall_timeout_secs)
    }
}

/// The retained request: everything a resume leg needs to rebuild the prompt.
///
/// Held for the life of the stream, bounded by `continuation.max_buffer_bytes`
/// (checked before arming). This is not a new class of memory use — outlet
/// already buffers every request and response body to write `http_requests`.
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub model: String,
    /// The parsed request body as the client sent it.
    pub body: Value,
    /// The path as THIS layer sees it (post-nesting), from which the resume
    /// leg's `/completions` path is derived.
    pub path: String,
    /// The inference layer's response id, when present — correlation only.
    pub response_id: Option<String>,
    /// This model's continuation route config, captured when the stream was
    /// armed. Held on the context rather than re-read per leg so a mid-stream
    /// config change cannot make leg 2 render differently from leg 1.
    pub route: RouteInfo,
    /// Metric label for the request origin (realtime / batch / playground),
    /// resolved at arm time from the API key purpose.
    pub origin: &'static str,
}

impl RequestContext {
    pub fn messages(&self) -> &Value {
        self.body.get("messages").unwrap_or(&Value::Null)
    }

    pub fn tools(&self) -> Option<&Value> {
        self.body.get("tools").filter(|v| !v.is_null())
    }

    pub fn chat_template_kwargs(&self) -> Option<&Value> {
        self.body.get("chat_template_kwargs").filter(|v| !v.is_null())
    }

    /// The `chat_template_kwargs` a render for this stream must use: the route's
    /// serving mode, overlaid with whatever the client asked for (see
    /// [`RouteInfo::merged_render_kwargs`]).
    pub fn render_kwargs(&self) -> Option<Value> {
        self.route.merged_render_kwargs(self.chat_template_kwargs())
    }

    pub fn max_tokens(&self) -> Option<u32> {
        // `max_completion_tokens` is the newer OpenAI spelling; both cap the
        // generation, so both must be decremented on a resume leg.
        self.body
            .get("max_tokens")
            .or_else(|| self.body.get("max_completion_tokens"))
            .and_then(Value::as_u64)
            .map(|v| u32::try_from(v).unwrap_or(u32::MAX))
    }
}

/// Request origin, derived exactly as analytics derives `request_origin`: from
/// the API key's purpose. Batch traffic reaches this layer through fusillade's
/// loopback on a hidden `batch` key, so no header sniffing is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Realtime,
    Batch,
    Playground,
}

impl Origin {
    fn from_purpose(purpose: Option<ApiKeyPurpose>) -> Self {
        match purpose {
            Some(ApiKeyPurpose::Batch) => Origin::Batch,
            Some(ApiKeyPurpose::Playground) => Origin::Playground,
            _ => Origin::Realtime,
        }
    }

    /// Bounded metric label.
    fn label(self) -> &'static str {
        match self {
            Origin::Realtime => "realtime",
            Origin::Batch => "batch",
            Origin::Playground => "playground",
        }
    }

    fn is_enabled(self, cfg: &ContinuationConfig) -> bool {
        match self {
            Origin::Realtime => cfg.origins.realtime,
            Origin::Batch => cfg.origins.batch,
            Origin::Playground => cfg.origins.playground,
        }
    }
}

/// Structured output cannot survive a seam: the grammar/FSM state lives in the
/// engine that was generating, and a fresh completions leg has no way to
/// reconstruct it — it would emit free text into a half-written JSON document.
/// `-dottxt` aliases are structured by construction.
fn is_structured_output(body: &Value, model: &str) -> bool {
    const KEYS: [&str; 6] = [
        "response_format",
        "json_schema",
        "guided_json",
        "guided_regex",
        "guided_grammar",
        "guided_choice",
    ];
    KEYS.iter().any(|k| body.get(*k).is_some_and(|v| !v.is_null())) || model.ends_with("-dottxt")
}

fn is_chat_completions(req: &Request) -> bool {
    req.method() == Method::POST && req.uri().path().ends_with("/chat/completions")
}

fn is_sse(response: &Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|ct| ct.trim().eq_ignore_ascii_case("text/event-stream"))
}

fn bearer(req: &Request) -> Option<String> {
    req.headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
        .map(|t| t.trim().to_string())
}

/// The middleware. Anything that fails a gate is forwarded untouched — this
/// layer never changes the request it decides not to arm.
pub async fn continuation_middleware(State(state): State<ContinuationState>, request: Request, next: Next) -> Response {
    // Gate 0 — the global kill switch. `build_router` does not add this layer at
    // all when continuation is disabled, so this is belt-and-braces (and the
    // hook the off-switch test drives).
    if !state.cfg.enabled {
        return next.run(request).await;
    }

    // Gate 1 — route shape. No metric: embeddings/responses traffic would swamp
    // the counter with a reason nobody will ever query.
    if !is_chat_completions(&request) {
        return next.run(request).await;
    }

    let path = request.uri().path().to_string();
    let response_id = request
        .headers()
        .get(ONWARDS_RESPONSE_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let token = bearer(&request);

    let (parts, body) = request.into_parts();
    let body_bytes = match axum::body::to_bytes(body, state.body_limit).await {
        Ok(b) => b,
        Err(e) => {
            // The body is consumed and cannot be forwarded; answer with the same
            // structured 400 the cache and image layers use for this case.
            warn!(error = %e, "Failed to read request body in continuation middleware");
            let body = serde_json::json!({
                "error": {
                    "message": format!("failed to read request body: {e}"),
                    "type": "invalid_request_error",
                    "code": "body_read_failed",
                }
            });
            return (StatusCode::BAD_REQUEST, axum::Json(body)).into_response();
        }
    };

    let forward = |parts, bytes: Bytes| Request::from_parts(parts, Body::from(bytes));
    let parsed = serde_json::from_slice::<Value>(&body_bytes).ok();

    // Gate 2 — streaming only. Non-streaming resume is out of scope: there is no
    // partial output to continue from. No metric (ordinary traffic shape).
    //
    // Batch loopback bodies are NOT streaming yet at this layer: fusillade
    // marks stream-intent with the `x-fusillade-stream` header and the
    // outbound middleware (below us) forces `stream: true` into the body
    // afterwards. Honor the header here or batch-origin streams — the largest
    // death population (gemma-4-31B: ~2.8k mid-stream batch deaths/week vs
    // 0-2 realtime) — could never arm.
    let fusillade_stream = parts
        .headers
        .get("x-fusillade-stream")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let Some(body) = parsed.filter(|b| fusillade_stream || b.get("stream").and_then(Value::as_bool) == Some(true)) else {
        return next.run(forward(parts, body_bytes)).await;
    };
    let Some(model) = body.get("model").and_then(Value::as_str).map(str::to_string) else {
        return next.run(forward(parts, body_bytes)).await;
    };

    // Gates 3-6, cheapest first. NOTE on ordering vs the spec, which lists the
    // origin gate before the route and structured-output gates on the premise
    // that the key purpose is "available post-auth": it is not, at this stack
    // position — auth happens in onwards, BELOW us, so the purpose costs a
    // (memoised) key lookup. The in-memory gates therefore run first, per the
    // spec's own stated cheapest-first principle. Only the recorded `reason`
    // label differs when several gates would fail.
    let Some(route) = state.routes.get(&model) else {
        metrics::record_outcome("ineligible", "no_route", "unknown");
        return next.run(forward(parts, body_bytes)).await;
    };
    if is_structured_output(&body, &model) {
        metrics::record_outcome("ineligible", "structured_output", "unknown");
        return next.run(forward(parts, body_bytes)).await;
    }
    // `logprobs` never arms: a resume leg's chunks carry no chat-shaped
    // logprobs, so a rescued stream would violate the response shape the
    // client asked for from the seam onward.
    if body.get("logprobs").and_then(Value::as_bool) == Some(true) {
        metrics::record_outcome("ineligible", "logprobs", "unknown");
        return next.run(forward(parts, body_bytes)).await;
    }
    // `n > 1` never arms: providers stream multiple choices as alternating
    // single-choice chunks, which the accumulator would concatenate into one
    // interleaved prefix. The accumulator's own `choice.index != 0` disarm is
    // the backstop for providers that emit extra choices unasked.
    if body.get("n").and_then(Value::as_u64).is_some_and(|n| n > 1) {
        metrics::record_outcome("ineligible", "multi_choice", "unknown");
        return next.run(forward(parts, body_bytes)).await;
    }
    if body_bytes.len() > state.cfg.max_buffer_bytes {
        // We would have to retain this body for the life of the stream.
        metrics::record_outcome("ineligible", "cap_exceeded", "unknown");
        return next.run(forward(parts, body_bytes)).await;
    }

    let mut ctx = RequestContext {
        model,
        body,
        path,
        response_id,
        route,
        origin: "unknown",
    };

    let response = next.run(forward(parts, body_bytes)).await;

    // Non-2xx and non-SSE responses pass through untouched: nothing was
    // generated, so there is nothing to continue, and error enrichment above
    // already gives those their final shape.
    if !response.status().is_success() {
        metrics::record_outcome("disarmed", "non_2xx", "unknown");
        return response;
    }
    if !is_sse(&response) {
        metrics::record_outcome("disarmed", "not_streaming", "unknown");
        return response;
    }

    // The origin gate runs AFTER the response is known good: a 2xx from
    // onwards means the bearer was AUTHENTICATED, so the (memoised) purpose
    // lookup only ever runs for valid keys — an unauthenticated caller
    // rotating garbage tokens gets its 401 from onwards without ever costing
    // this layer a database query.
    let origin = Origin::from_purpose(match token.as_deref() {
        Some(t) => state.purposes.resolve(t).await,
        None => None,
    });
    if !origin.is_enabled(&state.cfg) {
        metrics::record_outcome("ineligible", "origin_disabled", origin.label());
        return response;
    }
    ctx.origin = origin.label();
    metrics::record_eligible_stream(&ctx.model, ctx.origin);

    tee(response, state, ctx)
}

/// Records exactly one terminal outcome per armed stream, and — if the stream is
/// dropped before reaching any terminal state — attributes that to a client
/// disconnect. Without this, disconnects would be silently missing from the
/// outcome metric, which is exactly the population we most need to see (they are
/// the streams whose resume work would have been wasted).
struct OutcomeGuard {
    recorded: bool,
    origin: &'static str,
}

impl OutcomeGuard {
    fn new(origin: &'static str) -> Self {
        Self { recorded: false, origin }
    }

    fn record(&mut self, outcome: &'static str, reason: &'static str) {
        if !self.recorded {
            self.recorded = true;
            metrics::record_outcome(outcome, reason, self.origin);
        }
    }

    /// The stream ended without any resume having been needed.
    fn defuse(&mut self) {
        self.recorded = true;
    }
}

impl Drop for OutcomeGuard {
    fn drop(&mut self) {
        if !self.recorded {
            metrics::record_outcome("disarmed", "client_disconnect", self.origin);
        }
    }
}

/// The `data:` payload of one complete SSE event, if it has one.
fn frame_payload(event: &[u8]) -> Option<&str> {
    event.split(|b| *b == b'\n').find_map(|line| {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let rest = line.strip_prefix(b"data:")?;
        std::str::from_utf8(rest).ok().map(str::trim)
    })
}

/// Wrap the response body in the tee + resume loop.
fn tee(response: Response, state: ContinuationState, ctx: RequestContext) -> Response {
    let (parts, body) = response.into_parts();
    // Normalise the body error to io::Error, then reassemble SSE events so we
    // only ever inspect (and the client only ever receives) complete frames — a
    // torn final frame is discarded, which is safe precisely because the client
    // never saw it either.
    let leg_one = BodyExt::into_data_stream(body).map(|r| r.map_err(std::io::Error::other));
    let stall = state.stall_timeout();

    let stream = async_stream::stream! {
        let mut outcome = OutcomeGuard::new(ctx.origin);
        // Which reconstructor this stream gets is a per-model capability lookup;
        // see `accumulate::for_model`.
        let mut acc: Box<dyn StreamAccumulator> = accumulate::for_model(&ctx.model, &state.cfg, &ctx.route);
        let mut current: LegStream = Box::pin(SseBufferedStream::new(leg_one));
        // Is `current` a resume leg (text_completion chunks needing reframing)?
        let mut resuming = false;
        // Which upstream serves the CURRENT leg ("dynamo" / "external"),
        // detected from the leg's first frame (dynamo ids are `dyn-*`). Reset
        // per leg; used to label seams and leg-token metrics so dashboards can
        // split the free dynamo hop from paid external rescues.
        let mut leg_provider: Option<&'static str> = None;
        // The current resume leg's first-frame deadline (the attempt budget).
        // Some(_) until the leg's first frame arrives, then None — reads fall
        // through to the inter-frame stall timer.
        let mut leg_deadline: Option<tokio::time::Instant> = None;
        // The current leg's forward parser: raw generated text back into the
        // chat deltas the client expects. Built from the accumulator when a leg
        // starts, so it is seeded with the structure that was open at the death
        // point, and rebuilt per leg because each leg picks up somewhere new.
        let mut parser: Option<Box<dyn ForwardParser>> = None;
        let mut saw_usage = false;
        // The terminating frames, held rather than forwarded: we may need to put
        // a synthesized usage frame in front of `[DONE]`, and a death frame must
        // only reach the client if the resume chain fails. `first_death` is what
        // the client would have received had this layer not existed, so it — not
        // a later leg's error — is what an exhausted chain surfaces.
        let mut done_bytes: Option<Bytes> = None;
        let mut first_death: Option<Bytes> = None;
        // The FIRST death's family label — what a rescued stream was rescued
        // FROM. `resumed` outcomes carry it as their reason so dashboards can
        // split catches by fault type; `ok` never appears there (a defused
        // stream records nothing).
        let mut first_death_reason: Option<&'static str> = None;
        let mut refusal: Option<Bytes> = None;
        let mut attempts = 0u32;
        let mut last_render: Option<RenderedPrefix> = None;
        let mut death_at: Option<Instant> = None;
        // Inter-frame gap observation (armed streams): the empirical basis for
        // stall_timeout_secs. Every yielded event — keep-alives included —
        // counts as liveness, mirroring what the stall timer itself sees.
        let mut last_frame_at = Instant::now();
        let mut max_frame_gap = Duration::ZERO;
        // Held for the whole chain: one in-flight resume per stream.
        let mut _slot: Option<InflightGuard> = None;

        'chain: loop {
            let verdict = loop {
                // The stall timer arms only once there is something to resume
                // FROM — leg 1 has accumulated generated text. Pre-first-token
                // silence is admission-queue/prefill time, which this layer must
                // never bound: the platform has no first-token deadline, and
                // severing there would fabricate an empty success out of a
                // healthy slow stream. Keyed on accumulated bytes rather than
                // "any frame seen" because engines differ on whether they emit
                // a content-less `role` preamble before or after the first
                // token; a preamble is not a generation. (A disarmed accumulator
                // is empty, so a disarmed stream is never severed either —
                // exactly the pre-continuation passthrough.) Resume legs are
                // timed from their first read: their time-to-first-token IS
                // the seam the deadline exists to bound.
                let finished = acc.saw_finish_reason() || saw_usage;
                let next_item = if let Some(deadline) = leg_deadline {
                    // A resume leg that has not yet produced a frame is still
                    // spending its attempt budget: headers are not a token.
                    match tokio::time::timeout_at(deadline, current.next()).await {
                        Err(_) => break detect::classify(&DeathEvent::Stall { finished }),
                        Ok(item) => item,
                    }
                } else if resuming || acc.len_bytes() > 0 {
                    match tokio::time::timeout(stall, current.next()).await {
                        Err(_) => break detect::classify(&DeathEvent::Stall { finished }),
                        Ok(item) => item,
                    }
                } else {
                    current.next().await
                };
                let item = match next_item {
                    // `saw_done: false` is not an assumption: a `[DONE]` frame
                    // breaks this loop the moment it arrives, so reaching EOF
                    // here means the trailer never came. A terminal usage frame
                    // counts as "finished" alongside `finish_reason`: a
                    // generation that has already reported its accounting is
                    // over, and resuming past it would emit a second usage frame
                    // — the one thing outlet and the cache layer must never see.
                    None => break detect::classify(&DeathEvent::Eof {
                        saw_finish_reason: acc.saw_finish_reason() || saw_usage,
                        saw_done: false,
                    }),
                    Some(Err(_)) => break detect::classify(&DeathEvent::TransportError { finished }),
                    Some(Ok(bytes)) => bytes,
                };

                max_frame_gap = max_frame_gap.max(last_frame_at.elapsed());
                last_frame_at = Instant::now();

                let payload = frame_payload(&item);
                if payload == Some("[DONE]") {
                    done_bytes = Some(item);
                    break detect::classify(&DeathEvent::Eof {
                        saw_finish_reason: acc.saw_finish_reason() || saw_usage,
                        saw_done: true,
                    });
                }

                let Some(value) = payload.and_then(|p| serde_json::from_str::<Value>(p).ok()) else {
                    // A comment, keep-alive or otherwise unparseable event. On leg 1
                    // it is part of the client's stream and is forwarded verbatim; on
                    // a resume leg it belongs to that leg's own framing, so drop it.
                    if !resuming {
                        // A `data:` frame we could not parse is DIFFERENT from a
                        // comment or keep-alive: the client received content the
                        // accumulator did not, so our reconstruction of "what has
                        // been said so far" is now incomplete. Resuming from it
                        // would silently drop whatever that frame carried, so
                        // disarm — the stream is still forwarded byte for byte,
                        // it simply can no longer be saved.
                        if payload.is_some() && acc.disarmed().is_none() {
                            let cause = crate::continuation::accumulate::AccumulateError::UnparseableFrame;
                            acc.disarm_externally(cause);
                            outcome.record("disarmed", cause.reason());
                        }
                        // The one annotated yield: it fixes the stream's item type.
                        yield Ok::<Bytes, std::io::Error>(item);
                    }
                    continue;
                };

                match detect::classify(&DeathEvent::Frame(&value)) {
                    Verdict::Alive => {}
                    other => {
                        if first_death.is_none() {
                            first_death = Some(item.clone());
                        }
                        refusal = Some(item);
                        break other;
                    }
                }

                if !resuming {
                    if acc.disarmed().is_none() && let Err(e) = acc.ingest(&value) {
                        // Armed, then made non-reconstructable. Keep forwarding
                        // the stream untouched; it simply can no longer be saved.
                        outcome.record("disarmed", e.reason());
                    }
                    saw_usage |= rewrap::usage_of(&value).is_some();
                    // Byte passthrough — never a re-serialization.
                    yield Ok(item);
                    continue;
                }

                // ── resume leg: reframe onto the client's original stream ──
                if leg_provider.is_none() {
                    leg_deadline = None;
                    let provider = if value.get("id").and_then(Value::as_str).is_some_and(|id| id.starts_with("dyn-")) {
                        "dynamo"
                    } else {
                        "external"
                    };
                    leg_provider = Some(provider);
                    metrics::record_leg_served(
                        &ctx.model,
                        provider,
                        last_render.as_ref().map(|r| r.total as u64).unwrap_or(0),
                    );
                }
                let Some(env) = acc.envelope().cloned() else {
                    // Leg 1 never produced a usable envelope; without one we
                    // cannot address the client's stream. Should be unreachable
                    // (we only resume after content arrived).
                    continue;
                };
                if let Some((leg_prompt, leg_completion)) = rewrap::usage_of(&value) {
                    // The leg's terminal usage describes the LEG. Replace it with
                    // the merged accounting for the whole logical request.
                    let render = last_render.as_ref();
                    let seg = render.and_then(|r| r.continuation_tokens).unwrap_or(0) as u64;
                    let total = render.map(|r| r.total).unwrap_or(0) as u64;
                    let (merged, anomaly) = rewrap::merge_usage(seg, leg_prompt, leg_completion, total);
                    if let Some(a) = anomaly {
                        metrics::record_usage_anomaly(a.kind());
                    }
                    saw_usage = true;
                    yield Ok(rewrap::sse_frame(&rewrap::usage_frame(&env, merged)));
                    continue;
                }
                // The leg's text is the model's RAW sequence, so it goes through
                // the parser rather than straight into `delta.content`: one
                // completions chunk yields 0..n chat chunks (reasoning, content
                // and tool-call deltas), each on the client's own envelope. A
                // `finish_reason` flushes the parser's held-back bytes first, so
                // nothing a split tag was hiding is lost at the end of the leg.
                let Some((text, finish_reason)) = rewrap::completion_parts(&value) else {
                    continue;
                };
                let deltas = match parser.as_mut() {
                    Some(p) => {
                        let mut deltas = p.feed(text);
                        if !finish_reason.is_null() {
                            deltas.extend(p.finish());
                        }
                        deltas
                    }
                    // Unreachable: `resuming` is only ever set alongside a parser.
                    None => continue,
                };

                let last = deltas.len().saturating_sub(1);
                let mut frames: Vec<Value> = deltas
                    .into_iter()
                    .enumerate()
                    .map(|(i, delta)| {
                        // Only the final frame of a chunk carries its
                        // finish_reason, which is where the client expects it.
                        let finish = if i == last { finish_reason.clone() } else { Value::Null };
                        rewrap::delta_chunk(&env, delta.into_delta(), finish)
                    })
                    .collect();
                if frames.is_empty() && !finish_reason.is_null() {
                    // A terminal chunk whose text produced nothing still has to
                    // deliver the finish_reason.
                    frames.push(rewrap::delta_chunk(&env, serde_json::json!({}), finish_reason));
                }

                for chat in frames {
                    if let Some(died) = death_at.take() {
                        metrics::record_seam(&ctx.model, leg_provider.unwrap_or("external"), died.elapsed().as_secs_f64());
                    }
                    // The reframed chunk is what the client received, so it — not
                    // the completions chunk — is what a further resume continues from.
                    let _ = acc.ingest(&chat);
                    yield Ok(rewrap::sse_frame(&chat));
                }
            };

            match verdict {
                Verdict::Alive => unreachable!("Alive never breaks the frame loop"),
                Verdict::Complete | Verdict::LostTrailer => {
                    // A leg can end without ever sending a finish_reason chunk
                    // (a lost trailer, or a usage frame that stood in for one),
                    // and the parser may still be holding bytes that turned out
                    // not to be the start of a tag. Flush before closing rather
                    // than dropping them. `PlainForward` holds nothing back, so
                    // this is inert for every unmapped model.
                    let flushed = parser.as_mut().map(|p| p.finish()).unwrap_or_default();
                    if !flushed.is_empty() && let Some(env) = acc.envelope().cloned() {
                        for delta in flushed {
                            let chat = rewrap::delta_chunk(&env, delta.into_delta(), Value::Null);
                            let _ = acc.ingest(&chat);
                            yield Ok(rewrap::sse_frame(&chat));
                        }
                    }
                    // The generation finished. If its usage frame never arrived
                    // (death families no_usage / no_done), synthesize one from a
                    // render so the request still bills and still reports.
                    if !saw_usage
                        && let Some(env) = acc.envelope().cloned()
                        && let Some(text) = acc.continuation_text()
                        && let Some(usage) = render_only_usage(&state, &ctx, &text).await
                    {
                        metrics::record_usage_anomaly("no_usage_frame");
                        yield Ok(rewrap::sse_frame(&rewrap::usage_frame(&env, usage)));
                    }
                    yield Ok(done_bytes.unwrap_or_else(|| Bytes::from_static(DONE_FRAME)));
                    if attempts > 0 {
                        outcome.record("resumed", first_death_reason.unwrap_or("ok"));
                    } else {
                        outcome.defuse();
                    }
                    break 'chain;
                }
                Verdict::NoResume(reason) => {
                    // Surface the frame we are refusing to resume — that is the
                    // error the client needs to see, not an earlier one we did
                    // try to recover from.
                    if let Some(frame) = refusal.take().or_else(|| first_death.take()) {
                        yield Ok(frame);
                    }
                    outcome.record("failed", reason);
                    // Restore passthrough close semantics: drain whatever leg 1
                    // still sends (typically the trailing `[DONE]`) so the
                    // client's stream ends exactly as it would have without this
                    // layer. Resume legs are never drained — their raw
                    // completions frames belong to the leg, not the client.
                    if !resuming {
                        while let Some(Ok(bytes)) = current.next().await {
                            yield Ok(bytes);
                        }
                    }
                    break 'chain;
                }
                Verdict::Resume(reason) => {
                    if first_death_reason.is_none() {
                        first_death_reason = Some(reason);
                    }
                    // A seam inside reasoning/tool syntax no longer blocks the
                    // resume: the leg's raw output goes through the
                    // accumulator's paired forward parser (seeded at dispatch
                    // below), which re-frames `</think>`/DSML back into chat
                    // deltas instead of leaking them as answer text.
                    let Some(text) = acc.continuation_text() else {
                        // Disarmed, or nothing generated yet: resume-from-zero is
                        // a plain retry, which is not this feature's job.
                        if let Some(frame) = first_death.take() {
                            yield Ok(frame);
                        }
                        outcome.record("failed", "not_reconstructable");
                        break 'chain;
                    };
                    debug!(
                        model = %ctx.model,
                        reason,
                        attempts,
                        response_id = ?ctx.response_id,
                        "Mid-stream death detected; attempting resume"
                    );
                    if death_at.is_none() {
                        death_at = Some(Instant::now());
                    }
                    if _slot.is_none() {
                        match state.inflight.try_acquire(&ctx.model) {
                            Some(slot) => _slot = Some(slot),
                            None => {
                                // An incident is already saturating this model's
                                // resume budget; surface the death as today.
                                if let Some(frame) = first_death.take() {
                                    yield Ok(frame);
                                }
                                outcome.record("failed", "throttled");
                                break 'chain;
                            }
                        }
                    }

                    // Attempts are per death AND per chain: `max_attempts` bounds
                    // the total resume legs for one logical stream.
                    let mut leg: Option<super::resume::Leg> = None;
                    let mut last_failure = "attempts_exhausted";
                    while attempts < state.cfg.max_attempts {
                        attempts += 1;
                        match resume::attempt(&state, &ctx, &text, attempts).await {
                            Ok(l) => {
                                leg = Some(l);
                                break;
                            }
                            Err(LegError::MaxTokensReached(render)) => {
                                // The client's own `max_tokens` is already spent —
                                // finishing is correct, resuming would overrun it.
                                if let Some(env) = acc.envelope().cloned() {
                                    yield Ok(rewrap::sse_frame(&rewrap::length_stop_frame(&env)));
                                    if !saw_usage {
                                        let (usage, _) = rewrap::merge_usage(
                                            render.continuation_tokens.unwrap_or(0) as u64,
                                            0,
                                            0,
                                            render.total as u64,
                                        );
                                        yield Ok(rewrap::sse_frame(&rewrap::usage_frame(&env, usage)));
                                    }
                                }
                                yield Ok(Bytes::from_static(DONE_FRAME));
                                outcome.record("failed", "max_tokens_reached");
                                break 'chain;
                            }
                            Err(e) => {
                                warn!(model = %ctx.model, attempt = attempts, error = %e, "Resume leg failed");
                                last_failure = e.reason();
                            }
                        }
                    }

                    match leg {
                        Some(l) => {
                            last_render = Some(l.render);
                            current = l.stream;
                            // Seeded HERE, before any of the leg's own output is
                            // fed back in: the accumulator is at the death point
                            // exactly now, and that is the structure the leg's
                            // first token continues.
                            parser = Some(acc.forward_parser());
                            resuming = true;
                            leg_provider = None;
                            leg_deadline = Some(l.deadline);
                            refusal = None;
                        }
                        None => {
                            // Out of attempts: the client sees exactly what an
                            // unresumed death would have given them.
                            if let Some(frame) = first_death.take() {
                                yield Ok(frame);
                            }
                            outcome.record("failed", last_failure);
                            break 'chain;
                        }
                    }
                }
            }
        }

        metrics::record_max_frame_gap(&ctx.model, max_frame_gap.as_secs_f64());
    };

    let mut response = Response::from_parts(parts, Body::from_stream(stream));
    response.headers_mut().remove(header::CONTENT_LENGTH);
    response
}

/// Token accounting for a stream that finished without a usage frame: no leg
/// ran, so the render is the only source. `prompt = total - seg` is the
/// conversation plus generation stub; `completion = seg` is what the model
/// produced. Returns `None` if the render fails — better no usage frame than a
/// fabricated one.
async fn render_only_usage(state: &ContinuationState, ctx: &RequestContext, text: &str) -> Option<rewrap::MergedUsage> {
    let render_kwargs = ctx.render_kwargs();
    let prefix = RenderPrefix {
        virtual_model: &ctx.model,
        messages: ctx.messages(),
        tools: ctx.tools(),
        chat_template_kwargs: render_kwargs.as_ref(),
        continuation_text: text,
    };
    let render = state.tokenizer.render(&prefix).await.ok()?;
    let seg = render.continuation_tokens? as u64;
    let (usage, _) = rewrap::merge_usage(seg, 0, 0, render.total as u64);
    Some(usage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frame_payload_reads_the_data_line_through_crlf_and_multiline_events() {
        assert_eq!(frame_payload(b"data: {\"a\":1}\n\n"), Some("{\"a\":1}"));
        assert_eq!(frame_payload(b"data: [DONE]\r\n\r\n"), Some("[DONE]"));
        assert_eq!(frame_payload(b"event: message\ndata: {\"a\":1}\n\n"), Some("{\"a\":1}"));
        assert_eq!(frame_payload(b": keep-alive\n\n"), None, "comments carry no payload");
        assert_eq!(frame_payload(b"data:{\"tight\":1}\n\n"), Some("{\"tight\":1}"));
    }

    #[test]
    fn structured_output_is_detected_in_every_documented_form() {
        for key in [
            "response_format",
            "json_schema",
            "guided_json",
            "guided_regex",
            "guided_grammar",
            "guided_choice",
        ] {
            let body = json!({ key: {"type": "json_object"} });
            assert!(is_structured_output(&body, "m"), "{key} must disqualify the request");
        }
        assert!(
            is_structured_output(&json!({}), "deepseek-v4-flash-dottxt"),
            "-dottxt aliases are structured by construction"
        );
        assert!(
            !is_structured_output(&json!({"response_format": null}), "m"),
            "a null field is not a constraint"
        );
        assert!(!is_structured_output(&json!({"temperature": 0.7}), "deepseek-v4-flash"));
    }

    #[test]
    fn origin_derives_from_the_key_purpose_and_gates_on_config() {
        assert_eq!(Origin::from_purpose(Some(ApiKeyPurpose::Batch)), Origin::Batch);
        assert_eq!(Origin::from_purpose(Some(ApiKeyPurpose::Playground)), Origin::Playground);
        assert_eq!(Origin::from_purpose(Some(ApiKeyPurpose::Realtime)), Origin::Realtime);
        // An unknown/absent key is treated as realtime — the same default
        // analytics uses for `request_origin`.
        assert_eq!(Origin::from_purpose(None), Origin::Realtime);

        // Defaults: realtime leads the rollout, batch and playground trail.
        let cfg = ContinuationConfig::default();
        assert!(Origin::Realtime.is_enabled(&cfg));
        assert!(!Origin::Batch.is_enabled(&cfg));
        assert!(!Origin::Playground.is_enabled(&cfg));
    }

    #[test]
    fn request_context_reads_the_resume_inputs() {
        let ctx = RequestContext {
            model: "m".to_string(),
            body: json!({
                "model": "m", "stream": true,
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{"type": "function"}],
                "chat_template_kwargs": {"thinking": true},
                "max_tokens": 500
            }),
            path: "/chat/completions".to_string(),
            response_id: None,
            route: RouteInfo::default(),
            origin: "realtime",
        };
        assert_eq!(ctx.messages()[0]["role"], "user");
        assert!(ctx.tools().is_some());
        assert_eq!(ctx.chat_template_kwargs().unwrap()["thinking"], true);
        assert_eq!(ctx.max_tokens(), Some(500));

        // `max_completion_tokens` is the newer spelling for the same cap.
        let ctx = RequestContext {
            model: "m".to_string(),
            body: json!({"max_completion_tokens": 42}),
            path: "/chat/completions".to_string(),
            response_id: None,
            route: RouteInfo::default(),
            origin: "realtime",
        };
        assert_eq!(ctx.max_tokens(), Some(42));
        assert!(ctx.tools().is_none());
    }
}
