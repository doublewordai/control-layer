mod sse_handler;

use askama::Template;
use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json},
    routing::{get, post},
};
use hdrhistogram::Histogram;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sse_handler::benchmark_events_handler;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{RwLock, broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

// --- Data Models ---

#[derive(Deserialize, Debug)]
struct BenchmarkConfig {
    url: String,
    concurrency: u32,
    requests: u32,
    body: serde_json::Value,
    api_key: Option<String>,
    stream_progress: Option<bool>,
}

#[derive(Serialize, Clone)]
struct BenchmarkResults {
    total_time_seconds: f64,
    requests_per_second: f64,
    total_requests: u32,
    successful_requests: u32,
    failed_requests: u32,
    p50_latency_ms: u64,
    p90_latency_ms: u64,
    p99_latency_ms: u64,
    min_latency_ms: u64,
    max_latency_ms: u64,
    avg_latency_ms: f64,
}

#[derive(Deserialize, Debug)]
struct OpenAIChatRequest {
    messages: Vec<serde_json::Value>,
    model: String,
    stream: Option<bool>,
}

#[derive(Serialize)]
struct OpenAIChatChoice {
    index: u32,
    message: OpenAIMessage,
    finish_reason: String,
}

#[derive(Serialize)]
struct OpenAIMessage {
    role: String,
    content: String,
}

#[derive(Serialize)]
struct OpenAIChatCompletion {
    id: String,
    object: String,
    created: u64,
    model: String,
    choices: Vec<OpenAIChatChoice>,
}

/// The returned models from the /v1/models endpoint.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct Model {
    /// The model identifier, which can be referenced in the API endpoints.
    pub(crate) id: String,
    /// The Unix timestamp (in seconds) when the model was created.
    pub(crate) created: u32,
    /// The object type, which is always "model".
    pub(crate) object: String,
    /// The organization that owns the model.
    pub(crate) owned_by: String,
}

/// The response from the /v1/models endpoint, which is a list of models.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub(crate) struct ListModelResponse {
    /// The object type, which is always "list".
    pub object: String,
    /// A list of model objects.
    pub data: Vec<Model>,
}

#[derive(Serialize, Clone)]
pub struct ProgressUpdate {
    status: String,
    completed: u32,
    total: u32,
    results: Option<BenchmarkResults>,
}

#[derive(Serialize)]
struct BenchmarkStartResponse {
    run_id: String,
}

pub struct AppState {
    http_client: Client,
    pub progress_channels: Arc<RwLock<HashMap<String, broadcast::Sender<ProgressUpdate>>>>,
    pub cancellation_tokens: Arc<RwLock<HashMap<String, CancellationToken>>>,
}

#[derive(Template)]
#[template(path = "monitor.html")]
struct MonitorTemplate;

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate;

// --- Main Application ---

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "openai_benchmark_rust=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let shared_state = Arc::new(AppState {
        http_client: Client::new(),
        progress_channels: Arc::new(RwLock::new(HashMap::new())),
        cancellation_tokens: Arc::new(RwLock::new(HashMap::new())),
    });

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/monitor", get(monitor_handler))
        .route("/v1/chat/completions", post(mock_openai_handler))
        .route("/v1/models", get(mock_openai_models_handler))
        .route("/benchmark", post(benchmark_handler))
        .route("/benchmark/:run_id/events", get(benchmark_events_handler))
        .route("/benchmark/:run_id/cancel", post(cancel_benchmark_handler))
        .with_state(shared_state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
    tracing::debug!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

async fn mock_openai_models_handler() -> impl IntoResponse {
    tracing::debug!("Mock OpenAI models endpoint received request");

    let response = ListModelResponse {
        object: "list".to_string(),
        data: vec![Model {
            id: "simple-model".to_string(),
            created: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as u32,
            object: "model".to_string(),
            owned_by: "openai".to_string(),
        }],
    };

    (StatusCode::OK, Json(response))
}

// --- API Handlers ---

async fn index_handler() -> impl IntoResponse {
    let template = IndexTemplate;
    Html(template.render().unwrap())
}

async fn monitor_handler() -> impl IntoResponse {
    let template = MonitorTemplate;
    Html(template.render().unwrap())
}

async fn mock_openai_handler(Json(payload): Json<OpenAIChatRequest>) -> impl IntoResponse {
    tracing::debug!("Mock OpenAI endpoint received request: {:?}", payload);

    let response = OpenAIChatCompletion {
        id: format!("cmpl-{}", uuid::Uuid::new_v4()),
        object: "chat.completion".to_string(),
        created: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        model: payload.model,
        choices: vec![OpenAIChatChoice {
            index: 0,
            message: OpenAIMessage {
                role: "assistant".to_string(),
                content: "This is a mock response from the benchmark server.".to_string(),
            },
            finish_reason: "stop".to_string(),
        }],
    };

    (StatusCode::OK, Json(response))
}

async fn benchmark_handler(
    State(state): State<Arc<AppState>>,
    Json(config): Json<BenchmarkConfig>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    tracing::debug!("Benchmark triggered with config: {:?}", config);

    let stream_progress = config.stream_progress.unwrap_or(false);

    if stream_progress {
        let run_id = uuid::Uuid::new_v4().to_string();
        let (progress_tx, _) = broadcast::channel::<ProgressUpdate>(100);
        let cancel_token = CancellationToken::new();

        state
            .progress_channels
            .write()
            .await
            .insert(run_id.clone(), progress_tx);
        state
            .cancellation_tokens
            .write()
            .await
            .insert(run_id.clone(), cancel_token.clone());

        let state_clone = state.clone();
        let config_clone = config;
        let run_id_clone = run_id.clone();

        tokio::spawn(async move {
            let _ =
                run_benchmark_with_progress(state_clone, config_clone, run_id_clone, cancel_token)
                    .await;
        });

        return Ok(Json(BenchmarkStartResponse { run_id }).into_response());
    }

    let results = run_benchmark(state, config).await?;
    Ok(Json(results).into_response())
}

async fn run_benchmark(
    state: Arc<AppState>,
    config: BenchmarkConfig,
) -> Result<BenchmarkResults, (StatusCode, String)> {
    let (tx, mut rx) = mpsc::channel::<Duration>(config.requests as usize);
    let start_time = Instant::now();
    let client = state.http_client.clone();
    let body = Arc::new(config.body);
    let url = Arc::new(config.url);

    let requests_per_worker = config.requests / config.concurrency;
    let remaining_requests = config.requests % config.concurrency;

    let mut workers = vec![];
    for i in 0..config.concurrency {
        let client = client.clone();
        let url = url.clone();
        let body = body.clone();
        let tx = tx.clone();
        let num_requests = requests_per_worker + if i < remaining_requests { 1 } else { 0 };

        let api_key = config.api_key.clone();
        let worker = tokio::spawn(async move {
            let mut durations = vec![];
            for _ in 0..num_requests {
                let req_start = Instant::now();
                let mut req = client.post(url.as_str()).json(body.as_ref());
                if let Some(ref key) = api_key {
                    req = req.header("Authorization", format!("Bearer {}", key));
                }
                let res = req.send().await;
                let duration = req_start.elapsed();
                if let Ok(response) = res {
                    if response.status() == 200 {
                        durations.push(duration);
                    }
                }
            }
            for duration in durations {
                if tx.send(duration).await.is_err() {
                    tracing::error!("Failed to send duration to channel");
                }
            }
        });
        workers.push(worker);
    }

    drop(tx);

    for worker in workers {
        worker.await.map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Worker task failed: {}", e),
            )
        })?;
    }

    let total_duration = start_time.elapsed();
    let mut durations = Vec::new();

    while let Some(duration) = rx.recv().await {
        durations.push(duration);
    }

    let successful_requests = durations.len() as u32;
    let failed_requests = config.requests - successful_requests;

    let results = tokio::task::spawn_blocking(move || {
        let mut latencies = Histogram::<u64>::new(3).unwrap();

        for duration in durations {
            latencies.record(duration.as_millis() as u64).unwrap();
        }

        BenchmarkResults {
            total_time_seconds: total_duration.as_secs_f64(),
            requests_per_second: successful_requests as f64 / total_duration.as_secs_f64(),
            total_requests: config.requests,
            successful_requests,
            failed_requests,
            p50_latency_ms: latencies.value_at_percentile(50.0),
            p90_latency_ms: latencies.value_at_percentile(90.0),
            p99_latency_ms: latencies.value_at_percentile(99.0),
            min_latency_ms: latencies.min(),
            max_latency_ms: latencies.max(),
            avg_latency_ms: latencies.mean(),
        }
    })
    .await
    .map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to calculate histogram: {}", e),
        )
    })?;

    Ok(results)
}

async fn run_benchmark_with_progress(
    state: Arc<AppState>,
    config: BenchmarkConfig,
    run_id: String,
    cancel_token: CancellationToken,
) -> Result<(), ()> {
    let progress_tx = {
        let channels = state.progress_channels.read().await;
        channels.get(&run_id).cloned()
    };

    let Some(progress_tx) = progress_tx else {
        return Err(());
    };

    let _ = progress_tx.send(ProgressUpdate {
        status: "running".to_string(),
        completed: 0,
        total: config.requests,
        results: None,
    });

    let (tx, mut rx) = mpsc::channel::<Duration>(config.requests as usize);
    let (completed_tx, mut completed_rx) = mpsc::channel::<u32>(config.requests as usize);

    let start_time = Instant::now();
    let client = state.http_client.clone();
    let body = Arc::new(config.body.clone());
    let url = Arc::new(config.url.clone());

    let requests_per_worker = config.requests / config.concurrency;
    let remaining_requests = config.requests % config.concurrency;

    let mut workers = vec![];
    for i in 0..config.concurrency {
        let client = client.clone();
        let url = url.clone();
        let body = body.clone();
        let tx = tx.clone();
        let completed_tx = completed_tx.clone();
        let cancel_token = cancel_token.clone();
        let num_requests = requests_per_worker + if i < remaining_requests { 1 } else { 0 };

        let api_key = config.api_key.clone();
        let worker = tokio::spawn(async move {
            let mut durations = vec![];
            for _ in 0..num_requests {
                if cancel_token.is_cancelled() {
                    break;
                }
                let req_start = Instant::now();
                let mut req = client.post(url.as_str()).json(body.as_ref());
                if let Some(ref key) = api_key {
                    req = req.header("Authorization", format!("Bearer {}", key));
                }
                let res = req.send().await;
                let duration = req_start.elapsed();
                if let Ok(response) = res {
                    if response.status() == 200 {
                        durations.push(duration);
                        let _ = completed_tx.send(1).await;
                    }
                }
            }
            for duration in durations {
                let _ = tx.send(duration).await;
            }
        });
        workers.push(worker);
    }

    drop(tx);
    drop(completed_tx);

    let progress_tx_clone = progress_tx.clone();
    let total_requests = config.requests;
    tokio::spawn(async move {
        let mut completed = 0u32;
        while (completed_rx.recv().await).is_some() {
            completed += 1;
            let _ = progress_tx_clone.send(ProgressUpdate {
                status: "running".to_string(),
                completed,
                total: total_requests,
                results: None,
            });
        }
    });

    for worker in workers {
        let _ = worker.await;
    }

    if cancel_token.is_cancelled() {
        let _ = progress_tx.send(ProgressUpdate {
            status: "cancelled".to_string(),
            completed: 0,
            total: config.requests,
            results: None,
        });

        state.progress_channels.write().await.remove(&run_id);
        state.cancellation_tokens.write().await.remove(&run_id);
        return Ok(());
    }

    let total_duration = start_time.elapsed();
    let mut durations = Vec::new();

    while let Some(duration) = rx.recv().await {
        durations.push(duration);
    }

    let successful_requests = durations.len() as u32;
    let failed_requests = config.requests - successful_requests;
    let total_requests = config.requests;

    let results = tokio::task::spawn_blocking(move || {
        let mut latencies = Histogram::<u64>::new(3).unwrap();

        for duration in durations {
            latencies.record(duration.as_millis() as u64).unwrap();
        }

        BenchmarkResults {
            total_time_seconds: total_duration.as_secs_f64(),
            requests_per_second: successful_requests as f64 / total_duration.as_secs_f64(),
            total_requests,
            successful_requests,
            failed_requests,
            p50_latency_ms: latencies.value_at_percentile(50.0),
            p90_latency_ms: latencies.value_at_percentile(90.0),
            p99_latency_ms: latencies.value_at_percentile(99.0),
            min_latency_ms: latencies.min(),
            max_latency_ms: latencies.max(),
            avg_latency_ms: latencies.mean(),
        }
    })
    .await
    .unwrap();

    let _ = progress_tx.send(ProgressUpdate {
        status: "completed".to_string(),
        completed: config.requests,
        total: config.requests,
        results: Some(results),
    });

    state.progress_channels.write().await.remove(&run_id);
    state.cancellation_tokens.write().await.remove(&run_id);

    Ok(())
}

async fn cancel_benchmark_handler(
    Path(run_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let cancel_token = {
        let tokens = state.cancellation_tokens.read().await;
        tokens.get(&run_id).cloned()
    };

    match cancel_token {
        Some(token) => {
            token.cancel();
            (
                StatusCode::OK,
                Json(serde_json::json!({"message": "Benchmark cancelled"})),
            )
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Benchmark not found"})),
        ),
    }
}
