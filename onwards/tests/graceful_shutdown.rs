//! Exercise the real gateway process, OS signals and proxied response bodies.
#![cfg(unix)]

use std::{
    convert::Infallible,
    fs,
    net::SocketAddr,
    path::PathBuf,
    process::{ExitStatus, Stdio},
    sync::Arc,
    time::Duration,
};

use axum::{Json, Router, body::Body, extract::State, response::Response, routing::post};
use futures_util::StreamExt;
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::{Child, Command},
    sync::Semaphore,
    task::JoinHandle,
    time::{sleep, timeout},
};
use uuid::Uuid;

const FIRST: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"first\"},\"finish_reason\":null}]}\n\n";
const LAST: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

struct Engine {
    started: Semaphore,
    finish: Semaphore,
}

async fn completion(State(engine): State<Arc<Engine>>, Json(request): Json<Value>) -> Response {
    engine.started.add_permits(1);
    if request["stream"] == true {
        let stream = async_stream::stream! {
            yield Ok::<_, Infallible>(FIRST);
            engine.finish.acquire().await.unwrap().forget();
            yield Ok::<_, Infallible>(LAST);
        };
        Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(stream))
            .unwrap()
    } else {
        engine.finish.acquire().await.unwrap().forget();
        Response::builder().header("content-type", "application/json")
            .body(Body::from(json!({"choices":[{"message":{"role":"assistant","content":"finished"},"finish_reason":"stop"}]}).to_string())).unwrap()
    }
}

struct Gateway {
    child: Child,
    dir: PathBuf,
    proxy: SocketAddr,
    metrics: SocketAddr,
    client: Client,
    engine: Arc<Engine>,
    engine_task: JoinHandle<()>,
}

impl Gateway {
    async fn start(drain_timeout: u64) -> Self {
        let engine = Arc::new(Engine {
            started: Semaphore::new(0),
            finish: Semaphore::new(0),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = listener.local_addr().unwrap();
        let router = Router::new()
            .route("/v1/chat/completions", post(completion))
            .with_state(engine.clone());
        let engine_task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let metrics_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = proxy_listener.local_addr().unwrap();
        let metrics = metrics_listener.local_addr().unwrap();
        let dir = std::env::temp_dir().join(format!("onwards-shutdown-{}", Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        fs::write(
            dir.join("targets.json"),
            json!({"targets":{"test-model":{"url":format!("http://{backend}/v1")}}}).to_string(),
        )
        .unwrap();
        let log = fs::File::create(dir.join("gateway.log")).unwrap();
        drop((proxy_listener, metrics_listener));
        let child = Command::new(env!("CARGO_BIN_EXE_onwards"))
            .args([
                "--port",
                &proxy.port().to_string(),
                "--metrics-port",
                &metrics.port().to_string(),
                "--targets",
            ])
            .arg(dir.join("targets.json"))
            .args([
                "--shutdown-delay-secs",
                "1",
                "--shutdown-timeout-secs",
                &drain_timeout.to_string(),
            ])
            .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
            .env("RUST_LOG", "info")
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut gateway = Self {
            child,
            dir,
            proxy,
            metrics,
            client: Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            engine,
            engine_task,
        };
        timeout(Duration::from_secs(15), async {
            loop {
                if let Some(status) = gateway.child.try_wait().unwrap() {
                    panic!("gateway exited at startup: {status}: {}", gateway.log());
                }
                if gateway
                    .client
                    .get(gateway.admin("/readyz"))
                    .send()
                    .await
                    .is_ok_and(|r| r.status() == StatusCode::OK)
                {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("gateway readiness deadline");
        gateway
    }

    fn admin(&self, path: &str) -> String {
        format!("http://{}{path}", self.metrics)
    }
    fn log(&self) -> String {
        fs::read_to_string(self.dir.join("gateway.log")).unwrap()
    }

    async fn signal(&self, signal: &str) {
        assert!(
            Command::new("kill")
                .args([signal, &self.child.id().unwrap().to_string()])
                .status()
                .await
                .unwrap()
                .success()
        );
    }

    async fn wait_for_drain(&mut self) {
        timeout(Duration::from_secs(5), async {
            loop {
                let response = self.client.get(self.admin("/readyz")).send().await.unwrap();
                if response.status() == StatusCode::SERVICE_UNAVAILABLE {
                    break;
                }
                assert_eq!(response.status(), StatusCode::OK);
                sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(
                self.client
                    .get(self.admin("/healthz"))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::OK
            );
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "active response must keep process alive"
            );
            // New TCP connections eventually stop, while the accepted response
            // and the independently served health/metrics endpoints stay alive.
            while TcpStream::connect(self.proxy).await.is_ok() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("proxy did not close admission");
    }

    async fn exit(&mut self) -> ExitStatus {
        timeout(Duration::from_secs(8), self.child.wait())
            .await
            .expect("gateway did not exit")
            .unwrap()
    }

    fn request(&self, stream: bool) -> reqwest::RequestBuilder {
        self.client.post(format!("http://{}/v1/chat/completions", self.proxy))
            .json(&json!({"model":"test-model","messages":[{"role":"user","content":"test"}],"stream":stream}))
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        self.engine_task.abort();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn sigterm_preserves_sse_and_metrics_until_the_body_finishes() {
    let mut gateway = Gateway::start(10).await;
    let response = gateway.request(true).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.bytes_stream();
    let first = timeout(Duration::from_secs(5), body.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first.as_ref(), FIRST.as_bytes());
    gateway.signal("-TERM").await;
    gateway.wait_for_drain().await;
    let metrics = gateway
        .client
        .get(gateway.admin("/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(metrics.contains("onwards_model_inflight{model=\"test-model\"} 1"));
    assert!(gateway.child.try_wait().unwrap().is_none());
    gateway.engine.finish.add_permits(1);
    let mut tail = Vec::new();
    while let Some(chunk) = body.next().await {
        tail.extend(chunk.unwrap());
    }
    assert_eq!(tail, LAST.as_bytes());
    assert!(gateway.exit().await.success(), "{}", gateway.log());
    assert!(gateway.log().contains("Gateway request drain complete"));
}

#[tokio::test]
async fn sigterm_waits_for_a_request_that_has_not_produced_headers() {
    let mut gateway = Gateway::start(10).await;
    let request = gateway.request(false);
    let response = tokio::spawn(async move { request.send().await.unwrap() });
    timeout(Duration::from_secs(5), gateway.engine.started.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    assert!(!response.is_finished());
    gateway.signal("-TERM").await;
    gateway.wait_for_drain().await;
    assert!(!response.is_finished());
    gateway.engine.finish.add_permits(1);
    let response = response.await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.unwrap()["choices"][0]["message"]["content"],
        "finished"
    );
    assert!(gateway.exit().await.success(), "{}", gateway.log());
}

#[tokio::test]
async fn stuck_stream_exits_unsuccessfully_at_the_drain_deadline() {
    let mut gateway = Gateway::start(1).await;
    let response = gateway.request(true).send().await.unwrap();
    let mut body = response.bytes_stream();
    assert_eq!(
        body.next().await.unwrap().unwrap().as_ref(),
        FIRST.as_bytes()
    );
    gateway.signal("-TERM").await;
    gateway.wait_for_drain().await;
    // Never release the upstream response; shutdown must remain bounded.
    assert!(!gateway.exit().await.success());
    assert!(
        gateway
            .log()
            .contains("gateway drain deadline exceeded after 1 seconds")
    );
    assert!(!gateway.log().contains("Gateway request drain complete"));
    assert!(
        body.next().await.unwrap().is_err(),
        "unfinished HTTP body must not look complete"
    );
}

#[tokio::test]
async fn sigint_closes_idle_keep_alive_connections_without_waiting_for_the_deadline() {
    let mut gateway = Gateway::start(30).await;
    let mut idle = TcpStream::connect(gateway.proxy).await.unwrap();
    idle.write_all(b"GET /v1/models HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    timeout(Duration::from_secs(5), async {
        loop {
            let mut chunk = [0; 1024];
            let count = idle.read(&mut chunk).await.unwrap();
            assert_ne!(count, 0, "connection must stay open after the response");
            response.extend_from_slice(&chunk[..count]);
            if let Some(end) = response.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&response[..end]).to_lowercase();
                assert!(headers.starts_with("http/1.1 200"));
                let length: usize = headers
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                if response.len() >= end + 4 + length {
                    break;
                }
            }
        }
    })
    .await
    .unwrap();
    gateway.signal("-INT").await;
    assert!(
        timeout(Duration::from_secs(4), gateway.exit())
            .await
            .unwrap()
            .success()
    );
    let mut byte = [0];
    assert_eq!(
        timeout(Duration::from_secs(1), idle.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
}
