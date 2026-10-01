//! Mock OpenAI-compatible upstream that holds every request open until the
//! test releases it, so the application's per-request memory can be read while
//! a known number of requests are in flight. Every response closes its
//! connection, so the application keeps no idle upstream connections.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Notify, watch};

/// Streamed content is sent as events of this many bytes.
pub const EVENT_BYTES: usize = 16 * 1024;

/// What the upstream sends back once a request has arrived.
#[derive(Clone, Copy, Debug)]
pub enum Reply {
    /// Send a complete non-streaming response straight away.
    Immediate,
    /// Wait for release, then send a complete non-streaming response.
    Hold,
    /// Send `content_bytes` of streamed content, wait for release, then finish the stream.
    StreamThenHold { content_bytes: usize },
}

/// Streamed events the load generator has received through the application.
///
/// The upstream sends each event only once every stream has delivered the
/// previous ones, so the application reads one event at a time on every
/// machine rather than whatever the kernel has queued.
#[derive(Clone, Default)]
pub struct Delivered {
    events: Arc<AtomicUsize>,
    changed: Arc<Notify>,
}

impl Delivered {
    pub fn add(&self, events: usize) {
        if events > 0 {
            self.events.fetch_add(events, Ordering::SeqCst);
            self.changed.notify_waiters();
        }
    }

    async fn at_least(&self, events: usize) {
        loop {
            let changed = self.changed.notified();
            if self.events.load(Ordering::SeqCst) >= events {
                return;
            }
            changed.await;
        }
    }
}

#[derive(Clone)]
struct Shared {
    model: String,
    reply: Arc<Mutex<Reply>>,
    /// Requests in the current round, which move through a stream together.
    requests: Arc<AtomicUsize>,
    /// Requests whose body has been read and that are now waiting for release.
    parked: Arc<AtomicUsize>,
    /// Streams whose events before release have all reached the load generator.
    streamed: Arc<AtomicUsize>,
    delivered: Delivered,
    release: watch::Receiver<u64>,
}

pub struct Upstream {
    pub base_url: String,
    shared: Shared,
    release: watch::Sender<u64>,
}

impl Upstream {
    /// Binds a loopback listener and serves on the current runtime.
    pub async fn start(model: &str) -> Upstream {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock upstream");
        let addr = listener.local_addr().unwrap();
        let (release, release_rx) = watch::channel(0u64);
        let shared = Shared {
            model: model.to_string(),
            reply: Arc::new(Mutex::new(Reply::Immediate)),
            requests: Arc::new(AtomicUsize::new(1)),
            parked: Arc::new(AtomicUsize::new(0)),
            streamed: Arc::new(AtomicUsize::new(0)),
            delivered: Delivered::default(),
            release: release_rx,
        };
        let router = Router::new()
            .route("/v1/chat/completions", post(completion))
            .with_state(shared.clone());
        tokio::spawn(async move { axum::serve(listener, router).await.expect("mock upstream server") });
        Upstream {
            base_url: format!("http://{addr}/v1"),
            shared,
            release,
        }
    }

    /// Sets the reply for the next round of `requests` requests and resets the counters.
    pub fn prepare(&self, reply: Reply, requests: usize) {
        *self.shared.reply.lock().unwrap() = reply;
        self.shared.requests.store(requests, Ordering::SeqCst);
        self.shared.parked.store(0, Ordering::SeqCst);
        self.shared.streamed.store(0, Ordering::SeqCst);
        self.shared.delivered.events.store(0, Ordering::SeqCst);
    }

    pub fn delivered(&self) -> Delivered {
        self.shared.delivered.clone()
    }

    pub fn parked(&self) -> usize {
        self.shared.parked.load(Ordering::SeqCst)
    }

    pub fn streamed(&self) -> usize {
        self.shared.streamed.load(Ordering::SeqCst)
    }

    /// Lets every request that is currently parked complete.
    pub fn release_all(&self) {
        self.release.send_modify(|generation| *generation += 1);
    }
}

async fn completion(State(shared): State<Shared>, request: Request) -> Response {
    // Read the body frame by frame without keeping it.
    let mut body = request.into_body();
    while let Some(frame) = body.frame().await {
        if frame.is_err() {
            return StatusCode::BAD_REQUEST.into_response();
        }
    }

    let reply = *shared.reply.lock().unwrap();
    let mut release = shared.release.clone();
    let generation = *release.borrow_and_update();
    if !matches!(reply, Reply::Immediate) {
        shared.parked.fetch_add(1, Ordering::SeqCst);
    }

    match reply {
        Reply::Immediate | Reply::Hold => {
            if matches!(reply, Reply::Hold) {
                wait_for_release(&mut release, generation).await;
            }
            let completion = json!({
                "id": "chatcmpl-memory",
                "object": "chat.completion",
                "created": 0,
                "model": shared.model,
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "ok" },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 }
            });
            ([(header::CONNECTION, "close")], axum::Json(completion)).into_response()
        }
        Reply::StreamThenHold { content_bytes } => {
            let requests = shared.requests.load(Ordering::SeqCst);
            let stream = async_stream::stream! {
                let events = content_bytes.div_ceil(EVENT_BYTES);
                for event in 0..events {
                    shared.delivered.at_least(event * requests).await;
                    let len = EVENT_BYTES.min(content_bytes - event * EVENT_BYTES);
                    yield Ok::<_, std::convert::Infallible>(sse(&chunk(&shared.model, Some(&"a".repeat(len)), None, None)));
                }
                shared.delivered.at_least(events * requests).await;
                shared.streamed.fetch_add(1, Ordering::SeqCst);
                wait_for_release(&mut release, generation).await;
                yield Ok(sse(&chunk(&shared.model, None, Some("stop"), None)));
                let usage = json!({ "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 });
                yield Ok(sse(&chunk(&shared.model, None, None, Some(usage))));
                yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
            };
            Response::builder()
                .header(header::CONTENT_TYPE, "text/event-stream")
                .header(header::CONNECTION, "close")
                .body(Body::from_stream(stream))
                .unwrap()
        }
    }
}

async fn wait_for_release(release: &mut watch::Receiver<u64>, generation: u64) {
    let _ = release.wait_for(|current| *current > generation).await;
}

fn chunk(model: &str, content: Option<&str>, finish_reason: Option<&str>, usage: Option<Value>) -> Value {
    let choices = if usage.is_some() {
        json!([])
    } else {
        let delta = match content {
            Some(content) => json!({ "content": content }),
            None => json!({}),
        };
        json!([{ "index": 0, "delta": delta, "finish_reason": finish_reason }])
    };
    let mut value = json!({
        "id": "chatcmpl-memory",
        "object": "chat.completion.chunk",
        "created": 0,
        "model": model,
        "choices": choices,
    });
    if let Some(usage) = usage {
        value["usage"] = usage;
    }
    value
}

fn sse(value: &Value) -> Bytes {
    Bytes::from(format!("data: {value}\n\n"))
}
