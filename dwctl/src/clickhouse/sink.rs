//! A generic batched, best-effort sink: rows in, one gzip `JSONEachRow` insert out per
//! flush. See the module docs for the drop semantics.

use std::sync::Arc;
use std::time::{Duration, Instant};

use metrics::{counter, gauge, histogram};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::{ClickHouseClient, ClickHouseError, is_identifier};
use crate::metrics::errors::component::CLICKHOUSE_SINK;

/// Per-writer settings. The connection is shared; these are not.
#[derive(Debug, Clone)]
pub struct SinkOptions {
    /// Table within the client's database. Plain identifier.
    pub table: String,
    /// Flush at least this often while rows are queued.
    pub flush_interval: Duration,
    /// Flush as soon as this many rows are buffered.
    pub max_batch_rows: usize,
    /// Bounded queue between producers and the flusher; a full queue drops.
    pub queue_capacity: usize,
    /// Wait before the single retry of a failed insert.
    pub retry_delay: Duration,
}

impl SinkOptions {
    pub fn validate(&self) -> Result<(), String> {
        if !is_identifier(&self.table) {
            return Err(format!("table {:?} is not a plain identifier", self.table));
        }
        if self.max_batch_rows == 0 || self.queue_capacity == 0 {
            return Err("max_batch_rows and queue_capacity must be > 0".to_string());
        }
        if self.flush_interval.is_zero() {
            return Err("flush_interval must be > 0".to_string());
        }
        Ok(())
    }
}

/// Producer side. Cheap to clone; `send` never blocks and never fails loudly.
#[derive(Debug)]
pub struct SinkHandle<T> {
    tx: mpsc::Sender<T>,
    table: Arc<str>,
}

// Not derived: a derive would demand `T: Clone`, and the handle is cloneable whatever the
// row type is.
impl<T> Clone for SinkHandle<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            table: self.table.clone(),
        }
    }
}

impl<T> SinkHandle<T> {
    /// Queue a row. Returns `false` (and counts a drop) when the queue is full or the
    /// sink has shut down — the caller has nothing useful to do with that beyond
    /// knowing it, so it is not an error.
    pub fn send(&self, row: T) -> bool {
        match self.tx.try_send(row) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                counter!("dwctl_clickhouse_dropped_rows_total", "table" => self.table.to_string(), "reason" => "queue_full").increment(1);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                counter!("dwctl_clickhouse_dropped_rows_total", "table" => self.table.to_string(), "reason" => "closed").increment(1);
                false
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(tx: mpsc::Sender<T>, table: &str) -> Self {
        Self {
            tx,
            table: Arc::from(table),
        }
    }
}

/// The flusher. Owns the receiving end of the queue and the client.
pub struct ClickHouseSink<T> {
    client: ClickHouseClient,
    options: SinkOptions,
    rx: mpsc::Receiver<T>,
    table: Arc<str>,
}

impl<T: Serialize + Send + 'static> ClickHouseSink<T> {
    /// Build the queue and the flusher without starting it. `spawn` is the usual entry
    /// point; this exists so the run loop can be driven directly in tests.
    pub fn new(client: ClickHouseClient, options: SinkOptions) -> Result<(SinkHandle<T>, Self), ClickHouseError> {
        options.validate().map_err(ClickHouseError::Config)?;
        let (tx, rx) = mpsc::channel(options.queue_capacity);
        let table: Arc<str> = Arc::from(options.table.as_str());
        let handle = SinkHandle { tx, table: table.clone() };
        Ok((
            handle,
            Self {
                client,
                options,
                rx,
                table,
            },
        ))
    }

    /// Start the flusher on the runtime. The returned join handle completes after the
    /// final drain on shutdown.
    pub fn spawn(
        client: ClickHouseClient,
        options: SinkOptions,
        shutdown: CancellationToken,
    ) -> Result<(SinkHandle<T>, JoinHandle<()>), ClickHouseError> {
        let (handle, sink) = Self::new(client, options)?;
        let task = tokio::spawn(sink.run(shutdown));
        Ok((handle, task))
    }

    pub async fn run(mut self, shutdown: CancellationToken) {
        info!(
            table = %self.table,
            flush_interval_ms = self.options.flush_interval.as_millis() as u64,
            max_batch_rows = self.options.max_batch_rows,
            queue_capacity = self.options.queue_capacity,
            "ClickHouse sink started"
        );
        let mut buffer: Vec<T> = Vec::with_capacity(self.options.max_batch_rows);
        let mut ticker = tokio::time::interval(self.options.flush_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker.tick().await; // the first tick completes immediately; skip it

        loop {
            tokio::select! {
                biased;

                _ = shutdown.cancelled() => {
                    info!(table = %self.table, "Shutdown: draining ClickHouse sink");
                    self.rx.close();
                    while let Some(row) = self.rx.recv().await {
                        buffer.push(row);
                        if buffer.len() >= self.options.max_batch_rows {
                            self.flush(&mut buffer).await;
                        }
                    }
                    self.flush(&mut buffer).await;
                    info!(table = %self.table, "ClickHouse sink stopped");
                    return;
                }

                _ = ticker.tick() => {
                    self.flush(&mut buffer).await;
                }

                maybe_row = self.rx.recv() => {
                    match maybe_row {
                        Some(row) => {
                            buffer.push(row);
                            if buffer.len() >= self.options.max_batch_rows {
                                self.flush(&mut buffer).await;
                            }
                        }
                        None => {
                            self.flush(&mut buffer).await;
                            info!(table = %self.table, "ClickHouse sink queue closed");
                            return;
                        }
                    }
                }
            }
            gauge!("dwctl_clickhouse_queue_depth", "table" => self.table.to_string()).set(self.rx.len() as f64);
        }
    }

    /// Serialize the buffer to NDJSON and insert it, retrying once. Rows that fail to
    /// serialize are dropped individually; a batch that fails twice is dropped whole.
    /// The buffer is always empty afterwards.
    async fn flush(&self, buffer: &mut Vec<T>) {
        if buffer.is_empty() {
            return;
        }
        let table = self.table.to_string();
        let start = Instant::now();
        let mut body = Vec::with_capacity(buffer.len() * 256);
        let mut scratch = Vec::with_capacity(256);
        let mut rows = 0u64;
        for row in buffer.drain(..) {
            // Serialize into a scratch buffer first: a row that fails half-way must not
            // leave partial bytes in the batch, which would corrupt every row after it.
            scratch.clear();
            match serde_json::to_writer(&mut scratch, &row) {
                Ok(()) => {
                    body.extend_from_slice(&scratch);
                    body.push(b'\n');
                    rows += 1;
                }
                Err(e) => {
                    crate::background_error!(CLICKHOUSE_SINK, "serialize", Warning, table = %self.table, error = %e, "ClickHouse row failed to serialize; dropped");
                    counter!("dwctl_clickhouse_dropped_rows_total", "table" => table.clone(), "reason" => "serialize").increment(1);
                }
            }
        }
        if rows == 0 {
            return;
        }

        let mut attempt = 0u32;
        let outcome = loop {
            attempt += 1;
            match self.client.insert_json_each_row(&self.options.table, &body).await {
                Ok(()) => break "ok",
                Err(e) if attempt == 1 => {
                    warn!(table = %self.table, rows, error = %e, "ClickHouse insert failed; retrying once");
                    tokio::time::sleep(self.options.retry_delay).await;
                }
                Err(e) => {
                    crate::background_error!(CLICKHOUSE_SINK, "insert_failed", Error, table = %self.table, rows, error = %e, "ClickHouse insert failed again; batch dropped");
                    counter!("dwctl_clickhouse_dropped_rows_total", "table" => table.clone(), "reason" => "insert_failed").increment(rows);
                    break "error";
                }
            }
        };
        histogram!("dwctl_clickhouse_flush_duration_seconds", "table" => table.clone()).record(start.elapsed().as_secs_f64());
        counter!("dwctl_clickhouse_flushes_total", "table" => table.clone(), "outcome" => outcome).increment(1);
        counter!("dwctl_clickhouse_rows_total", "table" => table.clone(), "outcome" => outcome).increment(rows);
        debug!(table = %self.table, rows, outcome, bytes = body.len(), attempts = attempt, "ClickHouse flush");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clickhouse::ClickhouseConfig;
    use std::io::Read as _;
    use wiremock::matchers::{header, method, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[derive(Serialize)]
    struct Row {
        id: u32,
        name: &'static str,
    }

    fn client_for(server: &MockServer) -> ClickHouseClient {
        ClickHouseClient::from_config(&ClickhouseConfig {
            endpoint_url: server.uri(),
            database: "clay".into(),
            user: "dwctl".into(),
            password: "pw".into(),
        })
        .unwrap()
    }

    fn options(max_batch_rows: usize) -> SinkOptions {
        SinkOptions {
            table: "t".into(),
            flush_interval: Duration::from_secs(3600), // never, in these tests
            max_batch_rows,
            queue_capacity: 16,
            retry_delay: Duration::from_millis(10),
        }
    }

    fn gunzip(bytes: &[u8]) -> String {
        let mut out = String::new();
        flate2::read::GzDecoder::new(bytes).read_to_string(&mut out).unwrap();
        out
    }

    /// Three rows, batch cap three: exactly one insert, gzip body, one JSON object per
    /// line, statement and datetime setting in the query string, credentials in headers.
    #[tokio::test]
    async fn rows_are_batched_into_one_gzipped_insert() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(query_param("query", "INSERT INTO clay.t FORMAT JSONEachRow"))
            .and(query_param("date_time_input_format", "best_effort"))
            .and(header("x-clickhouse-user", "dwctl"))
            .and(header("x-clickhouse-key", "pw"))
            .and(header("content-encoding", "gzip"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let shutdown = CancellationToken::new();
        let (handle, task) = ClickHouseSink::<Row>::spawn(client_for(&server), options(3), shutdown.clone()).unwrap();
        for id in 0..3 {
            assert!(handle.send(Row { id, name: "x" }));
        }
        // Let the flusher run, then stop it cleanly.
        tokio::time::sleep(Duration::from_millis(200)).await;
        shutdown.cancel();
        task.await.unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body = gunzip(&requests[0].body);
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], r#"{"id":0,"name":"x"}"#);
    }

    /// Rows below the batch cap are flushed by the shutdown drain, not lost.
    #[tokio::test]
    async fn shutdown_flushes_a_partial_batch() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        let shutdown = CancellationToken::new();
        let (handle, task) = ClickHouseSink::<Row>::spawn(client_for(&server), options(100), shutdown.clone()).unwrap();
        assert!(handle.send(Row { id: 1, name: "a" }));
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown.cancel();
        task.await.unwrap();
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    /// A failing insert is tried exactly twice and then the batch is dropped; the sink
    /// keeps running for the next batch.
    #[tokio::test]
    async fn insert_failure_is_retried_once_then_dropped() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("Code: 62. DB::Exception: Syntax error"))
            .expect(2)
            .mount(&server)
            .await;
        let shutdown = CancellationToken::new();
        let (handle, task) = ClickHouseSink::<Row>::spawn(client_for(&server), options(1), shutdown.clone()).unwrap();
        assert!(handle.send(Row { id: 1, name: "a" }));
        tokio::time::sleep(Duration::from_millis(300)).await;
        shutdown.cancel();
        task.await.unwrap();
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    /// The producer never blocks: with the queue full the newest row is dropped and
    /// `send` says so.
    #[tokio::test]
    async fn queue_full_drops_the_newest_row() {
        let (tx, _rx) = mpsc::channel::<Row>(1);
        let handle = SinkHandle::for_test(tx, "t");
        assert!(handle.send(Row { id: 1, name: "a" }));
        assert!(!handle.send(Row { id: 2, name: "b" }), "second send finds the queue full");
    }

    #[test]
    fn options_are_validated() {
        assert!(options(1).validate().is_ok());
        let mut o = options(1);
        o.table = "bad table".into();
        assert!(o.validate().is_err());
        let mut o = options(0);
        assert!(o.validate().is_err());
        o.max_batch_rows = 1;
        o.flush_interval = Duration::ZERO;
        assert!(o.validate().is_err());
    }
}
