//! In-process batched writer for Open Responses lifecycle persistence.
//!
//! Replaces the underway `create-response` + `complete-response` jobs with a
//! channel + batched consumer modelled on [`crate::request_logging::batcher`].
//!
//! # Why a writer rather than underway
//!
//! Underway's per-job durability (rows on `underway.task`, retries, leader
//! polling) is the wrong shape for realtime/responses persistence. Realtime
//! cannot meaningfully be retried (the client connection is already gone),
//! flex durability is owned by the fusillade daemon, and the only
//! `requests`-table guarantee we actually need is "the row eventually
//! appears for observability". The pre-existing analytics batcher solves
//! the same shape for `http_analytics`; this is the parallel for `requests`.
//!
//! # Failure modes (explicit)
//!
//! Records can be lost in two situations, and there is no dead-letter
//! store — this is a deliberate trade-off:
//!
//!   * **Process crash with records still in-channel**: anything sitting
//!     in the mpsc buffer or pre-batch buffer when the process dies is
//!     gone. Graceful shutdown drains and flushes; SIGKILL or panic does
//!     not. Realtime clients already lost their connection in that
//!     scenario so the missing row is the smaller loss.
//!   * **Sustained fusillade outage**: `flush_batch` retries with
//!     exponential backoff up to `max_retries` (default 3). If every
//!     attempt fails the batch is dropped, logged at `error`, and
//!     `dwctl_background_errors_total{component="responses_writer", reason="flush_drop"}`
//!     increments. There is no requeue or dead-letter table.
//!
//! Both losses are acceptable here because billing and usage accounting
//! read from `http_analytics` and `credit_transactions`, not `requests`.
//! The `requests` table only powers the responses listing and
//! `GET /v1/responses/{id}` polling, where eventual visibility under
//! normal operation is sufficient. If that ever changes, the right fix
//! is either a config-gated panic on drop (for crash-restart recovery)
//! or a dead-letter table — both larger changes than this writer.
//!
//! # Architecture
//!
//! ```text
//! outlet handler
//!     |
//!     | resolve created_by from api_key (one dwctl_pool lookup),
//!     | drop records the api_key doesn't attribute, then
//!     | send(RawCompletedRequest).await   (in-memory mpsc, backpressure to outlet)
//!     v
//! RequestsWriter::run
//!     |
//!     | block on first record (or shutdown)
//!     | try_recv up to batch_size more
//!     | flush_batch: call fusillade::Storage::persist_completed_realtime_batch
//!     | retry on transient errors with exponential backoff
//!     v
//! one fusillade transaction per flush, no dwctl_pool access on the bulk path
//! ```

use crate::metrics::errors::component::RESPONSES_WRITER;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use fusillade::{PersistCompletedRealtimeInput, Storage};
use fusillade_arsenal::PostgresRequestManager;
use metrics::{counter, gauge, histogram};
use sqlx_pool_router::PoolProvider;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, info, info_span, warn};
use uuid::Uuid;

/// Channel capacity. Records sit here when the writer can't keep up; once
/// full, outlet handlers block on `send().await` rather than dropping
/// records. Matches the analytics batcher capacity.
const CHANNEL_BUFFER_SIZE: usize = 10_000;

/// Default maximum retry attempts on transient fusillade errors.
const DEFAULT_MAX_RETRIES: u32 = 3;

/// Default base delay for exponential backoff between retries.
const DEFAULT_RETRY_BASE_DELAY_MS: u64 = 100;

/// One completed-response record sent from the outlet handler to the writer.
///
/// The outlet handler resolves `created_by` from the api_key before sending,
/// so the writer can flush without touching the dwctl pool inside the bulk
/// transaction. Records the outlet handler can't attribute (no api_key, or
/// api_key unknown) are dropped at send time rather than landing here, so
/// `created_by` is always populated.
#[derive(Debug, Clone)]
pub struct RawCompletedRequest {
    /// Pre-generated request UUID (the fusillade row's primary key).
    pub request_id: Uuid,
    /// Upstream HTTP status code from the proxied response.
    pub status_code: u16,
    /// Upstream response body (or synthesized envelope for abandoned requests).
    pub response_body: String,
    /// Original request body. Stored on the synthesized template only on the
    /// INSERT path (non-background realtime); ignored on the UPDATE path.
    pub request_body: String,
    /// Model name from the request.
    pub model: String,
    /// API path (e.g. `/v1/responses`, `/v1/chat/completions`).
    pub endpoint: String,
    /// Bearer token from the Authorization header. Stored on the synthesized
    /// template only on the INSERT path; the daemon never claims these rows
    /// so the upstream call has already used this key.
    pub api_key: String,
    /// Resolved user/org ID for the request (XOR-paired with batch_id on
    /// the fusillade row; required non-empty for batchless rows).
    pub created_by: String,
    /// Wall-clock instant the request arrived, from outlet's request
    /// timestamp. Consulted on the INSERT path only, where it becomes the
    /// synthesized row's `created_at`/`claimed_at`/`started_at`.
    pub started_at: DateTime<Utc>,
    /// Wall-clock instant the response completed (`started_at +` outlet's
    /// measured request duration). Consulted on the INSERT path only, where it
    /// becomes `completed_at`/`failed_at` — so the row's duration reflects the
    /// real latency instead of zero.
    pub completed_at: DateTime<Utc>,
}

/// Sender handle handed to the outlet handler.
pub type RequestsWriterSender = mpsc::Sender<RawCompletedRequest>;

/// Background consumer that batches `RawCompletedRequest`s and flushes them
/// to fusillade in a single transaction per batch.
///
/// Generic over `PoolProvider` so the same struct works in production
/// (`DbPools`) and tests (`TestDbPools`).
pub struct RequestsWriter<P: PoolProvider + Clone + Send + Sync + 'static> {
    request_manager: Arc<PostgresRequestManager<P>>,
    receiver: mpsc::Receiver<RawCompletedRequest>,
    batch_size: usize,
    max_retries: u32,
    retry_base_delay: Duration,
}

impl<P: PoolProvider + Clone + Send + Sync + 'static> RequestsWriter<P> {
    /// Build the writer and return it alongside the sender handle. Spawn the
    /// returned future via `tokio::spawn(writer.run(token))`; pass the sender
    /// into `FusilladeOutletHandler::new`.
    pub fn new(request_manager: Arc<PostgresRequestManager<P>>, batch_size: usize) -> (Self, RequestsWriterSender) {
        let (sender, receiver) = mpsc::channel(CHANNEL_BUFFER_SIZE);
        let writer = Self {
            request_manager,
            receiver,
            batch_size: batch_size.max(1),
            max_retries: DEFAULT_MAX_RETRIES,
            retry_base_delay: Duration::from_millis(DEFAULT_RETRY_BASE_DELAY_MS),
        };
        (writer, sender)
    }

    /// Run the writer until the shutdown token fires or the channel closes.
    ///
    /// Mirrors `AnalyticsBatcher::run`:
    /// 1. Block until at least one record arrives, or shutdown.
    /// 2. Drain the channel up to `batch_size` more records.
    /// 3. Flush the buffer in one fusillade transaction.
    /// 4. Repeat.
    ///
    /// On shutdown, drains the channel and flushes remaining records so
    /// in-flight completions aren't lost on graceful pod termination.
    pub async fn run(mut self, shutdown_token: CancellationToken) {
        info!(
            batch_size = self.batch_size,
            max_retries = self.max_retries,
            "Responses writer started"
        );

        let mut buffer: Vec<RawCompletedRequest> = Vec::with_capacity(self.batch_size);

        loop {
            tokio::select! {
                biased;

                _ = shutdown_token.cancelled() => {
                    // Close first so no new records can be sent, then drain
                    // whatever is already in the channel + the pre-existing
                    // buffer. Anything that arrived between cancel and close
                    // is still drained because `recv` only returns None once
                    // the channel is both closed and empty.
                    let pre_buffered = buffer.len();
                    info!(pre_buffered, "Shutdown signal received, draining responses writer channel");
                    self.receiver.close();
                    let mut drained = 0usize;
                    while let Some(record) = self.receiver.recv().await {
                        buffer.push(record);
                        drained += 1;
                        if buffer.len() >= self.batch_size {
                            self.flush_batch(&mut buffer).await;
                        }
                    }
                    if !buffer.is_empty() {
                        self.flush_batch(&mut buffer).await;
                    }
                    let total_flushed = pre_buffered + drained;
                    counter!("dwctl_requests_writer_shutdown_records_flushed").increment(total_flushed as u64);
                    info!(
                        pre_buffered,
                        drained,
                        total_flushed,
                        "Responses writer shutdown complete"
                    );
                    break;
                }

                maybe_record = self.receiver.recv() => {
                    match maybe_record {
                        Some(record) => buffer.push(record),
                        None => {
                            info!("Responses writer channel closed, shutting down");
                            if !buffer.is_empty() {
                                self.flush_batch(&mut buffer).await;
                            }
                            break;
                        }
                    }
                }
            }

            // Drain whatever else is sitting in the channel, up to batch_size.
            while buffer.len() < self.batch_size {
                match self.receiver.try_recv() {
                    Ok(record) => buffer.push(record),
                    Err(_) => break,
                }
            }

            gauge!("dwctl_requests_writer_channel_depth").set(self.receiver.len() as f64);
            self.flush_batch(&mut buffer).await;
        }
    }

    /// Flush the batch to fusillade in a single transaction. Records arrive
    /// with `created_by` already resolved by the outlet handler, so the
    /// flush path doesn't touch the dwctl pool. Retries on transient errors
    /// with exponential backoff; drops the batch (and increments a metric)
    /// only after all retries are exhausted.
    async fn flush_batch(&self, buffer: &mut Vec<RawCompletedRequest>) {
        if buffer.is_empty() {
            return;
        }

        let batch_size = buffer.len();
        let span = info_span!("dwctl.flush_responses_batch", batch_size);

        async {
            let start = Instant::now();
            histogram!("dwctl_requests_writer_flush_size").record(batch_size as f64);

            let inputs: Vec<PersistCompletedRealtimeInput> = buffer
                .iter()
                .map(|record| PersistCompletedRealtimeInput {
                    request_id: record.request_id,
                    response_body: record.response_body.clone(),
                    status_code: record.status_code,
                    request_body: record.request_body.clone(),
                    model: record.model.clone(),
                    // Loopback base URL is only consulted by the daemon for
                    // non-realtime tiers; realtime rows never get claimed, so
                    // an empty string is correct here.
                    endpoint: String::new(),
                    method: "POST".to_string(),
                    path: record.endpoint.clone(),
                    api_key: record.api_key.clone(),
                    created_by: record.created_by.clone(),
                    started_at: record.started_at,
                    completed_at: record.completed_at,
                })
                .collect();

            let mut last_error = None;
            for attempt in 0..=self.max_retries {
                match self.request_manager.persist_completed_realtime_batch(&inputs).await {
                    Ok(()) => {
                        if attempt > 0 {
                            debug!(attempt, batch_size, "Responses batch flush succeeded after retry");
                            counter!("dwctl_requests_writer_retries_total", "outcome" => "success").increment(1);
                        }
                        last_error = None;
                        break;
                    }
                    Err(e) => {
                        last_error = Some(e);
                        if attempt < self.max_retries {
                            let delay = self.retry_base_delay * 2u32.pow(attempt);
                            warn!(
                                error = %last_error.as_ref().unwrap(),
                                attempt = attempt + 1,
                                max_retries = self.max_retries,
                                delay_ms = delay.as_millis() as u64,
                                batch_size,
                                "Responses batch flush failed, retrying"
                            );
                            counter!("dwctl_requests_writer_retries_total", "outcome" => "retry").increment(1);
                            tokio::time::sleep(delay).await;
                        }
                    }
                }
            }

            if let Some(e) = last_error {
                crate::background_error!(
                    RESPONSES_WRITER, "flush_drop", Error,
                    error = %e,
                    batch_size,
                    attempts = self.max_retries + 1,
                    "Failed to flush responses batch after all retries, dropping batch"
                );
                buffer.clear();
                return;
            }

            let duration = start.elapsed();
            histogram!("dwctl_requests_writer_flush_duration_seconds").record(duration.as_secs_f64());
            counter!("dwctl_requests_writer_records_total").increment(batch_size as u64);

            debug!(batch_size, duration_ms = duration.as_millis() as u64, "Flushed responses batch");

            buffer.clear();
        }
        .instrument(span)
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fusillade::RequestId;
    use fusillade::ReqwestHttpClient;
    use fusillade_arsenal::PostgresRequestManager;
    use sqlx_pool_router::TestDbPools;
    use std::time::Duration;
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    /// Builds a writer wired to a fresh `#[sqlx::test]` pool with the
    /// fusillade schema installed via `fusillade_arsenal::migrator()` (so we don't
    /// reference the fusillade source directory, which doesn't exist in
    /// CI). The returned request manager runs against a pool scoped to
    /// the fusillade schema. Tests pass `created_by` directly on each
    /// record; the outlet handler is the part that resolves attribution
    /// from an api_key in production.
    async fn build_writer(
        pool: sqlx::PgPool,
    ) -> (
        RequestsWriter<TestDbPools>,
        RequestsWriterSender,
        Arc<PostgresRequestManager<TestDbPools>>,
    ) {
        let fusillade_pool = crate::test::utils::setup_fusillade_pool(&pool).await;
        let pools = TestDbPools::new(fusillade_pool).await.unwrap();
        let http_client = Arc::new(ReqwestHttpClient::default());
        let manager = Arc::new(PostgresRequestManager::with_client(pools, http_client));
        let (writer, sender) = RequestsWriter::new(manager.clone(), 8);
        (writer, sender, manager)
    }

    /// Poll fusillade until the request row appears in 'completed' state, or
    /// time out. Mirrors the polling-not-sleeping pattern fusillade uses
    /// (see fusillade/CLAUDE.md).
    async fn wait_until_completed(
        manager: &PostgresRequestManager<TestDbPools>,
        request_id: Uuid,
        timeout_secs: u64,
    ) -> fusillade::RequestDetail {
        let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
        loop {
            match Storage::get_request_detail(manager, RequestId(request_id)).await {
                Ok(detail) if detail.status == "completed" => return detail,
                Ok(_) | Err(fusillade::FusilladeError::RequestNotFound(_)) => {}
                Err(e) => panic!("get_request_detail failed: {e}"),
            }
            if std::time::Instant::now() >= deadline {
                panic!("timed out waiting for request {request_id} to complete");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[sqlx::test]
    async fn test_writer_persists_completed_record(pool: sqlx::PgPool) {
        let (writer, sender, manager) = build_writer(pool).await;
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(writer.run(shutdown.clone()));

        let request_id = Uuid::new_v4();
        // Real timing measured by the outlet handler: arrival, then completion
        // 2s later. The writer must carry it through so the persisted row's
        // duration is the true latency, not zero. Fixed, microsecond-aligned
        // instants: Postgres timestamptz is microsecond-precision, so a
        // nanosecond Utc::now() would not round-trip byte-for-byte and the
        // completed_at equality assert below would be flaky.
        let started_at = DateTime::from_timestamp_millis(1_700_000_000_000).unwrap();
        let completed_at = started_at + chrono::Duration::seconds(2);
        sender
            .send(RawCompletedRequest {
                request_id,
                status_code: 200,
                response_body: r#"{"output":"done"}"#.to_string(),
                request_body: r#"{"input":"hi"}"#.to_string(),
                model: "gpt-4".to_string(),
                endpoint: "/v1/responses".to_string(),
                api_key: String::new(),
                created_by: "user-test".to_string(),
                started_at,
                completed_at,
            })
            .await
            .expect("send should succeed");

        let detail = wait_until_completed(&manager, request_id, 5).await;
        assert_eq!(detail.status, "completed");
        assert_eq!(detail.service_tier, Some("priority".to_string()));
        assert_eq!(detail.response_body, Some(r#"{"output":"done"}"#.to_string()));
        assert_eq!(detail.response_status, Some(200));
        // Regression: the outlet-measured duration survives the writer round-trip.
        assert_eq!(detail.completed_at, Some(completed_at));
        let duration_ms = detail.duration_ms.expect("duration_ms should be populated");
        assert!(
            (duration_ms - 2000.0).abs() < 1.0,
            "duration_ms should be ~2000 (real latency), got {duration_ms}"
        );

        shutdown.cancel();
        timeout(Duration::from_secs(5), handle)
            .await
            .expect("writer should shut down within 5s")
            .expect("writer task should not panic");
    }

    #[sqlx::test]
    async fn test_writer_batches_multiple_records_in_one_flush(pool: sqlx::PgPool) {
        // Send N records faster than the writer can flush so they buffer up;
        // confirm all N land in the DB. Using batch_size=8 (from build_writer)
        // and 5 records, all should be visible after a single flush.
        let (writer, sender, manager) = build_writer(pool).await;
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(writer.run(shutdown.clone()));

        let request_ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        for id in &request_ids {
            sender
                .send(RawCompletedRequest {
                    request_id: *id,
                    status_code: 200,
                    response_body: format!(r#"{{"output":"{id}"}}"#),
                    request_body: r#"{"input":"hi"}"#.to_string(),
                    model: "gpt-4".to_string(),
                    endpoint: "/v1/responses".to_string(),
                    api_key: String::new(),
                    created_by: "user-test".to_string(),
                    started_at: Utc::now(),
                    completed_at: Utc::now(),
                })
                .await
                .expect("send should succeed");
        }

        for id in &request_ids {
            let detail = wait_until_completed(&manager, *id, 5).await;
            assert_eq!(detail.status, "completed");
        }

        shutdown.cancel();
        timeout(Duration::from_secs(5), handle)
            .await
            .expect("writer should shut down within 5s")
            .expect("writer task should not panic");
    }

    #[sqlx::test]
    async fn test_writer_drains_channel_on_shutdown(pool: sqlx::PgPool) {
        // Exercises the shutdown-cancellation arm specifically: cancel the
        // token while the sender is still alive, so `run` exits via
        // `shutdown_token.cancelled()` (which closes the receiver and
        // drains) rather than via the channel-closed arm. Records sent
        // before cancel must still land in fusillade.
        let (writer, sender, manager) = build_writer(pool).await;
        let shutdown = CancellationToken::new();
        let handle = tokio::spawn(writer.run(shutdown.clone()));

        let request_id = Uuid::new_v4();
        sender
            .send(RawCompletedRequest {
                request_id,
                status_code: 200,
                response_body: r#"{"output":"shutdown-test"}"#.to_string(),
                request_body: r#"{"input":"hi"}"#.to_string(),
                model: "gpt-4".to_string(),
                endpoint: "/v1/responses".to_string(),
                api_key: String::new(),
                created_by: "user-test".to_string(),
                started_at: Utc::now(),
                completed_at: Utc::now(),
            })
            .await
            .expect("send should succeed");

        // Cancel BEFORE dropping the sender — this routes the writer
        // through the `shutdown_token.cancelled()` branch (which then
        // calls `receiver.close()` itself), which is the path we want
        // under test. Keep the sender alive so the writer can't exit
        // via the channel-closed arm by accident.
        shutdown.cancel();

        timeout(Duration::from_secs(5), handle)
            .await
            .expect("writer should shut down within 5s")
            .expect("writer task should not panic");

        // Verify the record landed (writer drained + flushed via the
        // shutdown path).
        let detail = Storage::get_request_detail(&*manager, RequestId(request_id))
            .await
            .expect("request should exist after drain");
        assert_eq!(detail.status, "completed");
        assert_eq!(detail.response_body, Some(r#"{"output":"shutdown-test"}"#.to_string()));

        // Sender outlives the writer task to guarantee we didn't exit via
        // the channel-closed arm. Drop it here explicitly to make the
        // sequence intent clear.
        drop(sender);
    }
}
