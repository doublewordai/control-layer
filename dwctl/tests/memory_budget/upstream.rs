//! Mock OpenAI-compatible upstream that holds every request open until the
//! test releases it, so the application's per-request memory can be read while
//! a known number of requests are in flight. Every response closes its
//! connection, so the application keeps no idle upstream connections.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::watch;

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

#[derive(Clone)]
struct Shared {
    model: String,
    reply: Arc<std::sync::Mutex<Reply>>,
    /// Requests whose body has been read and that are now waiting for release.
    parked: Arc<AtomicUsize>,
    /// Streamed replies that have written everything they send before release.
    streamed: Arc<AtomicUsize>,
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
            reply: Arc::new(std::sync::Mutex::new(Reply::Immediate)),
            parked: Arc::new(AtomicUsize::new(0)),
            streamed: Arc::new(AtomicUsize::new(0)),
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

    pub fn set_reply(&self, reply: Reply) {
        *self.shared.reply.lock().unwrap() = reply;
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

    pub fn reset_counts(&self) {
        self.shared.parked.store(0, Ordering::SeqCst);
        self.shared.streamed.store(0, Ordering::SeqCst);
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
            let model = shared.model.clone();
            let streamed = shared.streamed.clone();
            let stream = async_stream::stream! {
                const PIECE: usize = 16 * 1024;
                let mut remaining = content_bytes;
                while remaining > 0 {
                    let len = remaining.min(PIECE);
                    remaining -= len;
                    yield Ok::<_, std::convert::Infallible>(sse(&chunk(&model, Some(&"a".repeat(len)), None, None)));
                }
                streamed.fetch_add(1, Ordering::SeqCst);
                wait_for_release(&mut release, generation).await;
                yield Ok(sse(&chunk(&model, None, Some("stop"), None)));
                let usage = json!({ "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 });
                yield Ok(sse(&chunk(&model, None, None, Some(usage))));
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
