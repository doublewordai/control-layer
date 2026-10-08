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
//! persistence guarantee we actually need is "the response eventually
//! appears for observability". The pre-existing analytics batcher solves
//! the same shape for `http_analytics`; this is the parallel for `requests`.
//!
//! # Failure modes (explicit)
//!
//! Records can be lost in three situations, and there is no dead-letter
//! store — this is a deliberate trade-off:
//!
//!   * **Writer behind**: when the channel is full the outlet handler drops
//!     the record instead of waiting, counted as
//!     `dwctl_requests_writer_dropped_total{reason="channel_full"}`. Waiting
//!     would keep the whole captured request and response alive in the outlet
//!     task for as long as the writer stays behind, so a slow database would
//!     grow memory without bound.
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
//!     | try_send(RawCompletedRequest)   (in-memory mpsc; dropped and counted when full)
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
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
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
/// full, the outlet handler drops new records (see the module docs) rather
/// than holding their captured bodies while it waits. Matches the analytics
/// batcher capacity.
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

/// Byte definition used by the writer body gauges: the heap bytes actually
/// allocated for the record's `request_body` and `response_body`
/// (`String::capacity()`), summed. Those are the only unbounded strings on a
/// record; `model`, `endpoint`, `api_key` and `created_by` are header-sized and
/// deliberately excluded so the gauges measure the burst-amplifying payload,
/// not bookkeeping. Constants of the definition live here so the queued, batch
/// and clone-accounting paths cannot drift apart.
fn body_bytes(record: &RawCompletedRequest) -> i64 {
    (record.request_body.capacity() + record.response_body.capacity()) as i64
}

/// Per-writer accounting state, behind an `Arc` shared by the sender handle
/// and the writer task.
///
/// Counters are per-writer (rather than process globals) so parallel tests
/// cannot interfere; the gauges they publish are process globals because a
/// production process runs exactly one writer. Gauges are `set` from the
/// atomics on every change, never incremented/decremented, so a recorder
/// installed after the writer started still reads the correct absolute value.
#[derive(Debug, Default)]
struct WriterAccounting {
    /// Records successfully handed to (or blocked in `send` toward) the
    /// channel and not yet taken by the writer task.
    queued_records: AtomicI64,
    /// [`body_bytes`] of `queued_records`.
    queued_body_bytes: AtomicI64,
    /// Records taken out of the channel and held in the writer's buffer until
    /// the flush handling them finishes.
    batch_records: AtomicI64,
    /// [`body_bytes`] of `batch_records`, plus the bytes of the body copies
    /// made for persistence while those copies are alive. During a flush this
    /// deliberately counts originals + clones.
    batch_body_bytes: AtomicI64,
    /// Serialises load-and-set of the gauges. Without it two threads can load
    /// in one order and set in the other, leaving a stale value published
    /// after the counters reach zero. Holding it across the load makes the last
    /// publisher see every counter change that preceded it.
    publish: Mutex<()>,
}

impl WriterAccounting {
    fn publish_queued(&self) {
        let _publish = self.publish.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        gauge!("dwctl_requests_writer_queued_records").set(self.queued_records.load(Ordering::Relaxed) as f64);
        gauge!("dwctl_requests_writer_queued_body_bytes").set(self.queued_body_bytes.load(Ordering::Relaxed) as f64);
    }

    fn publish_batch(&self) {
        let _publish = self.publish.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        gauge!("dwctl_requests_writer_batch_records").set(self.batch_records.load(Ordering::Relaxed) as f64);
        gauge!("dwctl_requests_writer_batch_body_bytes").set(self.batch_body_bytes.load(Ordering::Relaxed) as f64);
    }
}

/// Guide the operator to the exact definitions above once, at writer startup.
/// Idempotent when several writers are built (tests).
fn describe_writer_gauges() {
    metrics::describe_gauge!(
        "dwctl_requests_writer_queued_records",
        "Completed-response records enqueued into the writer channel and not yet taken by the writer task"
    );
    metrics::describe_gauge!(
        "dwctl_requests_writer_queued_body_bytes",
        "String::capacity() of request_body + response_body for records queued in the writer channel"
    );
    metrics::describe_gauge!(
        "dwctl_requests_writer_batch_records",
        "Records taken from the writer channel and held until the flush handling them finishes"
    );
    metrics::describe_gauge!(
        "dwctl_requests_writer_batch_body_bytes",
        "Body bytes of buffered records plus persistence body-clone bytes while the clones are alive"
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Queued,
    Batch,
}

/// A [`RawCompletedRequest`] plus the lifetime of its accounting.
///
/// Created `Queued` *before* the send (so a full/closed channel, where the
/// value comes back and is dropped, unwinds), promoted to `Batch` when the
/// writer takes it out of the channel, and dropped for good when the flush
/// that handles it finishes. `Drop` always subtracts exactly what the guard
/// added, which covers send failure, receiver/channel drop with records
/// inside, task cancellation or future drop at any await point, flush failure,
/// successful persistence, the terminal/unavailable split path and shutdown
/// drain.
struct AccountedRecord {
    // `Option` only so a failed send can hand the raw payload back to the
    // caller; it is always `Some` while the guard is alive.
    record: Option<RawCompletedRequest>,
    accounting: Arc<WriterAccounting>,
    body_bytes: i64,
    stage: Stage,
}

impl AccountedRecord {
    fn new_queued(record: RawCompletedRequest, accounting: Arc<WriterAccounting>) -> Self {
        let body_bytes = body_bytes(&record);
        accounting.queued_records.fetch_add(1, Ordering::Relaxed);
        accounting.queued_body_bytes.fetch_add(body_bytes, Ordering::Relaxed);
        accounting.publish_queued();
        Self {
            record: Some(record),
            accounting,
            body_bytes,
            stage: Stage::Queued,
        }
    }

    /// Account a record the writer already owns (used by tests that drive
    /// `flush_batch` directly).
    #[cfg(test)]
    fn new_batch(record: RawCompletedRequest, accounting: Arc<WriterAccounting>) -> Self {
        let body_bytes = body_bytes(&record);
        accounting.batch_records.fetch_add(1, Ordering::Relaxed);
        accounting.batch_body_bytes.fetch_add(body_bytes, Ordering::Relaxed);
        accounting.publish_batch();
        Self {
            record: Some(record),
            accounting,
            body_bytes,
            stage: Stage::Batch,
        }
    }

    fn record(&self) -> &RawCompletedRequest {
        self.record.as_ref().expect("accounted record always holds its payload")
    }

    /// The writer took this record out of the channel: move it from the queued
    /// to the batch stage. Ordering between the two counter updates does not
    /// matter; both gauges are published from their final values.
    fn into_batch(mut self) -> Self {
        debug_assert_eq!(self.stage, Stage::Queued);
        self.accounting.queued_records.fetch_sub(1, Ordering::Relaxed);
        self.accounting.queued_body_bytes.fetch_sub(self.body_bytes, Ordering::Relaxed);
        self.accounting.batch_records.fetch_add(1, Ordering::Relaxed);
        self.accounting.batch_body_bytes.fetch_add(self.body_bytes, Ordering::Relaxed);
        self.stage = Stage::Batch;
        self.accounting.publish_queued();
        self.accounting.publish_batch();
        self
    }

    /// Reclaim the payload for a failed `send`; dropping the guard unwinds the
    /// queued counters.
    fn into_record(mut self) -> RawCompletedRequest {
        self.record.take().expect("accounted record always holds its payload")
    }
}

impl Drop for AccountedRecord {
    fn drop(&mut self) {
        match self.stage {
            Stage::Queued => {
                self.accounting.queued_records.fetch_sub(1, Ordering::Relaxed);
                self.accounting.queued_body_bytes.fetch_sub(self.body_bytes, Ordering::Relaxed);
                self.accounting.publish_queued();
            }
            Stage::Batch => {
                self.accounting.batch_records.fetch_sub(1, Ordering::Relaxed);
                self.accounting.batch_body_bytes.fetch_sub(self.body_bytes, Ordering::Relaxed);
                self.accounting.publish_batch();
            }
        }
    }
}

/// Accounts body-clone bytes for the duration of a flush.
///
/// Declared *before* the persistence inputs in `flush_batch`, so it is dropped
/// after them; the copy bytes are therefore counted for exactly as long as the
/// clones exist. Only `request_body`/`response_body` clones are counted — the
/// other `PersistCompletedRealtimeInput` strings are bounded header values.
struct CloneBodyGuard<'a> {
    accounting: &'a WriterAccounting,
    bytes: i64,
}

impl<'a> CloneBodyGuard<'a> {
    fn new(accounting: &'a WriterAccounting) -> Self {
        Self { accounting, bytes: 0 }
    }

    fn add(&mut self, bytes: i64) {
        self.bytes += bytes;
        self.accounting.batch_body_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.accounting.publish_batch();
    }
}

impl Drop for CloneBodyGuard<'_> {
    fn drop(&mut self) {
        if self.bytes != 0 {
            self.accounting.batch_body_bytes.fetch_sub(self.bytes, Ordering::Relaxed);
            self.accounting.publish_batch();
        }
    }
}

/// Sender handle handed to the outlet handler.
///
/// A thin wrapper over the channel sender that keeps the queued-record
/// accounting exact. `send` mirrors `mpsc::Sender::send`, including returning
/// the record in the error on a full/closed channel, so call sites are
/// unchanged.
#[derive(Clone)]
pub struct RequestsWriterSender {
    sender: mpsc::Sender<AccountedRecord>,
    accounting: Arc<WriterAccounting>,
}

impl RequestsWriterSender {
    /// Send one completed-response record to the writer.
    // Mirrors `mpsc::Sender::send`, which hands the record back on failure;
    // boxing it would change the call sites for no benefit on a cold path.
    #[allow(clippy::result_large_err)]
    pub async fn send(&self, record: RawCompletedRequest) -> Result<(), mpsc::error::SendError<RawCompletedRequest>> {
        let accounted = AccountedRecord::new_queued(record, self.accounting.clone());
        self.sender
            .send(accounted)
            .await
            .map_err(|error| mpsc::error::SendError(error.0.into_record()))
    }

    /// Queue one completed-response record without waiting.
    ///
    /// Fails immediately when the channel is full or closed, handing the record
    /// back. The outlet handler uses this so a writer that has fallen behind
    /// never holds the caller's captured request and response.
    #[allow(clippy::result_large_err)]
    pub fn try_send(&self, record: RawCompletedRequest) -> Result<(), mpsc::error::TrySendError<RawCompletedRequest>> {
        let accounted = AccountedRecord::new_queued(record, self.accounting.clone());
        self.sender.try_send(accounted).map_err(|error| match error {
            mpsc::error::TrySendError::Full(record) => mpsc::error::TrySendError::Full(record.into_record()),
            mpsc::error::TrySendError::Closed(record) => mpsc::error::TrySendError::Closed(record.into_record()),
        })
    }

    /// A sender whose single-slot channel already holds one record, standing in
    /// for a writer that has fallen behind. The returned guard keeps the
    /// receiver (and so the channel) alive; `queued_records()` reads the queue.
    #[cfg(test)]
    pub(crate) fn full_for_test(record: RawCompletedRequest) -> (Self, Box<dyn std::any::Any + Send>) {
        let accounting = Arc::new(WriterAccounting::default());
        let (sender, receiver) = mpsc::channel::<AccountedRecord>(1);
        let sender = Self { sender, accounting };
        sender.try_send(record).expect("an empty single-slot channel accepts one record");
        (sender, Box::new(receiver))
    }

    #[cfg(test)]
    pub(crate) fn queued_records(&self) -> i64 {
        self.accounting.queued_records.load(Ordering::Relaxed)
    }
}

/// Background consumer that batches `RawCompletedRequest`s and flushes them
/// to fusillade in a single transaction per batch.
///
/// Generic over `PoolProvider` so the same struct works in production
/// (`DbPools`) and tests (`TestDbPools`).
pub struct RequestsWriter<P: PoolProvider + Clone + Send + Sync + 'static> {
    request_manager: Arc<PostgresRequestManager<P>>,
    receiver: mpsc::Receiver<AccountedRecord>,
    batch_size: usize,
    max_retries: u32,
    retry_base_delay: Duration,
    accounting: Arc<WriterAccounting>,
}

impl<P: PoolProvider + Clone + Send + Sync + 'static> RequestsWriter<P> {
    /// Build the writer and return it alongside the sender handle. Spawn the
    /// returned future via `tokio::spawn(writer.run(token))`; pass the sender
    /// into `FusilladeOutletHandler::new`.
    pub fn new(request_manager: Arc<PostgresRequestManager<P>>, batch_size: usize) -> (Self, RequestsWriterSender) {
        let (sender, receiver) = mpsc::channel(CHANNEL_BUFFER_SIZE);
        let accounting = Arc::new(WriterAccounting::default());
        describe_writer_gauges();
        // Publish zeros so an idle writer reads 0 rather than absent.
        accounting.publish_queued();
        accounting.publish_batch();
        let writer = Self {
            request_manager,
            receiver,
            batch_size: batch_size.max(1),
            max_retries: DEFAULT_MAX_RETRIES,
            retry_base_delay: Duration::from_millis(DEFAULT_RETRY_BASE_DELAY_MS),
            accounting: accounting.clone(),
        };
        let sender = RequestsWriterSender { sender, accounting };
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

        let mut buffer: Vec<AccountedRecord> = Vec::with_capacity(self.batch_size);

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
                        buffer.push(record.into_batch());
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
                        Some(record) => buffer.push(record.into_batch()),
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
                    Ok(record) => buffer.push(record.into_batch()),
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
    async fn flush_batch(&self, buffer: &mut Vec<AccountedRecord>) {
        if buffer.is_empty() {
            return;
        }

        let batch_size = buffer.len();
        let span = info_span!("dwctl.flush_responses_batch", batch_size);

        async {
            let start = Instant::now();
            histogram!("dwctl_requests_writer_flush_size").record(batch_size as f64);

            // Declared before `inputs` so it outlives them and counts the
            // persistence body clones for exactly as long as they exist.
            let mut clone_body_guard = CloneBodyGuard::new(&self.accounting);
            let inputs: Vec<PersistCompletedRealtimeInput> = buffer
                .iter()
                .map(|record| {
                    let record = record.record();
                    PersistCompletedRealtimeInput {
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
                    }
                })
                .collect();
            clone_body_guard.add(
                inputs
                    .iter()
                    .map(|input| (input.request_body.capacity() + input.response_body.capacity()) as i64)
                    .sum(),
            );

            let mut persisted = 0_usize;
            let mut unavailable = 0_usize;
            let mut dropped = 0_usize;
            match self.persist_with_retries(&inputs).await {
                Ok(()) => persisted = inputs.len(),
                Err(error) if retained_write_error(&error) == Some(fusillade::RetainedResponseWriteError::NotFound) && inputs.len() > 1 => {
                    // The storage transaction rolled the mixed batch back.
                    // Retry records individually so valid siblings persist;
                    // unavailable identities are terminal and never retried.
                    for input in &inputs {
                        match self.persist_with_retries(std::slice::from_ref(input)).await {
                            Ok(()) => persisted += 1,
                            Err(error) if retained_write_error(&error) == Some(fusillade::RetainedResponseWriteError::NotFound) => {
                                unavailable += 1;
                            }
                            Err(error) => {
                                dropped += 1;
                                crate::background_error!(
                                    RESPONSES_WRITER,
                                    "flush_drop",
                                    Error,
                                    error_class = response_write_error_class(&error),
                                    batch_size = 1,
                                    attempts = self.max_retries + 1,
                                    "Failed to flush one response record after retries"
                                );
                            }
                        }
                    }
                }
                Err(error) if retained_write_error(&error) == Some(fusillade::RetainedResponseWriteError::NotFound) => {
                    unavailable = inputs.len();
                }
                Err(error) => {
                    dropped = inputs.len();
                    crate::background_error!(
                        RESPONSES_WRITER,
                        "flush_drop",
                        Error,
                        error_class = response_write_error_class(&error),
                        batch_size,
                        attempts = self.max_retries + 1,
                        "Failed to flush responses batch after retries"
                    );
                }
            }

            let duration = start.elapsed();
            histogram!("dwctl_requests_writer_flush_duration_seconds").record(duration.as_secs_f64());
            counter!("dwctl_requests_writer_records_total").increment(persisted as u64);
            counter!("dwctl_requests_writer_terminal_total", "outcome" => "unavailable").increment(unavailable as u64);

            debug!(
                batch_size,
                persisted,
                unavailable,
                dropped,
                duration_ms = duration.as_millis() as u64,
                "Finished responses batch flush"
            );

            // Drop the clones (and their accounting) before the originals so
            // the gauges never briefly over-count dead copies.
            drop(inputs);
            drop(clone_body_guard);
            buffer.clear();
        }
        .instrument(span)
        .await;
    }

    async fn persist_with_retries(&self, inputs: &[PersistCompletedRealtimeInput]) -> Result<(), fusillade::FusilladeError> {
        let batch_size = inputs.len();
        for attempt in 0..=self.max_retries {
            match self.request_manager.persist_completed_realtime_batch(inputs).await {
                Ok(()) => {
                    if attempt > 0 {
                        debug!(attempt, batch_size, "Responses batch flush succeeded after retry");
                        counter!("dwctl_requests_writer_retries_total", "outcome" => "success").increment(1);
                    }
                    return Ok(());
                }
                Err(error) if retained_write_error(&error) == Some(fusillade::RetainedResponseWriteError::NotFound) => {
                    return Err(error);
                }
                Err(error) if attempt < self.max_retries => {
                    let delay = self.retry_base_delay * 2u32.pow(attempt);
                    warn!(
                        error_class = response_write_error_class(&error),
                        attempt = attempt + 1,
                        max_retries = self.max_retries,
                        delay_ms = delay.as_millis() as u64,
                        batch_size,
                        "Responses batch flush failed, retrying"
                    );
                    counter!("dwctl_requests_writer_retries_total", "outcome" => "retry").increment(1);
                    tokio::time::sleep(delay).await;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("the inclusive retry loop always returns")
    }
}

fn retained_write_error(error: &fusillade::FusilladeError) -> Option<fusillade::RetainedResponseWriteError> {
    fusillade::RetainedResponseWriteError::from_fusillade_error(error)
}

fn response_write_error_class(error: &fusillade::FusilladeError) -> &'static str {
    if retained_write_error(error).is_some() {
        "retained_lifecycle"
    } else {
        match error {
            fusillade::FusilladeError::RequestNotFound(_) => "request_not_found",
            fusillade::FusilladeError::RequestStateConflict { .. } => "state_conflict",
            fusillade::FusilladeError::ValidationError(_) => "validation",
            _ => "storage",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fusillade::RequestId;
    use fusillade::ReqwestHttpClient;
    use fusillade_arsenal::PostgresRequestManager;
    use sqlx_pool_router::TestDbPools;
    use std::io::Write;
    use std::time::Duration;
    use tokio::time::timeout;
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    #[derive(Clone)]
    struct CaptureWriter(Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for CaptureWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
        type Writer = CaptureWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

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

    /// A minimal valid record for accounting-only (non-DB) tests.
    fn test_record(request_id: Uuid) -> RawCompletedRequest {
        let now = Utc::now();
        RawCompletedRequest {
            request_id,
            status_code: 200,
            response_body: r#"{"output":"done"}"#.to_owned(),
            request_body: r#"{"input":"hi"}"#.to_owned(),
            model: "test-model".to_owned(),
            endpoint: "/v1/responses".to_owned(),
            api_key: "sk-test".to_owned(),
            created_by: "test-owner".to_owned(),
            started_at: now,
            completed_at: now,
        }
    }

    /// Every writer-accounting counter must be exactly zero once every guard
    /// that touched it has been dropped.
    fn assert_accounting_zero(accounting: &WriterAccounting) {
        assert_eq!(accounting.queued_records.load(Ordering::Relaxed), 0, "queued_records");
        assert_eq!(accounting.queued_body_bytes.load(Ordering::Relaxed), 0, "queued_body_bytes");
        assert_eq!(accounting.batch_records.load(Ordering::Relaxed), 0, "batch_records");
        assert_eq!(accounting.batch_body_bytes.load(Ordering::Relaxed), 0, "batch_body_bytes");
    }

    #[tokio::test]
    async fn test_accounted_record_moves_queued_to_batch_and_back_to_zero() {
        let accounting = Arc::new(WriterAccounting::default());
        let record = test_record(Uuid::new_v4());
        let expected_bytes = body_bytes(&record);

        let accounted = AccountedRecord::new_queued(record, accounting.clone());
        assert_eq!(accounting.queued_records.load(Ordering::Relaxed), 1);
        assert_eq!(accounting.queued_body_bytes.load(Ordering::Relaxed), expected_bytes);
        assert_eq!(accounting.batch_records.load(Ordering::Relaxed), 0);

        let accounted = accounted.into_batch();
        assert_eq!(accounting.queued_records.load(Ordering::Relaxed), 0);
        assert_eq!(accounting.queued_body_bytes.load(Ordering::Relaxed), 0);
        assert_eq!(accounting.batch_records.load(Ordering::Relaxed), 1);
        assert_eq!(accounting.batch_body_bytes.load(Ordering::Relaxed), expected_bytes);

        drop(accounted);
        assert_accounting_zero(&accounting);
    }

    #[tokio::test]
    async fn test_failed_send_returns_queued_accounting_to_zero() {
        // A closed receiver makes `send` fail and hand the record back, the
        // same unwind path a full channel takes once the caller's send is
        // cancelled or the receiver is dropped.
        let accounting = Arc::new(WriterAccounting::default());
        let (sender, receiver) = mpsc::channel::<AccountedRecord>(1);
        let sender = RequestsWriterSender {
            sender,
            accounting: accounting.clone(),
        };
        drop(receiver);

        let error = sender
            .send(test_record(Uuid::new_v4()))
            .await
            .expect_err("send to a closed channel must fail");
        // The raw payload is returned in the original error shape.
        let _ = error.0;

        assert_accounting_zero(&accounting);
    }

    #[tokio::test]
    async fn test_try_send_to_a_full_channel_fails_without_waiting() {
        let accounting = Arc::new(WriterAccounting::default());
        let (sender, mut receiver) = mpsc::channel::<AccountedRecord>(1);
        let sender = RequestsWriterSender {
            sender,
            accounting: accounting.clone(),
        };

        sender
            .try_send(test_record(Uuid::new_v4()))
            .expect("an empty channel accepts one record");
        let rejected = sender
            .try_send(test_record(Uuid::new_v4()))
            .expect_err("a full channel must reject the record immediately");
        assert!(matches!(rejected, mpsc::error::TrySendError::Full(_)));
        drop(rejected);
        // Only the record that made it into the channel is counted as queued.
        assert_eq!(accounting.queued_records.load(Ordering::Relaxed), 1);

        // Once the writer takes a record there is room again.
        let taken = receiver.recv().await.expect("the queued record is still there");
        drop(taken);
        sender
            .try_send(test_record(Uuid::new_v4()))
            .expect("a drained channel accepts a record");
        assert_eq!(accounting.queued_records.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_dropping_receiver_with_queued_records_returns_to_zero() {
        // The channel dropped with records still inside (writer gone, process
        // shutdown) must unwind the queued counters as the messages drop.
        let accounting = Arc::new(WriterAccounting::default());
        let (sender, receiver) = mpsc::channel::<AccountedRecord>(4);
        let sender = RequestsWriterSender {
            sender,
            accounting: accounting.clone(),
        };
        sender.send(test_record(Uuid::new_v4())).await.expect("send should succeed");
        assert_eq!(accounting.queued_records.load(Ordering::Relaxed), 1);

        drop(receiver);
        assert_accounting_zero(&accounting);
    }

    #[tokio::test]
    async fn test_clone_body_guard_accounts_and_releases_copy_bytes() {
        let accounting = WriterAccounting::default();
        {
            let mut guard = CloneBodyGuard::new(&accounting);
            guard.add(128);
            assert_eq!(accounting.batch_body_bytes.load(Ordering::Relaxed), 128);
        }
        assert_eq!(accounting.batch_body_bytes.load(Ordering::Relaxed), 0);
    }

    #[dwctl_test_macros::test]
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

        // Successful persistence must unwind both the queued and batch
        // counters to zero.
        assert_accounting_zero(&sender.accounting);
    }

    #[dwctl_test_macros::test]
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

    #[dwctl_test_macros::test]
    async fn test_writer_splits_unavailable_record_without_losing_valid_sibling(pool: sqlx::PgPool) {
        let fusillade_pool = crate::test::utils::setup_fusillade_pool(&pool).await;
        let unavailable_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO retained_response_resurrection_fences \
             (object_id, reason, expires_at) \
             VALUES ($1, 'erased', NOW() + INTERVAL '1 hour')",
        )
        .bind(unavailable_id)
        .execute(&fusillade_pool)
        .await
        .unwrap();
        let (writer, _sender, manager) = build_writer(pool).await;
        let valid_id = Uuid::new_v4();
        let now = Utc::now();
        let record = |request_id| RawCompletedRequest {
            request_id,
            status_code: 200,
            response_body: r#"{"output":"done"}"#.to_owned(),
            request_body: r#"{"input":"safe"}"#.to_owned(),
            model: "test-model".to_owned(),
            endpoint: "/v1/responses".to_owned(),
            api_key: String::new(),
            created_by: "test-owner".to_owned(),
            started_at: now,
            completed_at: now,
        };
        let mut records = vec![record(unavailable_id), record(valid_id)]
            .into_iter()
            .map(|record| AccountedRecord::new_batch(record, writer.accounting.clone()))
            .collect::<Vec<_>>();

        writer.flush_batch(&mut records).await;

        assert!(records.is_empty());
        assert_accounting_zero(&writer.accounting);
        let valid = manager
            .get_request_detail(RequestId(valid_id))
            .await
            .expect("the valid sibling must persist after batch splitting");
        assert_eq!(valid.status, "completed");
        assert!(matches!(
            manager.get_request_detail(RequestId(unavailable_id)).await,
            Err(fusillade::FusilladeError::RequestNotFound(_))
        ));
    }

    #[dwctl_test_macros::test]
    async fn test_writer_failure_logs_are_content_free(pool: sqlx::PgPool) {
        const REQUEST_SENTINEL: &str = "private-request-payload-b83b1a";
        const RESPONSE_SENTINEL: &str = "private-response-payload-66f319";
        const MODEL_SENTINEL: &str = "private-model-c13fa2";
        const API_KEY_SENTINEL: &str = "private-api-key-220b39";
        const OWNER_SENTINEL: &str = "private-owner-d34fb4";
        const DATABASE_SENTINEL: &str = "raw-database-error-566e2d";
        const POSITIVE_CONTROL: &str = "writer-log-capture-positive-control";

        let (mut writer, _sender, _manager) = build_writer(pool.clone()).await;
        writer.max_retries = 0;

        sqlx::query(
            r#"
            CREATE OR REPLACE FUNCTION fusillade.reject_writer_fixture()
            RETURNS trigger LANGUAGE plpgsql AS $function$
            BEGIN
                RAISE EXCEPTION 'raw-database-error-566e2d';
            END
            $function$
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            r#"
            CREATE TRIGGER reject_writer_fixture
            BEFORE INSERT ON fusillade.request_templates_g2
            FOR EACH ROW EXECUTE FUNCTION fusillade.reject_writer_fixture()
            "#,
        )
        .execute(&pool)
        .await
        .unwrap();

        let log_bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(CaptureWriter(log_bytes.clone()))
            .finish();
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);
        tracing::info!(POSITIVE_CONTROL);

        let request_id = Uuid::new_v4();
        let now = Utc::now();
        let mut records = vec![RawCompletedRequest {
            request_id,
            status_code: 200,
            response_body: RESPONSE_SENTINEL.to_owned(),
            request_body: REQUEST_SENTINEL.to_owned(),
            model: MODEL_SENTINEL.to_owned(),
            endpoint: "/v1/responses".to_owned(),
            api_key: API_KEY_SENTINEL.to_owned(),
            created_by: OWNER_SENTINEL.to_owned(),
            started_at: now,
            completed_at: now,
        }]
        .into_iter()
        .map(|record| AccountedRecord::new_batch(record, writer.accounting.clone()))
        .collect::<Vec<_>>();
        writer.flush_batch(&mut records).await;

        // A flush that fails after all retries still drops the batch and
        // unwinds its accounting.
        assert_accounting_zero(&writer.accounting);

        let logs = String::from_utf8(log_bytes.lock().unwrap().clone()).unwrap();
        assert!(logs.contains(POSITIVE_CONTROL), "the capture subscriber must be active");
        assert!(logs.contains("Failed to flush responses batch after retries"));
        for secret in [
            REQUEST_SENTINEL,
            RESPONSE_SENTINEL,
            MODEL_SENTINEL,
            API_KEY_SENTINEL,
            OWNER_SENTINEL,
            DATABASE_SENTINEL,
        ] {
            assert!(!logs.contains(secret), "sensitive value leaked into tracing output");
        }
        assert!(
            !logs.contains(&request_id.to_string()),
            "request identity leaked into tracing output"
        );
    }

    #[dwctl_test_macros::test]
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

        // The shutdown drain must unwind all accounting.
        assert_accounting_zero(&sender.accounting);

        // Sender outlives the writer task to guarantee we didn't exit via
        // the channel-closed arm. Drop it here explicitly to make the
        // sequence intent clear.
        drop(sender);
    }
}
