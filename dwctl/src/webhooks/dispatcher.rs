//! Webhook dispatch: claim, sign, send, process results.
//!
//! ```text
//! dispatcher.tick()
//!   ├─ claim_and_send()
//!   │    ├─ DB: claim_retriable_deliveries()  // single query: SELECT FOR UPDATE SKIP LOCKED
//!   │    │                                    // + JOIN webhook config (url, secret, enabled)
//!   │    └─ for each claimed delivery:
//!   │         ├─ DB: mark_exhausted()         // only if webhook deleted/disabled
//!   │         ├─ CPU: sign_payload()          // HMAC-SHA256
//!   │         └─ send_tx.try_send(request) ──────────────────────┐
//!   │                                                             │
//!   │              ┌──────────────────────────────────────────────┘
//!   │              ▼
//!   │         run_sender (spawned task):
//!   │              ├─ recv from send_rx
//!   │              ├─ acquire semaphore permit (caps concurrency)
//!   │              ├─ spawn HTTP POST
//!   │              └─ result_tx.send(result) ────────────────────┐
//!   │                                                             │
//!   └─ drain_results()                                            │
//!        ├─ result_rx.try_recv() ◄───────────────────────────────┘
//!        └─ for each result:
//!             ├─ Success → DB: mark_delivered() + reset_failures()
//!             └─ Failure → DB: mark_failed() + increment_failures()
//! ```
//!
//! The sender task has no DB access and no secrets — just HTTP in, result out.
//! On shutdown, unprocessed deliveries become re-claimable after the 5-minute
//! crash safety window.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use metrics::counter;
use sqlx::PgPool;
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::WebhookConfig;
use crate::db::handlers::Webhooks;
use crate::webhooks::signing;

// --- Channel types ---

/// A pre-built webhook HTTP request ready to send.
#[derive(Debug)]
struct WebhookSendRequest {
    url: String,
    headers: Vec<(String, String)>,
    body: String,
    delivery_id: Uuid,
    webhook_id: Uuid,
    attempt_count: i32,
}

/// Outcome of a single HTTP send attempt.
#[derive(Debug)]
enum SendOutcome {
    Success { status_code: u16 },
    Failure { status_code: Option<u16>, error: String },
}

/// Result of a webhook send attempt, sent back via the result channel.
#[derive(Debug)]
struct WebhookSendResult {
    delivery_id: Uuid,
    webhook_id: Uuid,
    attempt_count: i32,
    outcome: SendOutcome,
}

// --- Dispatcher ---

pub struct WebhookDispatcher {
    pool: PgPool,
    send_tx: mpsc::Sender<WebhookSendRequest>,
    result_rx: mpsc::Receiver<WebhookSendResult>,
    retry_schedule: Vec<i64>,
    claim_batch_size: i64,
    circuit_breaker_threshold: i32,
}

impl WebhookDispatcher {
    /// Create a new dispatcher and spawn the background sender task.
    pub fn spawn(pool: PgPool, config: &WebhookConfig, shutdown: CancellationToken) -> Self {
        let (send_tx, send_rx) = mpsc::channel::<WebhookSendRequest>(config.channel_capacity);
        let (result_tx, result_rx) = mpsc::channel(config.channel_capacity);

        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs))
            .build()
            .expect("Failed to create webhook HTTP client");

        tokio::spawn(run_sender(send_rx, result_tx, http_client, config.max_concurrent_sends, shutdown));

        Self {
            pool,
            send_tx,
            result_rx,
            retry_schedule: config.retry_schedule_secs.clone(),
            claim_batch_size: config.claim_batch_size,
            circuit_breaker_threshold: config.circuit_breaker_threshold,
        }
    }

    /// Run one dispatch cycle: claim → sign → send → process results.
    pub async fn tick(&mut self) {
        tracing::debug!("Webhook dispatcher tick");
        self.claim_and_send().await;
        self.drain_results().await;
    }

    /// Claim deliveries that are due, sign them, and push to the sender channel.
    async fn claim_and_send(&self) {
        let mut conn = match self.pool.acquire().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to acquire connection for retry claims");
                return;
            }
        };

        let deliveries = {
            let mut repo = Webhooks::new(&mut conn);
            match repo.claim_retriable_deliveries(self.claim_batch_size).await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to claim retriable deliveries");
                    return;
                }
            }
        };

        if deliveries.is_empty() {
            tracing::debug!("No deliveries to claim");
            return;
        }

        counter!("dwctl_webhook_deliveries_claimed_total").increment(deliveries.len() as u64);
        tracing::debug!(count = deliveries.len(), "Claimed deliveries for sending");

        for delivery in deliveries {
            // Webhook missing from LEFT JOIN. With CASCADE delete this is
            // unlikely (the delivery row would be gone too), but guard anyway.
            let (Some(url), Some(secret), Some(enabled)) = (&delivery.webhook_url, &delivery.webhook_secret, delivery.webhook_enabled)
            else {
                tracing::warn!(
                    delivery_id = %delivery.id,
                    webhook_id = %delivery.webhook_id,
                    "Webhook not found for delivery, marking exhausted"
                );
                let mut repo = Webhooks::new(&mut conn);
                let _ = repo.mark_exhausted(delivery.id).await;
                continue;
            };

            // Webhook disabled since delivery was created
            if !enabled {
                tracing::debug!(
                    delivery_id = %delivery.id,
                    webhook_id = %delivery.webhook_id,
                    "Webhook disabled, marking delivery exhausted"
                );
                let mut repo = Webhooks::new(&mut conn);
                let _ = repo.mark_exhausted(delivery.id).await;
                continue;
            }

            // Sign the stored payload
            let payload_str = match serde_json::to_string(&delivery.payload) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, delivery_id = %delivery.id, "Failed to serialize delivery payload, marking exhausted");
                    let mut repo = Webhooks::new(&mut conn);
                    let _ = repo.mark_exhausted(delivery.id).await;
                    continue;
                }
            };

            let timestamp = Utc::now().timestamp();
            let msg_id = delivery.event_id.to_string();
            let signature = match signing::sign_payload(&msg_id, timestamp, &payload_str, secret) {
                Some(s) => s,
                None => {
                    tracing::warn!(delivery_id = %delivery.id, "Failed to sign webhook payload");
                    continue;
                }
            };

            let send_request = WebhookSendRequest {
                url: url.clone(),
                headers: vec![
                    ("Content-Type".to_string(), "application/json".to_string()),
                    ("webhook-id".to_string(), msg_id),
                    ("webhook-timestamp".to_string(), timestamp.to_string()),
                    ("webhook-signature".to_string(), signature),
                    ("webhook-version".to_string(), "1".to_string()),
                ],
                body: payload_str,
                delivery_id: delivery.id,
                webhook_id: delivery.webhook_id,
                attempt_count: delivery.attempt_count,
            };

            if let Err(e) = self.send_tx.try_send(send_request) {
                tracing::warn!(
                    delivery_id = %delivery.id,
                    "Failed to push to sender channel (will retry after claim timeout): {}",
                    e
                );
                // The claim already bumped next_attempt_at by 5 minutes,
                // so this delivery will be re-claimed later.
            }
        }
    }

    /// Drain completed send results and update DB status.
    async fn drain_results(&mut self) {
        let mut conn = match self.pool.acquire().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to acquire connection for result drain");
                return;
            }
        };

        let mut drained = 0u32;
        while let Ok(result) = self.result_rx.try_recv() {
            drained += 1;
            let mut repo = Webhooks::new(&mut conn);

            // The delivery or webhook may have been CASCADE-deleted while this
            // send was in-flight. The UPDATE calls below silently affect 0 rows
            // in that case, which is fine — there's nothing left to update.
            match result.outcome {
                SendOutcome::Success { status_code } => {
                    counter!("dwctl_webhook_deliveries_total", "outcome" => "success").increment(1);
                    if let Err(e) = repo.mark_delivered(result.delivery_id, status_code as i32).await {
                        tracing::warn!(error = %e, delivery_id = %result.delivery_id, "Failed to mark delivery as delivered");
                    }
                    if let Err(e) = repo.reset_failures(result.webhook_id).await {
                        tracing::warn!(error = %e, webhook_id = %result.webhook_id, "Failed to reset webhook failures");
                    }
                    tracing::debug!(
                        webhook_id = %result.webhook_id,
                        delivery_id = %result.delivery_id,
                        status = status_code,
                        "Webhook delivered successfully"
                    );
                }
                SendOutcome::Failure { status_code, ref error } => {
                    counter!("dwctl_webhook_deliveries_total", "outcome" => "failure").increment(1);
                    if let Err(e) = repo
                        .mark_failed(
                            result.delivery_id,
                            status_code.map(|c| c as i32),
                            error,
                            result.attempt_count,
                            &self.retry_schedule,
                        )
                        .await
                    {
                        tracing::warn!(error = %e, delivery_id = %result.delivery_id, "Failed to mark delivery as failed");
                    }
                    // increment_failures returns None if the webhook was deleted
                    // while this delivery was in-flight — not an error.
                    match repo.increment_failures(result.webhook_id, self.circuit_breaker_threshold).await {
                        Ok(None) => {
                            tracing::debug!(
                                webhook_id = %result.webhook_id,
                                delivery_id = %result.delivery_id,
                                "Webhook deleted while delivery was in-flight, skipping failure tracking"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, webhook_id = %result.webhook_id, "Failed to increment webhook failures");
                        }
                        Ok(Some(_)) => {}
                    }
                    tracing::warn!(
                        webhook_id = %result.webhook_id,
                        delivery_id = %result.delivery_id,
                        status_code = ?status_code,
                        error = %error,
                        "Webhook delivery failed"
                    );
                }
            }
        }

        if drained > 0 {
            tracing::debug!(count = drained, "Drained webhook send results");
        } else {
            tracing::debug!("No send results to drain");
        }
    }
}

// --- Sender task ---

/// Long-lived task that receives signed requests and performs HTTP delivery.
/// Has no DB access and no secrets — just HTTP in, result out.
async fn run_sender(
    mut rx: mpsc::Receiver<WebhookSendRequest>,
    result_tx: mpsc::Sender<WebhookSendResult>,
    http_client: reqwest::Client,
    max_concurrent_sends: usize,
    shutdown: CancellationToken,
) {
    let semaphore = Arc::new(Semaphore::new(max_concurrent_sends));

    loop {
        let request = tokio::select! {
            req = rx.recv() => {
                match req {
                    Some(r) => r,
                    None => {
                        tracing::debug!("Webhook sender channel closed, shutting down");
                        break;
                    }
                }
            }
            _ = shutdown.cancelled() => {
                tracing::debug!("Webhook sender received shutdown signal");
                break;
            }
        };

        let permit = match semaphore.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                tracing::warn!("Webhook sender semaphore closed");
                break;
            }
        };

        let client = http_client.clone();
        let tx = result_tx.clone();

        tokio::spawn(async move {
            let _permit = permit;

            tracing::debug!(
                delivery_id = %request.delivery_id,
                url = %request.url,
                attempt = request.attempt_count,
                "Sending webhook HTTP request"
            );

            let mut req_builder = client.post(&request.url);
            for (name, value) in &request.headers {
                req_builder = req_builder.header(name, value);
            }
            req_builder = req_builder.body(request.body);

            let outcome = match req_builder.send().await {
                Ok(response) => {
                    let status_code = response.status().as_u16();
                    if response.status().is_success() {
                        SendOutcome::Success { status_code }
                    } else {
                        SendOutcome::Failure {
                            status_code: Some(status_code),
                            error: format!("HTTP {}", status_code),
                        }
                    }
                }
                Err(e) => SendOutcome::Failure {
                    status_code: None,
                    error: e.to_string(),
                },
            };

            let result = WebhookSendResult {
                delivery_id: request.delivery_id,
                webhook_id: request.webhook_id,
                attempt_count: request.attempt_count,
                outcome,
            };

            if let Err(e) = tx.send(result).await {
                tracing::warn!(delivery_id = %request.delivery_id, "Failed to send webhook result back: {}", e);
            }
        });
    }

    tracing::debug!("Webhook sender task exited");
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Helper to start the sender and return channels + shutdown token.
    async fn start_sender() -> (
        mpsc::Sender<WebhookSendRequest>,
        mpsc::Receiver<WebhookSendResult>,
        CancellationToken,
    ) {
        let (send_tx, send_rx) = mpsc::channel(10);
        let (result_tx, result_rx) = mpsc::channel(10);
        let http_client = reqwest::Client::new();
        let shutdown = CancellationToken::new();

        let sender_shutdown = shutdown.clone();
        tokio::spawn(async move {
            run_sender(send_rx, result_tx, http_client, 20, sender_shutdown).await;
        });

        (send_tx, result_rx, shutdown)
    }

    fn make_request(url: &str, delivery_id: Uuid, webhook_id: Uuid, attempt: i32) -> WebhookSendRequest {
        WebhookSendRequest {
            url: url.to_string(),
            headers: vec![("Content-Type".to_string(), "application/json".to_string())],
            body: r#"{"test": true}"#.to_string(),
            delivery_id,
            webhook_id,
            attempt_count: attempt,
        }
    }

    #[tokio::test]
    async fn test_successful_delivery() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&mock_server)
            .await;

        let (send_tx, mut result_rx, shutdown) = start_sender().await;
        let delivery_id = Uuid::new_v4();
        let webhook_id = Uuid::new_v4();

        send_tx
            .send(make_request(&mock_server.uri(), delivery_id, webhook_id, 0))
            .await
            .unwrap();

        let result = result_rx.recv().await.unwrap();
        assert_eq!(result.delivery_id, delivery_id);
        assert_eq!(result.webhook_id, webhook_id);
        assert_eq!(result.attempt_count, 0);
        assert!(matches!(result.outcome, SendOutcome::Success { status_code: 200 }));

        shutdown.cancel();
    }

    #[tokio::test]
    async fn test_http_error_delivery() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&mock_server)
            .await;

        let (send_tx, mut result_rx, shutdown) = start_sender().await;
        let delivery_id = Uuid::new_v4();

        send_tx
            .send(make_request(&mock_server.uri(), delivery_id, Uuid::new_v4(), 2))
            .await
            .unwrap();

        let result = result_rx.recv().await.unwrap();
        assert_eq!(result.delivery_id, delivery_id);
        assert_eq!(result.attempt_count, 2);
        assert!(matches!(
            result.outcome,
            SendOutcome::Failure {
                status_code: Some(500),
                ..
            }
        ));

        shutdown.cancel();
    }

    #[tokio::test]
    async fn test_network_error_delivery() {
        // Point to a port that's not listening
        let (send_tx, mut result_rx, shutdown) = start_sender().await;
        let delivery_id = Uuid::new_v4();

        send_tx
            .send(make_request("http://127.0.0.1:1", delivery_id, Uuid::new_v4(), 0))
            .await
            .unwrap();

        let result = result_rx.recv().await.unwrap();
        assert_eq!(result.delivery_id, delivery_id);
        assert!(matches!(result.outcome, SendOutcome::Failure { status_code: None, .. }));

        shutdown.cancel();
    }

    #[tokio::test]
    async fn test_metadata_passed_through() {
        let mock_server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&mock_server)
            .await;

        let (send_tx, mut result_rx, shutdown) = start_sender().await;
        let delivery_id = Uuid::new_v4();
        let webhook_id = Uuid::new_v4();

        send_tx
            .send(make_request(&mock_server.uri(), delivery_id, webhook_id, 5))
            .await
            .unwrap();

        let result = result_rx.recv().await.unwrap();
        assert_eq!(result.delivery_id, delivery_id);
        assert_eq!(result.webhook_id, webhook_id);
        assert_eq!(result.attempt_count, 5);

        shutdown.cancel();
    }

    #[tokio::test]
    async fn test_sender_exits_on_channel_close() {
        let (send_tx, send_rx) = mpsc::channel(10);
        let (result_tx, _result_rx) = mpsc::channel(10);
        let http_client = reqwest::Client::new();
        let shutdown = CancellationToken::new();

        let handle = tokio::spawn(async move {
            run_sender(send_rx, result_tx, http_client, 20, shutdown).await;
        });

        // Drop the sender — channel closes
        drop(send_tx);

        // Sender should exit promptly
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("sender should exit when channel closes")
            .expect("sender should not panic");
    }
}
