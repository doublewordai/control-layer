//! Integration tests for the request upload stall watchdog.
//!
//! These use raw TCP servers (no database) to verify request framing and the
//! boundary between upload completion and response waiting. Watchdog timing
//! is covered by controlled unit tests in `http.rs`, independent of platform
//! TCP buffer sizes.

use std::time::{Duration, Instant};

use fusillade::batch::{BatchId, TemplateId};
use fusillade::http::{HttpClient, ReqwestHttpClient};
use fusillade::{FusilladeError, RequestData, RequestId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket};

const TEST_RECEIVE_BUFFER_BYTES: u32 = 16 * 1024;

fn test_request(endpoint: String, body: String) -> RequestData {
    RequestData {
        id: RequestId::from(uuid::Uuid::new_v4()),
        batch_id: Some(BatchId::from(uuid::Uuid::new_v4())),
        template_id: TemplateId::from(uuid::Uuid::new_v4()),
        custom_id: None,
        endpoint,
        method: "POST".to_string(),
        path: "/v1/test".to_string(),
        body,
        model: "test-model".to_string(),
        api_key: "test-key".to_string(),
        created_by: String::new(),
        submitted_at: None,
        batch_metadata: std::collections::HashMap::new(),
    }
}

fn client(stall: Duration) -> ReqwestHttpClient {
    ReqwestHttpClient::new(
        Duration::from_secs(10),
        Duration::from_secs(10),
        Duration::from_secs(10),
        vec![],
    )
    .with_upload_stall_timeout(stall)
}

async fn read_headers(stream: &mut tokio::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).await.unwrap();
        buf.push(byte[0]);
    }
    String::from_utf8(buf).unwrap()
}

fn backpressured_listener() -> TcpListener {
    let socket = TcpSocket::new_v4().unwrap();
    socket
        .set_recv_buffer_size(TEST_RECEIVE_BUFFER_BYTES)
        .unwrap();
    let effective_size = socket.recv_buffer_size().unwrap();
    assert!(
        effective_size < 1024 * 1024,
        "test receive buffer is too large to enforce backpressure: {effective_size} bytes"
    );
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    socket.listen(1).unwrap()
}

#[tokio::test]
async fn healthy_upload_preserves_content_length_framing() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let body = "x".repeat(200 * 1024);
    let body_len = body.len();

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let headers = read_headers(&mut stream).await;
        let lower = headers.to_lowercase();
        assert!(
            lower.contains(&format!("content-length: {body_len}")),
            "expected exact content-length header, got: {headers}"
        );
        assert!(
            !lower.contains("transfer-encoding"),
            "body must not switch to chunked framing, got: {headers}"
        );
        let mut received = vec![0u8; body_len];
        stream.read_exact(&mut received).await.unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .unwrap();
        received
    });

    let request = test_request(format!("http://{addr}"), body.clone());
    let response = client(Duration::from_secs(5))
        .execute(&request, "test-key")
        .await
        .unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, "ok");

    let received = server.await.unwrap();
    assert_eq!(received.len(), body_len);
    assert_eq!(received, body.into_bytes());
}

#[tokio::test]
async fn upload_stall_aborts_after_stall_timeout() {
    let listener = backpressured_listener();
    let addr = listener.local_addr().unwrap();

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut first = vec![0u8; 1024];
        stream.read_exact(&mut first).await.unwrap();
        // Stop reading but keep the connection open. The deliberately small
        // receive buffer ensures the client's upload cannot finish locally.
        std::future::pending::<()>().await;
        drop(stream);
    });

    let request = test_request(format!("http://{addr}"), "x".repeat(8 * 1024 * 1024));
    let started = Instant::now();
    let error = client(Duration::from_millis(500))
        .execute(&request, "test-key")
        .await
        .unwrap_err();

    match error {
        FusilladeError::UploadStallTimeout(message) => {
            assert!(message.contains("stalled"), "unexpected message: {message}");
        }
        other => panic!("expected UploadStallTimeout, got: {other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "stall abort took {:?}",
        started.elapsed()
    );
    server.abort();
}

#[tokio::test]
async fn slow_but_progressing_upload_is_not_killed() {
    let listener = backpressured_listener();
    let addr = listener.local_addr().unwrap();
    let body_len = 16 * 1024 * 1024;
    let stall_timeout = Duration::from_millis(500);

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let headers = read_headers(&mut stream).await;
        assert!(headers.to_lowercase().contains("content-length"));
        let mut remaining = body_len;
        let mut chunk = vec![0u8; 1024 * 1024];
        let mut pacing = tokio::time::interval(Duration::from_millis(100));
        pacing.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        while remaining > 0 {
            pacing.tick().await;
            let take = remaining.min(chunk.len());
            stream.read_exact(&mut chunk[..take]).await.unwrap();
            remaining -= take;
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .unwrap();
    });

    let request = test_request(format!("http://{addr}"), "x".repeat(body_len));
    let started = Instant::now();
    let response = client(stall_timeout)
        .execute(&request, "test-key")
        .await
        .unwrap();
    assert_eq!(response.status, 200);
    assert!(
        started.elapsed() > stall_timeout * 2,
        "test did not span multiple complete stall windows"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn completed_upload_can_wait_longer_than_stall_timeout_for_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let body_len = 128 * 1024;
    let (body_read_tx, body_read_rx) = tokio::sync::oneshot::channel();
    let (respond_tx, respond_rx) = tokio::sync::oneshot::channel();

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let headers = read_headers(&mut stream).await;
        assert!(headers.to_lowercase().contains("content-length"));
        let mut received = vec![0u8; body_len];
        stream.read_exact(&mut received).await.unwrap();
        body_read_tx.send(()).unwrap();
        respond_rx.await.unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .unwrap();
    });

    let request = test_request(format!("http://{addr}"), "x".repeat(body_len));
    let mut client_task = tokio::spawn(async move {
        client(Duration::from_millis(100))
            .execute(&request, "test-key")
            .await
    });

    tokio::time::timeout(Duration::from_secs(5), body_read_rx)
        .await
        .expect("server did not receive the complete body")
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut client_task)
            .await
            .is_err(),
        "request completed while the server was deliberately withholding response headers"
    );

    respond_tx.send(()).unwrap();
    let response = tokio::time::timeout(Duration::from_secs(5), client_task)
        .await
        .expect("request did not complete after response headers were released")
        .unwrap()
        .unwrap();
    assert_eq!(response.status, 200);
    server.await.unwrap();
}
