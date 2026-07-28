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
//! flex durability is owned by the fusillade daemon. Create commands are
//! acknowledged only after their transaction commits so upstream dispatch can
//! depend on durable admission. Realtime completion remains best-effort because
//! the client connection has already ended when the outlet observes it.
//!
//! # Failure modes (explicit)
//!
//! Completion records can be lost in two situations, and there is no
//! dead-letter store — this is a deliberate trade-off:
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
//! Completion losses are acceptable here because billing and usage accounting
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
//! request paths / outlet handler
//!     |
//!     | create(...).await -> commit acknowledgement
//!     | complete_realtime(...).await -> in-memory admission only
//!     v
//! RequestsWriter::run
//!     |
//!     | block on first record (or shutdown)
//!     | collect until batch_size or max_linger
//!     | flush creates and completions in independent transactions
//!     | retry only transient database errors with exponential backoff
//!     v
//! fusillade storage, no dwctl_pool access on the bulk path
//! ```

use crate::metrics::errors::component::RESPONSES_WRITER;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use fusillade::{CreateFlexInput, CreateRealtimeInput, CreateResponseInput, PersistCompletedRealtimeInput, RequestId, Storage};
use fusillade_arsenal::{PostgresRequestManager, is_retryable_db_error};
use metrics::{counter, gauge, histogram};
use sqlx_pool_router::PoolProvider;
use tokio::sync::{mpsc, oneshot};
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

enum ResponseWriteCommand {
    Create {
        input: CreateResponseInput,
        ack: oneshot::Sender<fusillade::Result<RequestId>>,
    },
    CompleteRealtime(RawCompletedRequest),
}

struct QueuedCommand {
    enqueued_at: Instant,
    command: ResponseWriteCommand,
}

type PendingCreate = (Instant, CreateResponseInput, oneshot::Sender<fusillade::Result<RequestId>>);

#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct RequestsWriterTestObserver {
    create_transaction_attempts: Arc<AtomicUsize>,
    create_retry_backoffs: Arc<AtomicUsize>,
}

#[cfg(test)]
impl RequestsWriterTestObserver {
    pub(crate) fn create_transaction_attempts(&self) -> usize {
        self.create_transaction_attempts.load(Ordering::SeqCst)
    }

    pub(crate) fn create_retry_backoffs(&self) -> usize {
        self.create_retry_backoffs.load(Ordering::SeqCst)
    }
}

/// Cloneable producer handle for durable creates and best-effort completions.
#[derive(Clone)]
pub struct RequestsWriterHandle {
    sender: mpsc::Sender<QueuedCommand>,
    shutdown_token: Arc<OnceLock<CancellationToken>>,
    #[cfg(test)]
    test_observer: RequestsWriterTestObserver,
}

impl RequestsWriterHandle {
    #[cfg(test)]
    pub(crate) fn test_observer(&self) -> RequestsWriterTestObserver {
        self.test_observer.clone()
    }

    #[cfg(test)]
    pub(crate) fn queued_commands(&self) -> usize {
        self.sender.max_capacity() - self.sender.capacity()
    }

    pub async fn admit_flex(&self, input: CreateFlexInput) -> fusillade::Result<RequestId> {
        self.create(CreateResponseInput::Flex(input)).await
    }

    pub async fn admit_realtime(&self, input: CreateRealtimeInput) -> fusillade::Result<RequestId> {
        self.create(CreateResponseInput::Realtime(input)).await
    }

    async fn create(&self, input: CreateResponseInput) -> fusillade::Result<RequestId> {
        let (ack, receiver) = oneshot::channel();
        let command = QueuedCommand {
            enqueued_at: Instant::now(),
            command: ResponseWriteCommand::Create { input, ack },
        };
        if self.send(command).await.is_err() {
            counter!("dwctl_requests_writer_create_ack_total", "outcome" => "error").increment(1);
            return Err(writer_unavailable());
        }
        receiver.await.unwrap_or_else(|_| {
            counter!("dwctl_requests_writer_create_ack_total", "outcome" => "error").increment(1);
            Err(writer_unavailable())
        })
    }

    pub async fn complete_realtime(&self, input: RawCompletedRequest) -> fusillade::Result<()> {
        self.send(QueuedCommand {
            enqueued_at: Instant::now(),
            command: ResponseWriteCommand::CompleteRealtime(input),
        })
        .await
    }

    async fn send(&self, command: QueuedCommand) -> fusillade::Result<()> {
        let Some(shutdown_token) = self.shutdown_token.get().cloned() else {
            return self.sender.send(command).await.map_err(|_| writer_unavailable());
        };
        if shutdown_token.is_cancelled() {
            return Err(writer_unavailable());
        }

        let permit = tokio::select! {
            biased;
            _ = shutdown_token.cancelled() => return Err(writer_unavailable()),
            permit = self.sender.reserve() => permit.map_err(|_| writer_unavailable())?,
        };
        if shutdown_token.is_cancelled() {
            return Err(writer_unavailable());
        }
        permit.send(command);
        Ok(())
    }
}

fn writer_unavailable() -> fusillade::FusilladeError {
    fusillade::FusilladeError::Other(anyhow::anyhow!("responses writer unavailable"))
}

/// Background consumer that batches `RawCompletedRequest`s and flushes them
/// to fusillade in a single transaction per batch.
///
/// Generic over `PoolProvider` so the same struct works in production
/// (`DbPools`) and tests (`TestDbPools`).
pub struct RequestsWriter<P: PoolProvider + Clone + Send + Sync + 'static> {
    request_manager: Arc<PostgresRequestManager<P>>,
    receiver: mpsc::Receiver<QueuedCommand>,
    shutdown_token: Arc<OnceLock<CancellationToken>>,
    batch_size: usize,
    max_linger: Duration,
    max_retries: u32,
    retry_base_delay: Duration,
    #[cfg(test)]
    test_observer: RequestsWriterTestObserver,
}

impl<P: PoolProvider + Clone + Send + Sync + 'static> RequestsWriter<P> {
    /// Build the writer and return it alongside the sender handle. Spawn the
    /// returned future via `tokio::spawn(writer.run(token))`; share the handle
    /// with request admission and `FusilladeOutletHandler`.
    pub fn new(request_manager: Arc<PostgresRequestManager<P>>, batch_size: usize, max_linger: Duration) -> (Self, RequestsWriterHandle) {
        let (sender, receiver) = mpsc::channel(CHANNEL_BUFFER_SIZE);
        let shutdown_token = Arc::new(OnceLock::new());
        #[cfg(test)]
        let test_observer = RequestsWriterTestObserver::default();
        let writer = Self {
            request_manager,
            receiver,
            shutdown_token: shutdown_token.clone(),
            batch_size: batch_size.max(1),
            max_linger,
            max_retries: DEFAULT_MAX_RETRIES,
            retry_base_delay: Duration::from_millis(DEFAULT_RETRY_BASE_DELAY_MS),
            #[cfg(test)]
            test_observer: test_observer.clone(),
        };
        (
            writer,
            RequestsWriterHandle {
                sender,
                shutdown_token,
                #[cfg(test)]
                test_observer,
            },
        )
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
    pub fn run(self, shutdown_token: CancellationToken) -> impl std::future::Future<Output = ()> {
        let _ = self.shutdown_token.set(shutdown_token.clone());
        self.run_inner(shutdown_token)
    }

    async fn run_inner(mut self, shutdown_token: CancellationToken) {
        info!(
            batch_size = self.batch_size,
            max_retries = self.max_retries,
            "Responses writer started"
        );

        let mut buffer: Vec<QueuedCommand> = Vec::with_capacity(self.batch_size);

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
                    while let Some(command) = self.receiver.recv().await {
                        buffer.push(command);
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

                maybe_command = self.receiver.recv() => {
                    match maybe_command {
                        Some(command) => buffer.push(command),
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

            let deadline = tokio::time::Instant::now() + self.max_linger;
            let mut shutdown_during_linger = false;
            let mut channel_closed = false;
            while buffer.len() < self.batch_size {
                if self.max_linger.is_zero() {
                    while buffer.len() < self.batch_size {
                        match self.receiver.try_recv() {
                            Ok(command) => buffer.push(command),
                            Err(_) => break,
                        }
                    }
                    break;
                }
                tokio::select! {
                    biased;
                    _ = shutdown_token.cancelled() => {
                        self.receiver.close();
                        shutdown_during_linger = true;
                        break;
                    }
                    maybe_command = self.receiver.recv() => match maybe_command {
                        Some(command) => buffer.push(command),
                        None => {
                            channel_closed = true;
                            break;
                        }
                    },
                    _ = tokio::time::sleep_until(deadline) => break,
                }
            }

            gauge!("dwctl_requests_writer_channel_depth").set(self.receiver.len() as f64);
            let flushed_before_shutdown = buffer.len();
            self.flush_batch(&mut buffer).await;

            if shutdown_during_linger {
                self.receiver.close();
                let mut drained = 0usize;
                while let Some(command) = self.receiver.recv().await {
                    buffer.push(command);
                    drained += 1;
                    if buffer.len() >= self.batch_size {
                        self.flush_batch(&mut buffer).await;
                    }
                }
                if !buffer.is_empty() {
                    self.flush_batch(&mut buffer).await;
                }
                counter!("dwctl_requests_writer_shutdown_records_flushed").increment((flushed_before_shutdown + drained) as u64);
                break;
            }
            if channel_closed {
                break;
            }
        }
    }

    /// Flush the batch to fusillade in a single transaction. Records normally
    /// arrive with `created_by` already resolved by the caller, so the flush
    /// path doesn't touch the dwctl pool. Invalid create commands are rejected
    /// individually before the valid commands enter the shared transaction.
    /// Retries on transient errors with exponential backoff; drops the batch
    /// (and increments a metric) only after all retries are exhausted.
    async fn flush_batch(&self, buffer: &mut Vec<QueuedCommand>) {
        if buffer.is_empty() {
            return;
        }

        let batch_size = buffer.len();
        let span = info_span!("dwctl.flush_responses_batch", batch_size);

        async {
            let mut creates = Vec::new();
            let mut completions = Vec::new();
            for queued in buffer.drain(..) {
                match queued.command {
                    ResponseWriteCommand::Create { input, ack } => {
                        creates.push((queued.enqueued_at, input, ack));
                    }
                    ResponseWriteCommand::CompleteRealtime(record) => {
                        completions.push((queued.enqueued_at, record));
                    }
                }
            }

            self.flush_creates(creates).await;
            self.flush_completions(completions).await;
        }
        .instrument(span)
        .await;
    }

    async fn flush_creates(&self, mut creates: Vec<PendingCreate>) {
        if creates.is_empty() {
            return;
        }
        let submitted_batch_size = creates.len();
        let start = Instant::now();
        histogram!("dwctl_requests_writer_flush_size", "command" => "create").record(submitted_batch_size as f64);
        for (enqueued_at, _, _) in &creates {
            histogram!("dwctl_requests_writer_queue_wait_duration_seconds", "command" => "create")
                .record(enqueued_at.elapsed().as_secs_f64());
        }

        discard_canceled_creates(&mut creates);
        let mut valid_creates = Vec::with_capacity(submitted_batch_size);
        for (enqueued_at, input, ack) in creates {
            if create_input_created_by(&input).trim().is_empty() {
                let _ = ack.send(Err(invalid_create_owner()));
                counter!("dwctl_requests_writer_create_ack_total", "outcome" => "error").increment(1);
            } else {
                valid_creates.push((enqueued_at, input, ack));
            }
        }

        if valid_creates.is_empty() {
            histogram!("dwctl_requests_writer_flush_duration_seconds", "command" => "create").record(start.elapsed().as_secs_f64());
            return;
        }

        let result = self.retry_create_batch(&mut valid_creates).await;
        let batch_size = valid_creates.len();
        if valid_creates.is_empty() {
            histogram!("dwctl_requests_writer_flush_duration_seconds", "command" => "create").record(start.elapsed().as_secs_f64());
            return;
        }

        match result {
            Ok(ids) if ids.len() == valid_creates.len() => {
                for ((_, _, ack), id) in valid_creates.into_iter().zip(ids) {
                    let _ = ack.send(Ok(id));
                    counter!("dwctl_requests_writer_create_ack_total", "outcome" => "success").increment(1);
                }
                counter!("dwctl_requests_writer_records_total", "command" => "create").increment(batch_size as u64);
            }
            Ok(ids) => {
                crate::background_error!(
                    RESPONSES_WRITER,
                    "create_result_mismatch",
                    Error,
                    expected = batch_size,
                    actual = ids.len(),
                    "Responses writer create batch returned an unexpected number of IDs"
                );
                fan_out_create_error(
                    valid_creates,
                    "responses writer create failed: storage returned an invalid acknowledgement count",
                );
            }
            Err(error) => {
                let safe_error = redacted_create_error(&error);
                crate::background_error!(
                    RESPONSES_WRITER, "create_flush_failed", Error,
                    error = %error,
                    batch_size,
                    "Failed to flush responses create batch"
                );
                fan_out_create_error(valid_creates, &safe_error);
            }
        }
        histogram!("dwctl_requests_writer_flush_duration_seconds", "command" => "create").record(start.elapsed().as_secs_f64());
    }

    async fn retry_create_batch(&self, creates: &mut Vec<PendingCreate>) -> fusillade::Result<Vec<RequestId>> {
        for attempt in 0..=self.max_retries {
            // The admission future owns the acknowledgement receiver. If it
            // has gone away, persisting the create would leave an orphaned
            // request that no caller can dispatch or complete. Re-check at
            // every attempt because the caller can disappear during retry
            // backoff as well as while the command is queued.
            discard_canceled_creates(creates);
            if creates.is_empty() {
                return Ok(Vec::new());
            }
            let inputs: Vec<_> = creates.iter().map(|(_, input, _)| input.clone()).collect();
            #[cfg(test)]
            self.test_observer.create_transaction_attempts.fetch_add(1, Ordering::SeqCst);
            match self.request_manager.create_responses_batch(&inputs).await {
                Ok(ids) => {
                    if attempt > 0 {
                        counter!("dwctl_requests_writer_retries_total", "command" => "create", "outcome" => "success").increment(1);
                    }
                    return Ok(ids);
                }
                Err(error) if attempt < self.max_retries && is_retryable_db_error(&error) => {
                    // Avoid an otherwise unnecessary backoff when every
                    // waiter was canceled while this attempt was failing.
                    discard_canceled_creates(creates);
                    if creates.is_empty() {
                        return Ok(Vec::new());
                    }
                    counter!("dwctl_requests_writer_retries_total", "command" => "create", "outcome" => "retry").increment(1);
                    #[cfg(test)]
                    self.test_observer.create_retry_backoffs.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(self.retry_base_delay * 2u32.pow(attempt)).await;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("bounded retry loop always returns")
    }

    async fn flush_completions(&self, completions: Vec<(Instant, RawCompletedRequest)>) {
        if completions.is_empty() {
            return;
        }
        let batch_size = completions.len();
        let start = Instant::now();
        histogram!("dwctl_requests_writer_flush_size", "command" => "complete").record(batch_size as f64);
        for (enqueued_at, _) in &completions {
            histogram!("dwctl_requests_writer_queue_wait_duration_seconds", "command" => "complete")
                .record(enqueued_at.elapsed().as_secs_f64());
        }

        let inputs: Vec<PersistCompletedRealtimeInput> = completions
            .iter()
            .map(|(_, record)| PersistCompletedRealtimeInput {
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
        let mut attempts = 0;
        for attempt in 0..=self.max_retries {
            attempts += 1;
            match self.request_manager.persist_completed_realtime_batch(&inputs).await {
                Ok(()) => {
                    if attempt > 0 {
                        debug!(attempt, batch_size, "Responses batch flush succeeded after retry");
                        counter!("dwctl_requests_writer_retries_total", "command" => "complete", "outcome" => "success").increment(1);
                    }
                    last_error = None;
                    break;
                }
                Err(e) => {
                    last_error = Some(e);
                    if attempt < self.max_retries && is_retryable_db_error(last_error.as_ref().unwrap()) {
                        let delay = self.retry_base_delay * 2u32.pow(attempt);
                        warn!(
                            error = %last_error.as_ref().unwrap(),
                            attempt = attempt + 1,
                            max_retries = self.max_retries,
                            delay_ms = delay.as_millis() as u64,
                            batch_size,
                            "Responses batch flush failed, retrying"
                        );
                        counter!("dwctl_requests_writer_retries_total", "command" => "complete", "outcome" => "retry").increment(1);
                        tokio::time::sleep(delay).await;
                    } else {
                        break;
                    }
                }
            }
        }

        let duration = start.elapsed();
        histogram!("dwctl_requests_writer_flush_duration_seconds", "command" => "complete").record(duration.as_secs_f64());
        if let Some(e) = last_error {
            crate::background_error!(
                RESPONSES_WRITER, "flush_drop", Error,
                error = %e,
                batch_size,
                attempts,
                "Failed to flush responses batch after all retries, dropping batch"
            );
            return;
        }

        counter!("dwctl_requests_writer_records_total", "command" => "complete").increment(batch_size as u64);

        debug!(batch_size, duration_ms = duration.as_millis() as u64, "Flushed responses batch");
    }
}

fn create_input_created_by(input: &CreateResponseInput) -> &str {
    match input {
        CreateResponseInput::Flex(input) => &input.created_by,
        CreateResponseInput::Realtime(input) => &input.created_by,
    }
}

fn invalid_create_owner() -> fusillade::FusilladeError {
    fusillade::FusilladeError::ValidationError("response lifecycle create requires non-empty created_by".to_string())
}

fn discard_canceled_creates(creates: &mut Vec<PendingCreate>) {
    let submitted = creates.len();
    creates.retain(|(_, _, ack)| !ack.is_closed());
    let discarded = submitted - creates.len();
    if discarded > 0 {
        counter!("dwctl_requests_writer_create_ack_total", "outcome" => "canceled").increment(discarded as u64);
        debug!(
            discarded,
            remaining = creates.len(),
            "Discarded response creates whose callers were canceled"
        );
    }
}

fn redacted_create_error(error: &fusillade::FusilladeError) -> String {
    let kind = match error {
        fusillade::FusilladeError::ValidationError(_) => "validation error",
        fusillade::FusilladeError::RequestStateConflict { .. } | fusillade::FusilladeError::InvalidState(_, _, _) => "state conflict",
        fusillade::FusilladeError::Shutdown => "storage unavailable",
        _ => "storage error",
    };
    format!("responses writer create failed: {kind}")
}

fn fan_out_create_error(creates: Vec<PendingCreate>, safe_message: &str) {
    for (_, _, ack) in creates {
        let _ = ack.send(Err(fusillade::FusilladeError::Other(anyhow::anyhow!(safe_message.to_string()))));
        counter!("dwctl_requests_writer_create_ack_total", "outcome" => "error").increment(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fusillade::ReqwestHttpClient;
    use fusillade::{CreateFlexInput, CreateRealtimeInput, RequestId};
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
        RequestsWriterHandle,
        Arc<PostgresRequestManager<TestDbPools>>,
    ) {
        let (writer, handle, manager, _) = build_writer_with_pool(pool).await;
        (writer, handle, manager)
    }

    async fn build_writer_with_pool(
        pool: sqlx::PgPool,
    ) -> (
        RequestsWriter<TestDbPools>,
        RequestsWriterHandle,
        Arc<PostgresRequestManager<TestDbPools>>,
        sqlx::PgPool,
    ) {
        let fusillade_pool = crate::test::utils::setup_fusillade_pool(&pool).await;
        let pools = TestDbPools::new(fusillade_pool.clone()).await.unwrap();
        let http_client = Arc::new(ReqwestHttpClient::default());
        let manager = Arc::new(PostgresRequestManager::with_client(pools, http_client));
        let (writer, handle) = RequestsWriter::new(manager.clone(), 8, Duration::ZERO);
        (writer, handle, manager, fusillade_pool)
    }

    fn flex_input(request_id: Uuid, created_by: &str) -> CreateFlexInput {
        CreateFlexInput {
            request_id,
            body: r#"{"input":"flex"}"#.to_string(),
            model: "flex-model".to_string(),
            endpoint: "http://flex.example".to_string(),
            method: "POST".to_string(),
            path: "/v1/responses".to_string(),
            api_key: "flex-key".to_string(),
            created_by: created_by.to_string(),
        }
    }

    fn realtime_input(request_id: Uuid, created_by: &str) -> CreateRealtimeInput {
        CreateRealtimeInput {
            request_id,
            body: r#"{"input":"realtime"}"#.to_string(),
            model: "realtime-model".to_string(),
            endpoint: "http://realtime.example".to_string(),
            method: "POST".to_string(),
            path: "/v1/responses".to_string(),
            api_key: "realtime-key".to_string(),
            created_by: created_by.to_string(),
        }
    }

    fn completed_record(request_id: Uuid, created_by: &str) -> RawCompletedRequest {
        RawCompletedRequest {
            request_id,
            status_code: 200,
            response_body: r#"{"output":"done"}"#.to_string(),
            request_body: r#"{"input":"hi"}"#.to_string(),
            model: "gpt-4".to_string(),
            endpoint: "/v1/responses".to_string(),
            api_key: "test-key".to_string(),
            created_by: created_by.to_string(),
            started_at: Utc::now(),
            completed_at: Utc::now(),
        }
    }

    #[sqlx::test]
    async fn create_acknowledgements_wait_for_committed_flush(pool: sqlx::PgPool) {
        let (writer, handle, manager) = build_writer(pool).await;
        let flex_id = Uuid::new_v4();
        let realtime_id = Uuid::new_v4();

        let flex_ack = tokio::spawn({
            let handle = handle.clone();
            async move { handle.admit_flex(flex_input(flex_id, "flex-owner")).await }
        });
        let realtime_ack = tokio::spawn({
            let handle = handle.clone();
            async move { handle.admit_realtime(realtime_input(realtime_id, "realtime-owner")).await }
        });
        tokio::task::yield_now().await;

        assert!(!flex_ack.is_finished());
        assert!(!realtime_ack.is_finished());
        assert!(matches!(
            Storage::get_request_detail(&*manager, RequestId(flex_id)).await,
            Err(fusillade::FusilladeError::RequestNotFound(_))
        ));

        let shutdown = CancellationToken::new();
        let writer_task = tokio::spawn(writer.run(shutdown.clone()));
        assert_eq!(flex_ack.await.unwrap().unwrap(), RequestId(flex_id));
        assert_eq!(realtime_ack.await.unwrap().unwrap(), RequestId(realtime_id));

        shutdown.cancel();
        writer_task.await.unwrap();
    }

    #[sqlx::test]
    async fn create_canceled_before_writer_start_is_not_persisted(pool: sqlx::PgPool) {
        let (writer, handle, _manager, fusillade_pool) = build_writer_with_pool(pool).await;
        let request_id = Uuid::new_v4();
        let acknowledgement = tokio::spawn({
            let handle = handle.clone();
            async move { handle.admit_realtime(realtime_input(request_id, "canceled-owner")).await }
        });
        timeout(Duration::from_secs(1), async {
            while handle.queued_commands() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("create should enter the channel before its caller is canceled");

        acknowledgement.abort();
        assert!(
            acknowledgement.await.unwrap_err().is_cancelled(),
            "aborting the caller must close the create acknowledgement receiver"
        );

        let shutdown = CancellationToken::new();
        let writer_task = tokio::spawn(writer.run(shutdown.clone()));
        timeout(Duration::from_secs(1), async {
            while handle.queued_commands() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("writer should drain the canceled create");
        shutdown.cancel();
        writer_task.await.unwrap();

        let request_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM requests")
            .fetch_one(&fusillade_pool)
            .await
            .unwrap();
        let template_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_templates")
            .fetch_one(&fusillade_pool)
            .await
            .unwrap();
        assert_eq!((request_count, template_count), (0, 0));
    }

    #[sqlx::test]
    async fn create_canceled_during_retry_backoff_is_not_retried_or_persisted(pool: sqlx::PgPool) {
        let migrated_pool = crate::test::utils::setup_fusillade_pool(&pool).await;
        let constrained_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .min_connections(0)
            .acquire_timeout(Duration::from_millis(25))
            .connect_with(migrated_pool.connect_options().as_ref().clone())
            .await
            .unwrap();
        let pools = TestDbPools::new(constrained_pool.clone()).await.unwrap();
        let manager = Arc::new(
            PostgresRequestManager::with_client(pools, Arc::new(ReqwestHttpClient::default()))
                .with_db_retry_config(fusillade_arsenal::DbRetryConfig::disabled()),
        );
        let (mut writer, handle) = RequestsWriter::new(manager, 8, Duration::ZERO);
        writer.max_retries = 1;
        writer.retry_base_delay = Duration::from_millis(500);
        let observer = handle.test_observer();

        let pool_guard = constrained_pool.acquire().await.unwrap();
        let acknowledgement = tokio::spawn({
            let handle = handle.clone();
            async move { handle.admit_realtime(realtime_input(Uuid::new_v4(), "canceled-owner")).await }
        });
        let shutdown = CancellationToken::new();
        let writer_task = tokio::spawn(writer.run(shutdown.clone()));

        timeout(Duration::from_secs(1), async {
            while observer.create_retry_backoffs() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the exhausted pool should put the create into retry backoff");

        acknowledgement.abort();
        assert!(
            acknowledgement.await.unwrap_err().is_cancelled(),
            "aborting during backoff must close the acknowledgement receiver"
        );
        drop(pool_guard);
        shutdown.cancel();
        timeout(Duration::from_secs(2), writer_task)
            .await
            .expect("writer should finish the bounded backoff")
            .unwrap();

        assert_eq!(
            observer.create_transaction_attempts(),
            1,
            "a create whose caller canceled during backoff must not make a second DB attempt"
        );
        let request_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM requests")
            .fetch_one(&constrained_pool)
            .await
            .unwrap();
        let template_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_templates")
            .fetch_one(&constrained_pool)
            .await
            .unwrap();
        assert_eq!((request_count, template_count), (0, 0));
    }

    #[sqlx::test]
    async fn create_batch_acknowledges_every_create_in_original_order(pool: sqlx::PgPool) {
        let (writer, handle, _manager) = build_writer(pool).await;
        let ids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        let acknowledgements = ids.map(|id| {
            let handle = handle.clone();
            tokio::spawn(async move { handle.admit_realtime(realtime_input(id, "owner")).await })
        });
        tokio::task::yield_now().await;

        let shutdown = CancellationToken::new();
        let writer_task = tokio::spawn(writer.run(shutdown.clone()));
        let mut returned = Vec::new();
        for acknowledgement in acknowledgements {
            returned.push(acknowledgement.await.unwrap().unwrap());
        }
        assert_eq!(returned, ids.map(RequestId));

        shutdown.cancel();
        writer_task.await.unwrap();
    }

    #[sqlx::test]
    async fn invalid_create_does_not_poison_valid_create_or_completion(pool: sqlx::PgPool) {
        let (mut writer, handle, manager) = build_writer(pool).await;
        writer.max_retries = 0;
        let valid_id = Uuid::new_v4();
        let invalid_id = Uuid::new_v4();
        let completion_id = Uuid::new_v4();

        let valid_ack = tokio::spawn({
            let handle = handle.clone();
            async move { handle.admit_flex(flex_input(valid_id, "owner")).await }
        });
        let invalid_ack = tokio::spawn({
            let handle = handle.clone();
            async move { handle.admit_realtime(realtime_input(invalid_id, " ")).await }
        });
        handle.complete_realtime(completed_record(completion_id, "owner")).await.unwrap();
        tokio::task::yield_now().await;

        let shutdown = CancellationToken::new();
        let writer_task = tokio::spawn(writer.run(shutdown.clone()));
        assert_eq!(valid_ack.await.unwrap().unwrap(), RequestId(valid_id));
        let invalid_error = invalid_ack.await.unwrap().unwrap_err();
        assert!(matches!(
            invalid_error,
            fusillade::FusilladeError::ValidationError(message)
                if message == "response lifecycle create requires non-empty created_by"
        ));
        assert_eq!(
            Storage::get_request_detail(&*manager, RequestId(valid_id)).await.unwrap().status,
            "pending"
        );
        assert_eq!(wait_until_completed(&manager, completion_id, 5).await.status, "completed");

        shutdown.cancel();
        writer_task.await.unwrap();
    }

    #[sqlx::test]
    async fn deterministic_create_failure_is_not_retried(pool: sqlx::PgPool) {
        let (mut writer, handle, _manager) = build_writer(pool).await;
        writer.max_retries = 3;
        writer.retry_base_delay = Duration::from_secs(5);
        let acknowledgement = tokio::spawn(async move { handle.admit_realtime(realtime_input(Uuid::new_v4(), " ")).await });
        tokio::task::yield_now().await;

        let shutdown = CancellationToken::new();
        let writer_task = tokio::spawn(writer.run(shutdown.clone()));
        let result = timeout(Duration::from_secs(1), acknowledgement)
            .await
            .expect("deterministic validation failure must not enter retry backoff")
            .unwrap();
        assert!(result.is_err());

        shutdown.cancel();
        writer_task.await.unwrap();
    }

    #[sqlx::test]
    async fn completion_batch_failure_is_dropped_and_writer_continues(pool: sqlx::PgPool) {
        let (mut writer, handle, manager) = build_writer(pool).await;
        writer.max_retries = 0;
        let dropped_id = Uuid::new_v4();
        let persisted_id = Uuid::new_v4();
        handle.complete_realtime(completed_record(dropped_id, " ")).await.unwrap();

        let shutdown = CancellationToken::new();
        let writer_task = tokio::spawn(writer.run(shutdown.clone()));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(matches!(
            Storage::get_request_detail(&*manager, RequestId(dropped_id)).await,
            Err(fusillade::FusilladeError::RequestNotFound(_))
        ));

        handle.complete_realtime(completed_record(persisted_id, "owner")).await.unwrap();
        assert_eq!(wait_until_completed(&manager, persisted_id, 5).await.status, "completed");
        shutdown.cancel();
        writer_task.await.unwrap();
    }

    #[sqlx::test]
    async fn graceful_shutdown_drains_creates_and_acknowledges_all_waiters(pool: sqlx::PgPool) {
        let (writer, handle, _manager) = build_writer(pool).await;
        let ids = [Uuid::new_v4(), Uuid::new_v4()];
        let acknowledgements = ids.map(|id| {
            let handle = handle.clone();
            tokio::spawn(async move { handle.admit_realtime(realtime_input(id, "owner")).await })
        });
        tokio::task::yield_now().await;

        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let writer_task = tokio::spawn(writer.run(shutdown));
        for (acknowledgement, id) in acknowledgements.into_iter().zip(ids) {
            assert_eq!(acknowledgement.await.unwrap().unwrap(), RequestId(id));
        }
        writer_task.await.unwrap();
    }

    #[sqlx::test]
    async fn shutdown_during_linger_closes_admission_before_blocked_flush(pool: sqlx::PgPool) {
        let (mut writer, handle, _manager, fusillade_pool) = build_writer_with_pool(pool).await;
        writer.max_linger = Duration::from_secs(30);

        // Exhaust the real storage pool so the first accepted create cannot
        // finish its flush until this test explicitly releases the guards.
        let mut pool_guards = Vec::new();
        for _ in 0..4 {
            pool_guards.push(fusillade_pool.acquire().await.unwrap());
        }

        let admitted_id = Uuid::new_v4();
        let admitted_ack = tokio::spawn({
            let handle = handle.clone();
            async move { handle.admit_realtime(realtime_input(admitted_id, "owner")).await }
        });
        timeout(Duration::from_secs(1), async {
            while handle.sender.capacity() != CHANNEL_BUFFER_SIZE - 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first create should enter the channel before writer start");

        let shutdown = CancellationToken::new();
        let writer_task = tokio::spawn(writer.run(shutdown.clone()));
        timeout(Duration::from_secs(1), async {
            while handle.sender.capacity() != CHANNEL_BUFFER_SIZE {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("writer should receive the first create and enter linger");

        shutdown.cancel();
        timeout(Duration::from_millis(250), async {
            while !handle.sender.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shutdown observed during linger must close admission before flushing");

        let rejected = handle.admit_realtime(realtime_input(Uuid::new_v4(), "owner")).await;
        assert_eq!(rejected.unwrap_err().to_string(), "responses writer unavailable");
        assert!(
            !admitted_ack.is_finished(),
            "already-admitted create should still await the blocked flush"
        );

        drop(pool_guards);
        assert_eq!(
            timeout(Duration::from_secs(5), admitted_ack).await.unwrap().unwrap().unwrap(),
            RequestId(admitted_id)
        );
        timeout(Duration::from_secs(5), writer_task).await.unwrap().unwrap();
    }

    #[sqlx::test]
    async fn shutdown_during_zero_linger_blocked_flush_rejects_late_create(pool: sqlx::PgPool) {
        let (writer, handle, _manager, fusillade_pool) = build_writer_with_pool(pool).await;

        let mut pool_guards = Vec::new();
        for _ in 0..4 {
            pool_guards.push(fusillade_pool.acquire().await.unwrap());
        }

        let admitted_id = Uuid::new_v4();
        let admitted_ack = tokio::spawn({
            let handle = handle.clone();
            async move { handle.admit_realtime(realtime_input(admitted_id, "owner")).await }
        });
        timeout(Duration::from_secs(1), async {
            while handle.sender.capacity() != CHANNEL_BUFFER_SIZE - 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first create should enter the channel before writer start");

        let shutdown = CancellationToken::new();
        let writer_task = tokio::spawn(writer.run(shutdown.clone()));
        timeout(Duration::from_secs(1), async {
            while handle.sender.capacity() != CHANNEL_BUFFER_SIZE {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("writer should receive the first create before blocking in flush");

        shutdown.cancel();
        let rejected = timeout(
            Duration::from_millis(250),
            handle.admit_realtime(realtime_input(Uuid::new_v4(), "owner")),
        )
        .await
        .expect("post-cancellation create must reject while the prior flush is blocked");
        assert_eq!(rejected.unwrap_err().to_string(), "responses writer unavailable");
        assert!(
            !admitted_ack.is_finished(),
            "already-admitted create must remain in its blocked flush"
        );

        drop(pool_guards);
        assert_eq!(
            timeout(Duration::from_secs(5), admitted_ack).await.unwrap().unwrap().unwrap(),
            RequestId(admitted_id)
        );
        timeout(Duration::from_secs(5), writer_task).await.unwrap().unwrap();
    }

    #[sqlx::test]
    async fn closed_writer_returns_deterministic_unavailable_error(pool: sqlx::PgPool) {
        let (writer, handle, _manager) = build_writer(pool).await;
        drop(writer);

        let error = timeout(
            Duration::from_secs(1),
            handle.admit_realtime(realtime_input(Uuid::new_v4(), "owner")),
        )
        .await
        .expect("closed writer must not hang")
        .expect_err("closed writer must reject creates");
        assert_eq!(error.to_string(), "responses writer unavailable");
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
            .complete_realtime(RawCompletedRequest {
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
                .complete_realtime(RawCompletedRequest {
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
            .complete_realtime(RawCompletedRequest {
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
