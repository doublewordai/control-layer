//! Command-line client for interacting with the batcher API server.
//!
//! Run the server first with:
//!   cargo run --example server
//!
//! Then use this CLI to interact with it:
//!   cargo run --example cli -- submit --endpoint https://api.example.com --path /test
//!   cargo run --example cli -- status <request-id>
//!   cargo run --example cli -- batch --count 100

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// CLI client for the batcher API
#[derive(Parser)]
#[command(name = "batcher-cli")]
#[command(about = "Command-line client for the batcher API", long_about = None)]
struct Cli {
    /// API server base URL
    #[arg(short, long, default_value = "http://127.0.0.1:3000")]
    server: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Submit a single request
    Submit {
        /// Endpoint URL
        #[arg(short, long, default_value = "https://api.example.com")]
        endpoint: String,

        /// HTTP method
        #[arg(short, long, default_value = "POST")]
        method: String,

        /// Request path
        #[arg(short, long, default_value = "/v1/test")]
        path: String,

        /// Request body (JSON)
        #[arg(short, long, default_value = "{}")]
        body: String,

        /// Model name
        #[arg(long, default_value = "test-model")]
        model: String,

        /// API key
        #[arg(short, long, default_value = "")]
        api_key: String,
    },

    /// Submit a batch of identical requests
    Batch {
        /// Number of requests to submit
        #[arg(short, long, default_value = "10")]
        count: usize,

        /// Endpoint URL
        #[arg(short, long, default_value = "https://api.example.com")]
        endpoint: String,

        /// HTTP method
        #[arg(short, long, default_value = "POST")]
        method: String,

        /// Request path
        #[arg(short, long, default_value = "/v1/test")]
        path: String,

        /// Request body (JSON)
        #[arg(short, long, default_value = "{}")]
        body: String,

        /// Model name
        #[arg(long, default_value = "test-model")]
        model: String,

        /// API key
        #[arg(short, long, default_value = "")]
        api_key: String,
    },

    /// Get request status
    Status {
        /// Request ID (UUID)
        id: Uuid,
    },

    /// Cancel a request
    Cancel {
        /// Request ID (UUID)
        id: Uuid,
    },

    /// Watch live request updates via SSE
    Watch,
}

#[derive(Debug, Clone, Serialize)]
struct SubmitRequestBody {
    endpoint: String,
    method: String,
    path: String,
    body: String,
    model: String,
    api_key: String,
}

#[derive(Debug, Deserialize)]
struct SubmitResponse {
    id: Uuid,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
enum BatchSubmitResult {
    Success { id: Uuid },
    Error { error: String },
}

#[derive(Debug, Deserialize)]
struct BatchSubmitResponse {
    results: Vec<BatchSubmitResult>,
}

#[derive(Debug, Deserialize)]
struct ErrorResponse {
    error: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = reqwest::Client::new();

    match cli.command {
        Commands::Submit {
            endpoint,
            method,
            path,
            body,
            model,
            api_key,
        } => {
            let request_body = SubmitRequestBody {
                endpoint,
                method,
                path,
                body,
                model,
                api_key,
            };

            let response = client
                .post(format!("{}/api/requests", cli.server))
                .json(&request_body)
                .send()
                .await
                .context("Failed to send request")?;

            if response.status().is_success() {
                let result: SubmitResponse = response.json().await?;
                println!("✓ Request submitted successfully");
                println!("  ID: {}", result.id);
            } else {
                let error: ErrorResponse = response.json().await?;
                eprintln!("✗ Error: {}", error.error);
                std::process::exit(1);
            }
        }

        Commands::Batch {
            count,
            endpoint,
            method,
            path,
            body,
            model,
            api_key,
        } => {
            if count == 0 || count > 10000 {
                eprintln!("✗ Count must be between 1 and 10000");
                std::process::exit(1);
            }

            let request_body = SubmitRequestBody {
                endpoint,
                method,
                path,
                body,
                model,
                api_key,
            };

            // Create array of identical requests
            let batch: Vec<_> = (0..count).map(|_| request_body.clone()).collect();

            println!("Submitting batch of {} requests...", count);

            let response = client
                .post(format!("{}/api/requests/batch", cli.server))
                .json(&batch)
                .send()
                .await
                .context("Failed to send batch request")?;

            if response.status().is_success() {
                let result: BatchSubmitResponse = response.json().await?;
                let success_count = result
                    .results
                    .iter()
                    .filter(|r| matches!(r, BatchSubmitResult::Success { .. }))
                    .count();
                let error_count = result.results.len() - success_count;

                println!("✓ Batch submission complete");
                println!("  Submitted: {}", success_count);
                println!("  Failed: {}", error_count);

                // Show first few IDs
                let first_ids: Vec<Uuid> = result
                    .results
                    .iter()
                    .filter_map(|r| match r {
                        BatchSubmitResult::Success { id } => Some(*id),
                        _ => None,
                    })
                    .take(5)
                    .collect();

                if !first_ids.is_empty() {
                    println!("\n  First request IDs:");
                    for id in first_ids {
                        println!("    - {}", id);
                    }
                    if success_count > 5 {
                        println!("    ... and {} more", success_count - 5);
                    }
                }
            } else {
                let error: ErrorResponse = response.json().await?;
                eprintln!("✗ Error: {}", error.error);
                std::process::exit(1);
            }
        }

        Commands::Status { id } => {
            let response = client
                .get(format!("{}/api/requests/{}", cli.server, id))
                .send()
                .await
                .context("Failed to get request status")?;

            if response.status().is_success() {
                let status: serde_json::Value = response.json().await?;
                println!("✓ Request status:");
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else if response.status() == 404 {
                eprintln!("✗ Request not found: {}", id);
                std::process::exit(1);
            } else {
                let error: ErrorResponse = response.json().await?;
                eprintln!("✗ Error: {}", error.error);
                std::process::exit(1);
            }
        }

        Commands::Cancel { id } => {
            let response = client
                .delete(format!("{}/api/requests/{}", cli.server, id))
                .send()
                .await
                .context("Failed to cancel request")?;

            if response.status().is_success() {
                println!("✓ Request cancelled: {}", id);
            } else if response.status() == 404 {
                eprintln!("✗ Request not found: {}", id);
                std::process::exit(1);
            } else {
                let error: ErrorResponse = response.json().await?;
                eprintln!("✗ Error: {}", error.error);
                std::process::exit(1);
            }
        }

        Commands::Watch => {
            println!("Watching for live updates (press Ctrl+C to stop)...\n");

            // Simple SSE client - just read the stream
            let response = client
                .get(format!("{}/api/stream", cli.server))
                .send()
                .await
                .context("Failed to connect to SSE stream")?;

            if !response.status().is_success() {
                eprintln!("✗ Failed to connect to stream");
                std::process::exit(1);
            }

            use futures::StreamExt;
            let mut stream = response.bytes_stream();

            while let Some(result) = stream.next().await {
                match result {
                    Ok(chunk) => {
                        let text = String::from_utf8_lossy(&chunk);
                        for line in text.lines() {
                            if line.starts_with("data: ") {
                                let data = &line[6..];
                                if let Ok(json) = serde_json::from_str::<serde_json::Value>(data) {
                                    if let Some(state) = json.get("state") {
                                        if let Some(request) = json.get("request") {
                                            if let Some(data) = request.get("data") {
                                                let id = data.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
                                                let model = data.get("model").and_then(|v| v.as_str()).unwrap_or("unknown");
                                                println!("[{}] {} - {}", state, id, model);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("✗ Stream error: {}", e);
                        break;
                    }
                }
            }
        }
    }

    Ok(())
}
