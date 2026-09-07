//! Cross-pod relay for live flex-streaming chunks, over Redis Streams.
//!
//! Relays the per-token SSE events the daemon already sees
//! (`crate::inference::outbound_request::Sink::absorb`) to the client-holding
//! pod. Best-effort — the persisted body stays authoritative.
//!
//! Streams over pub/sub: `request_id` exists before the daemon claims the
//! row, so a subscriber can listen before anything is published — pub/sub
//! would silently lose those early messages, a stream lets it replay them.
//!
//! Publish (`Sink::absorb`) is synchronous and can't `.await`, so it's a
//! `try_send` into a channel drained by a shard of background publisher
//! tasks. Subscribe fans out across a fixed number of reader shards, each
//! doing a single `XREAD` per tick over its own slice of subscribers
//! (`request_id % reader_workers`). Batched per shard instead of one
//! blocking connection per client, but not diluted across every subscriber
//! in the process the way a single shared reader would be under load.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use deadpool_redis::redis::streams::StreamReadReply;
use deadpool_redis::redis::{Value, cmd, pipe};
use deadpool_redis::{Config as RedisConfig, Pool, Runtime};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

/// One relayed chunk, in delivery order.
#[derive(Debug, Clone)]
pub struct ChunkMsg {
    pub seq: u64,
    pub data: String,
    pub done: bool,
}

pub type ChunkStream = ReceiverStream<ChunkMsg>;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ChunkRelayConfig {
    /// Independent of `keystore.redis_url` — chunk-relay traffic is much
    /// higher QPS, so a shared instance risks a streaming burst affecting
    /// ZDR keystore latency.
    pub redis_url: String,
    /// Refreshed periodically while a stream is active, so it expires
    /// roughly this long after its last message, not from creation.
    #[serde(default = "default_stream_ttl_secs")]
    pub stream_ttl_secs: u64,
    #[serde(default = "default_maxlen")]
    pub maxlen: usize,
    #[serde(default = "default_publish_channel_capacity")]
    pub publish_channel_capacity: usize,
    #[serde(default = "default_reader_poll_interval_ms")]
    pub reader_poll_interval_ms: u64,
    /// Number of independent publisher shards, each with its own channel,
    /// Redis connection, and background task. A single sequential publisher
    /// (one `XADD` round trip at a time) has a hard throughput ceiling no
    /// buffer size can fix. Once aggregate chunk volume exceeds it, the
    /// channel backs up and overflows regardless of capacity. Sharding by
    /// `request_id` lets that many `XADD`s be in flight concurrently.
    #[serde(default = "default_publish_workers")]
    pub publish_workers: usize,
    /// Number of independent reader shards, each doing its own `XREAD` over
    /// a fixed slice of subscribers (`request_id % reader_workers`), on its
    /// own Redis connection. A single reader sweeping every subscriber
    /// dilutes as subscriber count grows, so a request's chunks take longer
    /// to reach it than the poll-fallback path, which doesn't dilute with
    /// subscriber count and wins the terminal race instead. Sharding keeps
    /// each reader's slice a fixed size regardless of total load.
    #[serde(default = "default_reader_workers")]
    pub reader_workers: usize,
}

impl Default for ChunkRelayConfig {
    fn default() -> Self {
        Self {
            redis_url: String::new(),
            stream_ttl_secs: default_stream_ttl_secs(),
            maxlen: default_maxlen(),
            publish_channel_capacity: default_publish_channel_capacity(),
            reader_poll_interval_ms: default_reader_poll_interval_ms(),
            publish_workers: default_publish_workers(),
            reader_workers: default_reader_workers(),
        }
    }
}

fn default_stream_ttl_secs() -> u64 {
    600
}
fn default_maxlen() -> usize {
    2000
}
fn default_publish_channel_capacity() -> usize {
    1024
}
fn default_reader_poll_interval_ms() -> u64 {
    75
}
fn default_publish_workers() -> usize {
    8
}
fn default_reader_workers() -> usize {
    8
}

/// How often (in messages) to refresh a stream's TTL.
const EXPIRE_REFRESH_EVERY: u64 = 20;

#[derive(Debug, thiserror::Error)]
pub enum ChunkRelayError {
    #[error("chunk relay unreachable: {0}")]
    Unreachable(String),

    #[error("chunk relay configuration error: {0}")]
    Config(String),
}

impl ChunkRelayError {
    pub fn is_unreachable(&self) -> bool {
        matches!(self, Self::Unreachable(_))
    }
}

struct PublishMsg {
    request_id: Uuid,
    seq: u64,
    data: String,
    done: bool,
}

struct ReaderEntry {
    tx: mpsc::Sender<ChunkMsg>,
    last_id: String,
}

fn stream_key(request_id: Uuid) -> String {
    format!("flex:chunks:{request_id}")
}

fn parse_stream_key(key: &str) -> Option<Uuid> {
    key.strip_prefix("flex:chunks:").and_then(|s| Uuid::parse_str(s).ok())
}

/// Cheap to clone — publish/subscribe go through the shared background
/// tasks spawned once in [`ChunkRelay::from_config`].
#[derive(Clone)]
pub struct ChunkRelay {
    /// One independent channel + background publisher per shard, see
    /// `ChunkRelayConfig::publish_workers`. A request's chunks always hash
    /// to the same shard, so per-request ordering into Redis is preserved.
    publish_txs: Arc<[mpsc::Sender<PublishMsg>]>,
    registry: Arc<DashMap<Uuid, ReaderEntry>>,
}

impl ChunkRelay {
    /// Spawns the publisher shards and the reader task. Construct once at
    /// startup and clone the handle; do not call per request.
    pub fn from_config(cfg: &ChunkRelayConfig) -> Result<Self, ChunkRelayError> {
        let pool = RedisConfig::from_url(cfg.redis_url.clone())
            .create_pool(Some(Runtime::Tokio1))
            .map_err(|e| ChunkRelayError::Config(format!("failed to create redis pool: {e}")))?;

        let workers = cfg.publish_workers.max(1);
        let publish_txs: Vec<mpsc::Sender<PublishMsg>> = (0..workers)
            .map(|_| {
                let (tx, rx) = mpsc::channel(cfg.publish_channel_capacity);
                tokio::spawn(publisher_loop(pool.clone(), rx, cfg.stream_ttl_secs, cfg.maxlen));
                tx
            })
            .collect();
        let registry: Arc<DashMap<Uuid, ReaderEntry>> = Arc::new(DashMap::new());

        let reader_workers = cfg.reader_workers.max(1);
        for shard in 0..reader_workers {
            tokio::spawn(reader_loop(
                pool.clone(),
                registry.clone(),
                Duration::from_millis(cfg.reader_poll_interval_ms),
                shard,
                reader_workers,
            ));
        }

        Ok(Self {
            publish_txs: publish_txs.into(),
            registry,
        })
    }

    /// Every chunk for a given request goes through the same shard, so
    /// ordering into that request's stream is preserved.
    fn shard_for(&self, request_id: Uuid) -> &mpsc::Sender<PublishMsg> {
        let idx = (request_id.as_u128() % self.publish_txs.len() as u128) as usize;
        &self.publish_txs[idx]
    }

    /// Non-blocking — called from `Sink::absorb`, which can't `.await`. A
    /// full channel or unreachable Redis just drops the message.
    pub fn publish_nonblocking(&self, request_id: Uuid, seq: u64, data: &str) {
        let msg = PublishMsg {
            request_id,
            seq,
            data: data.to_string(),
            done: false,
        };
        if let Err(e) = self.shard_for(request_id).try_send(msg) {
            tracing::debug!(%request_id, seq, error = %e, "chunk relay publish dropped");
        }
    }

    /// Call on every daemon-side exit path (success, timeout, parse error),
    /// not just success, so a subscriber always gets a clean stop signal.
    pub fn publish_done_nonblocking(&self, request_id: Uuid, seq: u64) {
        let msg = PublishMsg {
            request_id,
            seq,
            data: String::new(),
            done: true,
        };
        if let Err(e) = self.shard_for(request_id).try_send(msg) {
            tracing::debug!(%request_id, seq, error = %e, "chunk relay done-sentinel dropped");
        }
    }

    /// Reads from the start of the stream, so a late subscriber still sees
    /// earlier chunks.
    ///
    /// Capacity must absorb one reader-loop tick's worth of a burst. A
    /// bursty upstream can hand the daemon hundreds of chunks between ticks,
    /// and the reader forwards a subscriber's whole backlog in one pass. Too
    /// small silently drops the subscriber (`try_send` fails once, the
    /// reader removes the registry entry) and hands the request to the poll
    /// fallback with no error surfaced. This was 64 and hit in practice.
    pub fn subscribe(&self, request_id: Uuid) -> ChunkStream {
        let (tx, rx) = mpsc::channel(4096);
        self.registry.insert(
            request_id,
            ReaderEntry {
                tx,
                last_id: "0".to_string(),
            },
        );
        ReceiverStream::new(rx)
    }

    /// Dropping the stream also works (the reader notices a closed channel
    /// and removes the entry); this just skips one extra poll cycle.
    pub fn unsubscribe(&self, request_id: &Uuid) {
        self.registry.remove(request_id);
    }
}

/// Max messages pipelined into one Redis round trip. A burst of many
/// requests finishing at once can queue tens of thousands of messages
/// behind a single shard, so this bounds each pipeline call instead of
/// letting it grow without limit.
const MAX_PUBLISH_BATCH: usize = 256;

/// Drains whatever's already queued (up to `MAX_PUBLISH_BATCH`) into one
/// pipelined Redis call instead of one round trip per message, and holds
/// its connection for the task's lifetime instead of re-acquiring it every
/// message. One round trip per message was the actual bottleneck this was
/// built to fix. Under a burst of concurrent requests finishing close
/// together, a shard's queue built up tens of thousands of messages, and
/// awaiting each round trip serially measured a 20-30s gap between a
/// request's last chunk being queued and it landing in Redis. That made
/// the live-relay-vs-poll-fallback race hopeless no matter how the reader
/// or the race itself was tuned. Pipelining turns it into O(1) round trips
/// per batch instead of O(n).
async fn publisher_loop(pool: Pool, mut rx: mpsc::Receiver<PublishMsg>, ttl_secs: u64, maxlen: usize) {
    let mut conn = None;

    loop {
        let Some(first) = rx.recv().await else { return };
        let mut batch = vec![first];
        while batch.len() < MAX_PUBLISH_BATCH {
            match rx.try_recv() {
                Ok(msg) => batch.push(msg),
                Err(_) => break,
            }
        }

        let c = match conn.as_mut() {
            Some(c) => c,
            None => match pool.get().await {
                Ok(c) => conn.insert(c),
                Err(e) => {
                    tracing::debug!(error = %e, batch_len = batch.len(), "chunk relay: redis unreachable, dropping publish batch");
                    continue;
                }
            },
        };

        let keys: Vec<String> = batch.iter().map(|m| stream_key(m.request_id)).collect();
        let mut pipeline = pipe();
        for (msg, key) in batch.iter().zip(&keys) {
            pipeline
                .cmd("XADD")
                .arg(key)
                .arg("MAXLEN")
                .arg("~")
                .arg(maxlen)
                .arg("*")
                .arg("seq")
                .arg(msg.seq)
                .arg("data")
                .arg(&msg.data)
                .arg("done")
                .arg(if msg.done { "1" } else { "0" })
                .ignore();
            // Refresh the TTL periodically rather than on every message.
            // Halves command volume, and the stream only needs to outlive
            // its last write by roughly stream_ttl_secs, not exactly.
            if msg.seq % EXPIRE_REFRESH_EVERY == 0 || msg.done {
                pipeline.cmd("EXPIRE").arg(key).arg(ttl_secs).ignore();
            }
        }

        if let Err(e) = pipeline.query_async::<()>(c).await {
            tracing::debug!(error = %e, batch_len = batch.len(), "chunk relay: pipelined publish batch failed");
            conn = None; // connection may be bad; get a fresh one next batch
            continue;
        }
    }
}

/// One `XREAD` per tick across every subscribed stream, fanned out to each
/// subscriber's channel — the fix for the connection-per-reader scaling
/// limit a naive per-subscriber `XREAD BLOCK` would hit.
async fn reader_loop(pool: Pool, registry: Arc<DashMap<Uuid, ReaderEntry>>, poll_interval: Duration, shard: usize, n_shards: usize) {
    loop {
        tokio::time::sleep(poll_interval).await;

        let ids: Vec<Uuid> = registry
            .iter()
            .map(|entry| *entry.key())
            .filter(|id| (id.as_u128() % n_shards as u128) as usize == shard)
            .collect();
        if ids.is_empty() {
            continue;
        }

        let mut conn = match pool.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(error = %e, "chunk relay: redis unreachable, reader retrying next tick");
                continue;
            }
        };

        let mut command = cmd("XREAD");
        command.arg("COUNT").arg(500).arg("STREAMS");
        for id in &ids {
            command.arg(stream_key(*id));
        }
        for id in &ids {
            let last_id = registry.get(id).map(|e| e.last_id.clone()).unwrap_or_else(|| "0".to_string());
            command.arg(last_id);
        }

        let reply = match command.query_async::<Option<StreamReadReply>>(&mut conn).await {
            Ok(Some(reply)) => reply,
            Ok(None) => continue,
            Err(e) => {
                tracing::debug!(error = %e, "chunk relay XREAD failed");
                continue;
            }
        };

        for stream in reply.keys {
            let Some(request_id) = parse_stream_key(&stream.key) else {
                continue;
            };

            for entry_id in stream.ids {
                let fields = field_map(&entry_id.map);
                let seq: u64 = fields.get("seq").and_then(|s| s.parse().ok()).unwrap_or(0);
                let data = fields.get("data").cloned().unwrap_or_default();
                let done = fields.get("done").map(String::as_str) == Some("1");

                let delivered = registry
                    .get(&request_id)
                    .map(|subscriber| subscriber.tx.try_send(ChunkMsg { seq, data, done }).is_ok())
                    .unwrap_or(false);

                if !delivered {
                    registry.remove(&request_id);
                    continue;
                }

                if let Some(mut subscriber) = registry.get_mut(&request_id) {
                    subscriber.last_id = entry_id.id.clone();
                }
                if done {
                    registry.remove(&request_id);
                }
            }
        }
    }
}

fn field_map(fields: &HashMap<String, Value>) -> HashMap<String, String> {
    fields
        .iter()
        .filter_map(|(k, v)| value_to_string(v).map(|v| (k.clone(), v)))
        .collect()
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::BulkString(bytes) => String::from_utf8(bytes.clone()).ok(),
        Value::SimpleString(s) => Some(s.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_key_roundtrips_through_parse() {
        let id = Uuid::new_v4();
        assert_eq!(parse_stream_key(&stream_key(id)), Some(id));
    }

    #[test]
    fn parse_stream_key_rejects_wrong_prefix() {
        assert_eq!(parse_stream_key("not:a:relay:key"), None);
    }

    #[test]
    fn is_unreachable_distinguishes_error_kinds() {
        assert!(ChunkRelayError::Unreachable("x".into()).is_unreachable());
        assert!(!ChunkRelayError::Config("x".into()).is_unreachable());
    }

    // Needs a real Redis; override TEST_REDIS_URL if `just redis-start`'s
    // default doesn't apply.
    fn test_config() -> ChunkRelayConfig {
        ChunkRelayConfig {
            redis_url: std::env::var("TEST_REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".to_string()),
            stream_ttl_secs: 60,
            maxlen: 2000,
            publish_channel_capacity: 1024,
            reader_poll_interval_ms: 20,
            publish_workers: 2,
            reader_workers: 2,
        }
    }

    async fn collect_until_done(stream: &mut ChunkStream, timeout: Duration) -> Vec<ChunkMsg> {
        use futures::StreamExt;
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                panic!("no done sentinel within {timeout:?}; collected so far: {out:?}");
            }
            match tokio::time::timeout(remaining, stream.next()).await {
                Ok(Some(msg)) => {
                    let done = msg.done;
                    out.push(msg);
                    if done {
                        return out;
                    }
                }
                Ok(None) => panic!("stream ended without a done sentinel; collected so far: {out:?}"),
                Err(_) => panic!("timed out waiting for the next chunk; collected so far: {out:?}"),
            }
        }
    }

    async fn raw_conn(cfg: &ChunkRelayConfig) -> deadpool_redis::Connection {
        let pool = RedisConfig::from_url(cfg.redis_url.clone())
            .create_pool(Some(Runtime::Tokio1))
            .expect("redis pool for test setup");
        pool.get().await.expect("redis connection for test setup")
    }

    async fn wait_for_xlen_at_least(conn: &mut deadpool_redis::Connection, key: &str, min_len: i64, timeout: Duration) -> i64 {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let len: i64 = cmd("XLEN").arg(key).query_async(conn).await.unwrap_or(0);
            if len >= min_len {
                return len;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("XLEN({key}) did not reach {min_len} within {timeout:?} (last seen {len})");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    // EXPIRE is a separate round trip after XADD, so this is polled
    // independently rather than assumed from XLEN.
    async fn wait_for_ttl_positive(conn: &mut deadpool_redis::Connection, key: &str, timeout: Duration) -> i64 {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let ttl: i64 = cmd("TTL").arg(key).query_async(conn).await.unwrap_or(-1);
            if ttl > 0 {
                return ttl;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("TTL({key}) did not become positive within {timeout:?} (last seen {ttl})");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn publish_then_subscribe_delivers_in_order() {
        let relay = ChunkRelay::from_config(&test_config()).expect("relay should build against local redis");
        let request_id = Uuid::new_v4();
        let mut stream = relay.subscribe(request_id);

        relay.publish_nonblocking(request_id, 0, "hello");
        relay.publish_nonblocking(request_id, 1, "world");
        relay.publish_done_nonblocking(request_id, 2);

        let received = collect_until_done(&mut stream, Duration::from_secs(5)).await;

        let data: Vec<&str> = received.iter().map(|m| m.data.as_str()).collect();
        assert_eq!(data, vec!["hello", "world", ""]);
        assert!(!received[0].done);
        assert!(!received[1].done);
        assert!(received[2].done);
    }

    #[tokio::test]
    async fn late_subscriber_still_sees_earlier_chunks() {
        let cfg = test_config();
        let relay = ChunkRelay::from_config(&cfg).expect("relay should build against local redis");
        let request_id = Uuid::new_v4();

        relay.publish_nonblocking(request_id, 0, "early");
        let mut conn = raw_conn(&cfg).await;
        wait_for_xlen_at_least(&mut conn, &stream_key(request_id), 1, Duration::from_secs(5)).await;

        let mut stream = relay.subscribe(request_id);
        relay.publish_done_nonblocking(request_id, 1);

        let received = collect_until_done(&mut stream, Duration::from_secs(5)).await;
        assert_eq!(received[0].data, "early");
        assert!(received.last().unwrap().done);
    }

    #[tokio::test]
    async fn maxlen_trims_and_expire_is_applied() {
        let mut cfg = test_config();
        cfg.maxlen = 5;
        cfg.stream_ttl_secs = 120;
        // This test's burst-publishes far faster than real (paced) traffic
        // would, so the reader needs a tighter tick to keep up with it.
        cfg.reader_poll_interval_ms = 1;
        let relay = ChunkRelay::from_config(&cfg).expect("relay should build against local redis");
        let request_id = Uuid::new_v4();
        let key = stream_key(request_id);

        // Approximate (~) trimming only evicts a whole internal node
        // (~100 entries) at a time, so this needs enough volume to cross
        // that boundary.
        const PUBLISHED: u64 = 300;

        let mut stream = relay.subscribe(request_id);
        for i in 0..PUBLISHED - 1 {
            relay.publish_nonblocking(request_id, i, "x");
            // Let the publisher/reader tasks interleave, like real (paced)
            // traffic would, instead of racing ahead of the reader.
            tokio::task::yield_now().await;
        }
        relay.publish_done_nonblocking(request_id, PUBLISHED - 1);
        collect_until_done(&mut stream, Duration::from_secs(10)).await;

        let mut conn = raw_conn(&cfg).await;
        let len: i64 = cmd("XLEN").arg(&key).query_async(&mut conn).await.expect("XLEN query");
        assert!(
            len < PUBLISHED as i64 - 50,
            "expected MAXLEN ~ {} to have evicted at least one node's worth of the {PUBLISHED} published entries, got {len}",
            cfg.maxlen
        );

        let ttl = wait_for_ttl_positive(&mut conn, &key, Duration::from_secs(5)).await;
        assert!(
            ttl <= cfg.stream_ttl_secs as i64,
            "expected TTL <= {}s, got {ttl}",
            cfg.stream_ttl_secs
        );
    }

    #[tokio::test]
    async fn publish_nonblocking_never_blocks_even_when_redis_is_unreachable() {
        // TEST-NET-1 (RFC 5737): non-routable, so the publisher stalls
        // without ever landing a message.
        let cfg = ChunkRelayConfig {
            redis_url: "redis://192.0.2.1:6379".to_string(),
            publish_channel_capacity: 1,
            ..test_config()
        };
        let relay = ChunkRelay::from_config(&cfg).expect("pool construction doesn't itself connect");
        let request_id = Uuid::new_v4();

        let started = tokio::time::Instant::now();
        for i in 0..50u64 {
            relay.publish_nonblocking(request_id, i, "x");
        }
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(200),
            "50 publishes against an unreachable redis and a channel capacity of 1 took {elapsed:?} — publish_nonblocking must never block"
        );
    }
}
