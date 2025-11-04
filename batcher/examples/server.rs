/// A simple HTTP server using Axum that provides a web interface and API
/// for the batcher request manager.
///
/// Supports both in-memory and PostgreSQL backends:
/// - In-memory (default): cargo run --example server
/// - PostgreSQL: DATABASE_URL=postgresql://user@localhost/batcher cargo run --example server --features postgres
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        Html, IntoResponse, Response,
    },
    routing::{delete, get, post},
    Json, Router,
};
use batcher::{
    AnyRequest, DaemonConfig, InMemoryRequestManager, Pending, Request, RequestData, RequestId,
    RequestManager, ReqwestHttpClient,
};
use futures::stream::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::{convert::Infallible, sync::Arc};
use tracing::info;
use uuid::Uuid;

#[cfg(feature = "postgres")]
use batcher::PostgresRequestManager;

// Application state
#[derive(Clone)]
struct AppState {
    manager: Arc<dyn RequestManager>,
}

// API request/response types
#[derive(Debug, Serialize, Deserialize)]
struct SubmitRequestBody {
    endpoint: String,
    method: String,
    path: String,
    body: String,
    model: String,
    api_key: String,
}

#[derive(Debug, Serialize)]
struct SubmitResponse {
    id: RequestId,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
enum BatchSubmitResult {
    Success { id: RequestId },
    Error { error: String },
}

#[derive(Debug, Serialize)]
struct BatchSubmitResponse {
    results: Vec<BatchSubmitResult>,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

// API handlers
async fn submit_request(
    State(state): State<AppState>,
    Json(body): Json<SubmitRequestBody>,
) -> Result<Json<SubmitResponse>, AppError> {
    let request_id = RequestId::from(Uuid::new_v4());
    let request = Request {
        state: Pending {
            retry_attempt: 0,
            not_before: None,
        },
        data: RequestData {
            id: request_id,
            endpoint: body.endpoint,
            method: body.method,
            path: body.path,
            body: body.body,
            model: body.model,
            api_key: body.api_key,
        },
    };

    state
        .manager
        .submit_requests(vec![request])
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?
        .into_iter()
        .next()
        .unwrap()
        .map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(Json(SubmitResponse { id: request_id }))
}

async fn submit_batch(
    State(state): State<AppState>,
    Json(bodies): Json<Vec<SubmitRequestBody>>,
) -> Result<Json<BatchSubmitResponse>, AppError> {
    let mut request_ids = Vec::new();
    let requests: Vec<Request<Pending>> = bodies
        .into_iter()
        .map(|body| {
            let request_id = RequestId::from(Uuid::new_v4());
            request_ids.push(request_id);
            Request {
                state: Pending {
                    retry_attempt: 0,
                    not_before: None,
                },
                data: RequestData {
                    id: request_id,
                    endpoint: body.endpoint,
                    method: body.method,
                    path: body.path,
                    body: body.body,
                    model: body.model,
                    api_key: body.api_key,
                },
            }
        })
        .collect();

    let submit_results = state
        .manager
        .submit_requests(requests)
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;

    let results = request_ids
        .into_iter()
        .zip(submit_results)
        .map(|(request_id, result)| match result {
            Ok(_) => BatchSubmitResult::Success { id: request_id },
            Err(e) => BatchSubmitResult::Error {
                error: e.to_string(),
            },
        })
        .collect();

    Ok(Json(BatchSubmitResponse { results }))
}

async fn get_request_status(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<AnyRequest>, AppError> {
    let results = state
        .manager
        .get_status(vec![RequestId::from(id)])
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;

    let status = results
        .into_iter()
        .next()
        .unwrap()
        .map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(Json(status))
}

async fn cancel_request(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let results = state
        .manager
        .cancel_requests(vec![RequestId::from(id)])
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?;

    results
        .into_iter()
        .next()
        .unwrap()
        .map_err(|e| AppError::Internal(e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

async fn status_updates_stream(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = state.manager.get_status_updates(None);

    let sse_stream = stream.map(|result| {
        let event_data = match result {
            Ok(Ok(any_request)) => {
                serde_json::to_string(&any_request).unwrap_or_else(|_| "{}".to_string())
            }
            Ok(Err(e)) => serde_json::to_string(&ErrorResponse {
                error: e.to_string(),
            })
            .unwrap_or_else(|_| r#"{"error":"serialization error"}"#.to_string()),
            Err(e) => serde_json::to_string(&ErrorResponse {
                error: e.to_string(),
            })
            .unwrap_or_else(|_| r#"{"error":"stream error"}"#.to_string()),
        };

        Ok(Event::default().data(event_data))
    });

    Sse::new(sse_stream).keep_alive(KeepAlive::default())
}

async fn serve_frontend() -> Html<&'static str> {
    Html(HTML)
}

// Error handling
enum AppError {
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            AppError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };

        let body = Json(ErrorResponse {
            error: error_message,
        });

        (status, body).into_response()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    // Create HTTP client and config
    let http_client = Arc::new(ReqwestHttpClient::new());
    let config = DaemonConfig {
        claim_batch_size: 1000,
        default_model_concurrency: 1000,
        ..Default::default()
    };

    // Choose backend based on DATABASE_URL environment variable
    let manager: Arc<dyn RequestManager> = if let Ok(database_url) = std::env::var("DATABASE_URL") {
        #[cfg(feature = "postgres")]
        {
            info!("Using PostgreSQL backend: {}", database_url);
            let pool = sqlx::PgPool::connect(&database_url).await?;
            info!("Connected to PostgreSQL database");
            Arc::new(PostgresRequestManager::new(pool, http_client, config))
        }
        #[cfg(not(feature = "postgres"))]
        {
            eprintln!("ERROR: DATABASE_URL is set but postgres feature is not enabled!");
            eprintln!("Run with: cargo run --example server --features postgres");
            std::process::exit(1);
        }
    } else {
        info!("Using in-memory backend");
        Arc::new(InMemoryRequestManager::new(http_client, config))
    };

    // Start the daemon
    let daemon_handle = manager.run()?;
    info!("Daemon started");

    // Create application state
    let state = AppState { manager };

    // Build router
    let app = Router::new()
        .route("/", get(serve_frontend))
        .route("/api/requests", post(submit_request))
        .route("/api/requests/batch", post(submit_batch))
        .route("/api/requests/:id", get(get_request_status))
        .route("/api/requests/:id", delete(cancel_request))
        .route("/api/stream", get(status_updates_stream))
        .with_state(state);

    // Start server
    let addr = "127.0.0.1:3000";
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("Server listening on http://{}", addr);
    info!("Open http://{} in your browser to use the interface", addr);

    axum::serve(listener, app).await?;

    // Wait for daemon to complete (should never happen)
    daemon_handle.await??;

    Ok(())
}

const HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Batcher - Request Manager (In-Memory / PostgreSQL)</title>
    <style>
        * {
            margin: 0;
            padding: 0;
            box-sizing: border-box;
        }

        body {
            font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Oxygen, Ubuntu, Cantarell, sans-serif;
            background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
            min-height: 100vh;
            padding: 20px;
        }

        .container {
            max-width: 1400px;
            margin: 0 auto;
        }

        h1 {
            color: white;
            margin-bottom: 30px;
            text-align: center;
            font-size: 2.5em;
            text-shadow: 2px 2px 4px rgba(0,0,0,0.2);
        }

        .grid {
            display: grid;
            grid-template-columns: 1fr 1fr;
            gap: 20px;
            margin-bottom: 20px;
        }

        .panel {
            background: white;
            border-radius: 12px;
            padding: 25px;
            box-shadow: 0 10px 30px rgba(0,0,0,0.2);
        }

        .panel h2 {
            color: #667eea;
            margin-bottom: 20px;
            font-size: 1.5em;
            border-bottom: 2px solid #667eea;
            padding-bottom: 10px;
        }

        .form-group {
            margin-bottom: 15px;
        }

        label {
            display: block;
            margin-bottom: 5px;
            color: #333;
            font-weight: 500;
        }

        input, textarea, select {
            width: 100%;
            padding: 10px;
            border: 2px solid #e0e0e0;
            border-radius: 6px;
            font-size: 14px;
            transition: border-color 0.3s;
        }

        input:focus, textarea:focus, select:focus {
            outline: none;
            border-color: #667eea;
        }

        textarea {
            font-family: 'Courier New', monospace;
            resize: vertical;
            min-height: 100px;
        }

        .form-row {
            display: grid;
            grid-template-columns: 1fr 1fr;
            gap: 10px;
        }

        button {
            background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
            color: white;
            border: none;
            padding: 12px 24px;
            border-radius: 6px;
            font-size: 16px;
            font-weight: 600;
            cursor: pointer;
            transition: transform 0.2s, box-shadow 0.2s;
            width: 100%;
            margin-top: 10px;
        }

        button:hover {
            transform: translateY(-2px);
            box-shadow: 0 5px 15px rgba(102, 126, 234, 0.4);
        }

        button:active {
            transform: translateY(0);
        }

        .requests-list {
            max-height: 600px;
            overflow-y: auto;
        }

        .request-item {
            background: #f8f9fa;
            border-left: 4px solid #667eea;
            padding: 15px;
            margin-bottom: 15px;
            border-radius: 6px;
            transition: transform 0.2s;
        }

        .request-item:hover {
            transform: translateX(5px);
        }

        .request-header {
            display: flex;
            justify-content: space-between;
            align-items: center;
            margin-bottom: 10px;
        }

        .request-id {
            font-family: 'Courier New', monospace;
            font-size: 12px;
            color: #666;
        }

        .status-badge {
            padding: 4px 12px;
            border-radius: 12px;
            font-size: 12px;
            font-weight: 600;
            text-transform: uppercase;
        }

        .status-pending { background: #ffeaa7; color: #d97706; }
        .status-claimed { background: #bfdbfe; color: #1e40af; }
        .status-processing { background: #c7d2fe; color: #4338ca; }
        .status-completed { background: #86efac; color: #15803d; }
        .status-failed { background: #fca5a5; color: #b91c1c; }
        .status-canceled { background: #e5e7eb; color: #4b5563; }

        .request-details {
            font-size: 13px;
            color: #555;
            line-height: 1.6;
        }

        .request-model {
            font-weight: 600;
            color: #667eea;
        }

        .notification {
            position: fixed;
            top: 20px;
            right: 20px;
            background: white;
            padding: 15px 20px;
            border-radius: 8px;
            box-shadow: 0 5px 20px rgba(0,0,0,0.3);
            display: none;
            z-index: 1000;
        }

        .notification.show {
            display: block;
            animation: slideIn 0.3s ease-out;
        }

        @keyframes slideIn {
            from {
                transform: translateX(400px);
                opacity: 0;
            }
            to {
                transform: translateX(0);
                opacity: 1;
            }
        }

        .stats {
            display: grid;
            grid-template-columns: repeat(auto-fit, minmax(150px, 1fr));
            gap: 15px;
            margin-bottom: 20px;
        }

        .stat-card {
            background: linear-gradient(135deg, #667eea 0%, #764ba2 100%);
            color: white;
            padding: 20px;
            border-radius: 8px;
            text-align: center;
        }

        .stat-value {
            font-size: 2em;
            font-weight: 700;
        }

        .stat-label {
            font-size: 0.9em;
            opacity: 0.9;
            margin-top: 5px;
        }

        .connection-status {
            display: inline-block;
            width: 10px;
            height: 10px;
            border-radius: 50%;
            margin-right: 5px;
        }

        .connection-status.connected {
            background: #00b894;
            box-shadow: 0 0 5px #00b894;
        }

        .connection-status.disconnected {
            background: #d63031;
        }
    </style>
</head>
<body>
    <div class="container">
        <h1>Batcher - HTTP Request Manager</h1>

        <div class="stats">
            <div class="stat-card">
                <div class="stat-value" id="stat-total">0</div>
                <div class="stat-label">Total Requests</div>
            </div>
            <div class="stat-card">
                <div class="stat-value" id="stat-pending">0</div>
                <div class="stat-label">Pending</div>
            </div>
            <div class="stat-card">
                <div class="stat-value" id="stat-claimed">0</div>
                <div class="stat-label">Claimed</div>
            </div>
            <div class="stat-card">
                <div class="stat-value" id="stat-processing">0</div>
                <div class="stat-label">Processing</div>
            </div>
            <div class="stat-card">
                <div class="stat-value" id="stat-completed">0</div>
                <div class="stat-label">Completed</div>
            </div>
            <div class="stat-card">
                <div class="stat-value" id="stat-failed">0</div>
                <div class="stat-label">Failed</div>
            </div>
        </div>

        <div class="grid">
            <div class="panel">
                <h2>Submit Request</h2>
                <form id="request-form">
                    <div class="form-group">
                        <label for="endpoint">Endpoint URL</label>
                        <input type="text" id="endpoint" value="https://app.doubleword.ai/ai" placeholder="https://app.doubleword.ai/ai" required>
                    </div>

                    <div class="form-row">
                        <div class="form-group">
                            <label for="method">Method</label>
                            <select id="method">
                                <option>GET</option>
                                <option selected>POST</option>
                                <option>PUT</option>
                                <option>DELETE</option>
                            </select>
                        </div>

                        <div class="form-group">
                            <label for="path">Path</label>
                            <input type="text" id="path" value="/v1/chat/completions" placeholder="/v1/chat/completions" required>
                        </div>
                    </div>

                    <div class="form-row">
                        <div class="form-group">
                            <label for="model">Model</label>
                            <input type="text" id="model" value="google/gemma-3-12b-it" placeholder="google/gemma-3-12b-it" required>
                        </div>

                        <div class="form-group">
                            <label for="api-key">API Key</label>
                            <input type="password" id="api-key" value="" placeholder="sk-...">
                        </div>
                    </div>

                    <div class="form-group">
                        <label for="body">Request Body (JSON)</label>
                        <textarea id="body" placeholder='{"model":"google/gemma-3-12b-it","messages":[{"role":"user","content":"Hello!"}]}'>{"model":"google/gemma-3-12b-it","messages":[{"role":"user","content":"Hello, how are you?"}]}</textarea>
                    </div>

                    <details>
                        <summary style="cursor: pointer; margin-bottom: 15px; color: #667eea;">Advanced Options</summary>

                        <div class="form-row">
                            <div class="form-group">
                                <label for="max-retries">Max Retries</label>
                                <input type="number" id="max-retries" value="3" min="0">
                            </div>

                            <div class="form-group">
                                <label for="timeout">Timeout (ms)</label>
                                <input type="number" id="timeout" value="30000" min="1000">
                            </div>
                        </div>

                        <div class="form-row">
                            <div class="form-group">
                                <label for="backoff">Initial Backoff (ms)</label>
                                <input type="number" id="backoff" value="1000" min="100">
                            </div>

                            <div class="form-group">
                                <label for="backoff-factor">Backoff Factor</label>
                                <input type="number" id="backoff-factor" value="2" min="1">
                            </div>
                        </div>
                    </details>

                    <button type="submit">Submit Request</button>

                    <div style="margin-top: 20px; padding-top: 20px; border-top: 2px solid #e0e0e0;">
                        <label style="display: block; margin-bottom: 10px; color: #667eea; font-weight: 600;">Batch Submit</label>
                        <div style="display: flex; gap: 10px; align-items: center;">
                            <input type="number" id="batch-count" value="10" min="1" max="1000" style="width: 100px;" placeholder="Count">
                            <button type="button" onclick="submitBatch()" style="flex: 1; margin: 0;">Submit <span id="batch-count-display">10</span>x</button>
                        </div>
                        <div style="display: flex; gap: 10px; margin-top: 10px;">
                            <button type="button" onclick="submitBatch(5)" style="flex: 1; margin: 0; background: linear-gradient(135deg, #84fab0 0%, #8fd3f4 100%);">5x</button>
                            <button type="button" onclick="submitBatch(25)" style="flex: 1; margin: 0; background: linear-gradient(135deg, #a8edea 0%, #fed6e3 100%);">25x</button>
                            <button type="button" onclick="submitBatch(100)" style="flex: 1; margin: 0; background: linear-gradient(135deg, #ffecd2 0%, #fcb69f 100%);">100x</button>
                        </div>
                    </div>
                </form>
            </div>

            <div class="panel">
                <h2>
                    <span class="connection-status" id="connection-status"></span>
                    Live Requests
                </h2>
                <div class="requests-list" id="requests-list">
                    <p style="text-align: center; color: #999; padding: 40px 0;">
                        No requests yet. Submit a request to get started!
                    </p>
                </div>
            </div>
        </div>
    </div>

    <div class="notification" id="notification"></div>

    <script>
        const requests = new Map();
        let eventSource = null;

        // Connect to SSE stream
        function connectSSE() {
            eventSource = new EventSource('/api/stream');

            eventSource.onopen = () => {
                console.log('SSE connected');
                document.getElementById('connection-status').classList.add('connected');
                document.getElementById('connection-status').classList.remove('disconnected');
            };

            eventSource.onmessage = (event) => {
                try {
                    const request = JSON.parse(event.data);
                    if (request.error) {
                        console.error('Stream error:', request.error);
                        return;
                    }
                    updateRequest(request);
                } catch (e) {
                    console.error('Failed to parse SSE data:', e);
                }
            };

            eventSource.onerror = (error) => {
                console.error('SSE error:', error);
                document.getElementById('connection-status').classList.remove('connected');
                document.getElementById('connection-status').classList.add('disconnected');

                // Reconnect after 3 seconds
                setTimeout(() => {
                    eventSource.close();
                    connectSSE();
                }, 3000);
            };
        }

        // Update request in the list
        function updateRequest(anyRequest) {
            // AnyRequest is serialized as: { state: "Pending", request: { state: {...}, data: {...} } }
            const stateName = anyRequest.state;
            const req = anyRequest.request;
            const stateData = req.state;
            const requestData = req.data;

            // Flatten for easier access
            const flattened = {
                state: stateName,
                ...requestData,
                ...stateData
            };

            requests.set(requestData.id, flattened);
            renderRequests();
            updateStats();
        }

        // Render requests list
        function renderRequests() {
            const container = document.getElementById('requests-list');

            if (requests.size === 0) {
                container.innerHTML = '<p style="text-align: center; color: #999; padding: 40px 0;">No requests yet. Submit a request to get started!</p>';
                return;
            }

            const sortedRequests = Array.from(requests.values())
                .sort((a, b) => {
                    // Sort by most recent timestamp available
                    const getTime = (req) => {
                        if (req.completed_at) return new Date(req.completed_at);
                        if (req.failed_at) return new Date(req.failed_at);
                        if (req.canceled_at) return new Date(req.canceled_at);
                        if (req.started_at) return new Date(req.started_at);
                        if (req.claimed_at) return new Date(req.claimed_at);
                        return new Date(0);
                    };
                    return getTime(b) - getTime(a);
                });

            container.innerHTML = sortedRequests.map(req => {
                const state = req.state.toLowerCase();

                let details = `<strong class="request-model">${req.model}</strong> - ${req.method} ${req.path}`;

                if (state === 'completed') {
                    details += `<br>Status: ${req.response_status}`;
                    if (req.response_body && req.response_body.length < 100) {
                        details += `<br>Response: ${req.response_body}`;
                    }
                } else if (state === 'failed') {
                    details += `<br>Error: ${req.error || 'Unknown error'}`;
                }

                return `
                    <div class="request-item">
                        <div class="request-header">
                            <span class="request-id">${req.id}</span>
                            <span class="status-badge status-${state}">${state}</span>
                        </div>
                        <div class="request-details">
                            ${details}
                        </div>
                    </div>
                `;
            }).join('');
        }

        // Update statistics
        function updateStats() {
            const stats = {
                total: requests.size,
                pending: 0,
                claimed: 0,
                processing: 0,
                completed: 0,
                failed: 0,
            };

            requests.forEach(req => {
                const state = req.state.toLowerCase();
                if (state in stats) {
                    stats[state]++;
                }
            });

            document.getElementById('stat-total').textContent = stats.total;
            document.getElementById('stat-pending').textContent = stats.pending;
            document.getElementById('stat-claimed').textContent = stats.claimed;
            document.getElementById('stat-processing').textContent = stats.processing;
            document.getElementById('stat-completed').textContent = stats.completed;
            document.getElementById('stat-failed').textContent = stats.failed;
        }

        // Show notification
        function showNotification(message, type = 'success') {
            const notification = document.getElementById('notification');
            notification.textContent = message;
            notification.style.background = type === 'success' ? '#55efc4' : '#ff7675';
            notification.classList.add('show');

            setTimeout(() => {
                notification.classList.remove('show');
            }, 3000);
        }

        // Update batch count display
        document.getElementById('batch-count').addEventListener('input', (e) => {
            document.getElementById('batch-count-display').textContent = e.target.value;
        });

        // Submit batch function
        async function submitBatch(count) {
            const batchCount = count || parseInt(document.getElementById('batch-count').value);

            if (!batchCount || batchCount < 1 || batchCount > 1000) {
                showNotification('Batch count must be between 1 and 1000', 'error');
                return;
            }

            const requestBody = {
                endpoint: document.getElementById('endpoint').value,
                method: document.getElementById('method').value,
                path: document.getElementById('path').value,
                body: document.getElementById('body').value,
                model: document.getElementById('model').value,
                api_key: document.getElementById('api-key').value || '',
                max_retries: parseInt(document.getElementById('max-retries').value),
                timeout_ms: parseInt(document.getElementById('timeout').value),
                backoff_ms: parseInt(document.getElementById('backoff').value),
                backoff_factor: parseInt(document.getElementById('backoff-factor').value),
                max_backoff_ms: 60000,
            };

            showNotification(`Submitting ${batchCount} requests...`);

            // Create array of identical requests
            const batch = Array(batchCount).fill(requestBody);

            try {
                const response = await fetch('/api/requests/batch', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify(batch),
                });

                if (response.ok) {
                    const result = await response.json();
                    const submitted = result.results.filter(r => r.status === 'success').length;
                    const failed = result.results.filter(r => r.status === 'error').length;
                    showNotification(`Batch complete: ${submitted} submitted, ${failed} failed`);
                } else {
                    const error = await response.json();
                    showNotification(`Error: ${error.error}`, 'error');
                }
            } catch (error) {
                showNotification(`Network error: ${error.message}`, 'error');
            }
        }

        // Handle form submission
        document.getElementById('request-form').addEventListener('submit', async (e) => {
            e.preventDefault();

            const requestBody = {
                endpoint: document.getElementById('endpoint').value,
                method: document.getElementById('method').value,
                path: document.getElementById('path').value,
                body: document.getElementById('body').value,
                model: document.getElementById('model').value,
                api_key: document.getElementById('api-key').value || '',
                max_retries: parseInt(document.getElementById('max-retries').value),
                timeout_ms: parseInt(document.getElementById('timeout').value),
                backoff_ms: parseInt(document.getElementById('backoff').value),
                backoff_factor: parseInt(document.getElementById('backoff-factor').value),
                max_backoff_ms: 60000,
            };

            try {
                const response = await fetch('/api/requests', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify(requestBody),
                });

                if (response.ok) {
                    const result = await response.json();
                    showNotification(`Request submitted: ${result.id}`);
                } else {
                    const error = await response.json();
                    showNotification(`Error: ${error.error}`, 'error');
                }
            } catch (error) {
                showNotification(`Network error: ${error.message}`, 'error');
            }
        });

        // Initialize
        connectSSE();
    </script>
</body>
</html>
"#;
