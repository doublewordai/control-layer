use axum::{
    extract::State,
    response::{sse::Event, Html, IntoResponse, Sse},
    routing::{get, post},
    Json, Router,
};
use batcher::{
    Batcher, Daemon, DaemonConfig, InMemoryBatcher, Request, RequestContext, RequestStatus,
};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::StreamExt as _;
use tracing_subscriber;

#[derive(Clone)]
struct AppState {
    batcher: Arc<InMemoryBatcher>,
    daemon: Arc<Daemon<InMemoryBatcher>>,
}

#[derive(Debug, Deserialize)]
struct SubmitRequest {
    endpoint: String,
    method: String,
    path: String,
    body: String,
    model: String,
}

#[derive(Debug, Serialize)]
struct SubmitResponse {
    request_ids: Vec<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter("batcher=debug,tower_http=debug,info")
        .init();

    println!("🚀 Batcher Web Server\n");

    // Create in-memory batcher
    let batcher = Arc::new(InMemoryBatcher::new());
    println!("✓ Created in-memory batcher");

    // Configure and spawn daemon
    let daemon = Arc::new(Daemon::new(batcher.clone(), DaemonConfig::default()));
    println!("✓ Created daemon");

    // Spawn daemon in background
    let _daemon_handle = daemon.clone().spawn_arc();
    println!("✓ Spawned daemon\n");

    // Create app state
    let state = AppState { batcher, daemon };

    // Build our application with routes
    let app = Router::new()
        .route("/", get(serve_frontend))
        .route("/api/submit", post(submit_request))
        .route("/api/stream", get(stream_updates))
        .with_state(state);

    // Run the server
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .unwrap();

    println!("🌐 Server running at http://127.0.0.1:3000");
    println!("   Open your browser to see the demo!\n");

    axum::serve(listener, app).await?;

    Ok(())
}

async fn serve_frontend() -> Html<&'static str> {
    Html(include_str!("frontend.html"))
}

async fn submit_request(
    State(state): State<AppState>,
    Json(payload): Json<SubmitRequest>,
) -> impl IntoResponse {
    let request = Request {
        endpoint: payload.endpoint,
        method: payload.method,
        path: payload.path,
        body: payload.body,
        api_key: String::new(), // No API key for generic HTTP requests
        model: payload.model,
    };

    let context = RequestContext::default();

    match state.batcher.submit_requests(vec![(request, context)]).await {
        Ok(ids) => {
            let request_ids: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
            Json(SubmitResponse { request_ids }).into_response()
        }
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("Error: {}", e),
        )
            .into_response(),
    }
}

async fn stream_updates(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Subscribe to all request updates
    let update_stream = state.daemon.subscribe(None);

    // Convert to SSE events
    let sse_stream = update_stream.map(|update| {
        let status_str = match &update.status {
            RequestStatus::Pending => "pending".to_string(),
            RequestStatus::PendingProcessing { .. } => "pending_processing".to_string(),
            RequestStatus::Processing { .. } => "processing".to_string(),
            RequestStatus::Completed {
                response_status,
                response_body,
                ..
            } => format!(
                "completed:{}:{}",
                response_status,
                &response_body[..response_body.len().min(100)]
            ),
            RequestStatus::Failed { error, .. } => format!("failed:{}", error),
            RequestStatus::Canceled { .. } => "canceled".to_string(),
        };

        let data = format!("{}:{}", update.request_id, status_str);
        Ok(Event::default().data(data))
    });

    Sse::new(sse_stream)
}
