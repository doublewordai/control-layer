use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use arc_swap::ArcSwapOption;
use dashmap::DashMap;
use futures::future::BoxFuture;
use metrics::counter;
use onwards::inflight::{InflightLimiter, InflightSlot};
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use tokio::runtime::{Builder as RuntimeBuilder, Runtime};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant as TokioInstant, interval_at, sleep, timeout};
use tracing::warn;
use uuid::Uuid;

use crate::config::RealtimeInflightLimitsConfig;
use crate::metrics::errors::component::REALTIME_INFLIGHT;

const LEASE: Duration = Duration::from_secs(90);
const RENEW_EVERY: Duration = Duration::from_secs(30);
const HEARTBEAT_EVERY: Duration = Duration::from_secs(5);
const POD_LEASE: Duration = Duration::from_secs(15);
const REDIS_TIMEOUT: Duration = Duration::from_millis(250);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(1);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const RECONNECT_AFTER_TIMEOUTS: u32 = 3;
const RECONNECT_BACKOFF: Duration = Duration::from_secs(1);
const RELEASE_RETRY_EVERY: Duration = Duration::from_secs(1);
const SYNC_BATCH_LIMIT: usize = 512;

const CLAIM_SCRIPT: &str = r"
local now = redis.call('TIME')
local now_ms = tonumber(now[1]) * 1000 + math.floor(tonumber(now[2]) / 1000)
redis.call('ZREMRANGEBYSCORE', KEYS[1], '-inf', now_ms)
if redis.call('ZCARD', KEYS[1]) >= tonumber(ARGV[1]) then
  return 0
end
redis.call('ZADD', KEYS[1], now_ms + tonumber(ARGV[3]), ARGV[2])
redis.call('PEXPIRE', KEYS[1], tonumber(ARGV[3]))
return 1
";

const SYNC_KEY_SCRIPT: &str = r"
local now = redis.call('TIME')
local exp = tonumber(now[1]) * 1000 + math.floor(tonumber(now[2]) / 1000) + tonumber(ARGV[1])
for i = 2, #ARGV do
  redis.call('ZADD', KEYS[1], exp, ARGV[i])
end
redis.call('PEXPIRE', KEYS[1], tonumber(ARGV[1]))
return #ARGV - 1
";

const HEARTBEAT_SCRIPT: &str = r"
local now = redis.call('TIME')
local now_ms = tonumber(now[1]) * 1000 + math.floor(tonumber(now[2]) / 1000)
local ttl = tonumber(ARGV[2])
redis.call('ZREMRANGEBYSCORE', KEYS[1], '-inf', now_ms)
redis.call('ZADD', KEYS[1], now_ms + ttl, ARGV[1])
redis.call('PEXPIRE', KEYS[1], ttl)
return redis.call('ZCARD', KEYS[1])
";

#[derive(Default)]
struct PodInflightCounter {
    counts: DashMap<(String, String), Arc<AtomicU32>>,
}

struct PodCountGuard(Arc<AtomicU32>);

impl Drop for PodCountGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl PodInflightCounter {
    fn counter(&self, account: &str, model: &str) -> Arc<AtomicU32> {
        if let Some(existing) = self.counts.get(&(account.to_owned(), model.to_owned())) {
            return Arc::clone(&existing);
        }
        Arc::clone(&self.counts.entry((account.to_owned(), model.to_owned())).or_default())
    }

    fn track(&self, account: &str, model: &str) -> PodCountGuard {
        let count = self.counter(account, model);
        count.fetch_add(1, Ordering::AcqRel);
        PodCountGuard(count)
    }

    fn acquire(&self, account: &str, model: &str, limit: u32) -> Option<PodCountGuard> {
        let count = self.counter(account, model);
        let mut current = count.load(Ordering::Acquire);
        loop {
            if current >= limit {
                return None;
            }
            match count.compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Some(PodCountGuard(count)),
                Err(actual) => current = actual,
            }
        }
    }
}

struct ActiveSlot {
    scope: &'static str,
    redis_key: String,
    member: String,
    in_redis: AtomicBool,
}

struct SlotSnapshot {
    id: u64,
    scope: &'static str,
    member: String,
}

struct RedisState {
    pod_id: String,
    client: redis::Client,
    connection: ArcSwapOption<ConnectionManager>,
    consecutive_timeouts: AtomicU32,
    connecting: AtomicBool,
    next_slot_id: AtomicU64,
    active_slots: DashMap<u64, ActiveSlot>,
    scopes: DashMap<&'static str, AtomicU32>,
}

impl RedisState {
    fn current(&self) -> Option<Arc<ConnectionManager>> {
        self.connection.load_full()
    }

    fn connection(&self) -> Option<ConnectionManager> {
        self.current().map(|connection| ConnectionManager::clone(&connection))
    }

    fn is_current(&self, connection: &Arc<ConnectionManager>) -> bool {
        self.connection
            .load()
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, connection))
    }

    fn answered(&self) {
        self.consecutive_timeouts.store(0, Ordering::Relaxed);
    }

    fn timed_out(self: &Arc<Self>, attempted: &Arc<ConnectionManager>) {
        if !self.is_current(attempted) {
            return;
        }
        if self.consecutive_timeouts.fetch_add(1, Ordering::Relaxed) + 1 >= RECONNECT_AFTER_TIMEOUTS && self.reconnect() {
            counter!("dwctl_realtime_inflight_redis_reconnects_total").increment(1);
        }
    }

    fn reconnect_broken(self: &Arc<Self>, attempted: &Arc<ConnectionManager>) {
        if self.is_current(attempted) && self.reconnect() {
            counter!("dwctl_realtime_inflight_redis_reconnects_total").increment(1);
        }
    }

    fn insert_slot(&self, scope: &'static str, redis_key: String, member: String, in_redis: bool) -> u64 {
        let id = self.next_slot_id.fetch_add(1, Ordering::Relaxed);
        self.active_slots.insert(
            id,
            ActiveSlot {
                scope,
                redis_key,
                member,
                in_redis: AtomicBool::new(in_redis),
            },
        );
        id
    }

    fn reconnect(self: &Arc<Self>) -> bool {
        if self.connecting.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.connection.store(None);
        self.consecutive_timeouts.store(0, Ordering::Relaxed);
        let state = Arc::clone(self);
        tokio::spawn(async move {
            let config = ConnectionManagerConfig::new()
                .set_connection_timeout(CONNECT_TIMEOUT)
                .set_response_timeout(RESPONSE_TIMEOUT)
                .set_number_of_retries(0);
            let connection = loop {
                match ConnectionManager::new_with_config(state.client.clone(), config.clone()).await {
                    Ok(connection) => break connection,
                    Err(error) => {
                        crate::background_error!(
                            REALTIME_INFLIGHT,
                            "redis_connect",
                            Warning,
                            error = %error,
                            "Failed to connect to the in-flight Redis; counting in this pod until it connects"
                        );
                        sleep(RECONNECT_BACKOFF).await;
                    }
                }
            };
            let shared = Arc::new(connection);
            state.connection.store(Some(Arc::clone(&shared)));
            state.connecting.store(false, Ordering::Release);
            tokio::spawn(sync_active_slots(Arc::clone(&state), ConnectionManager::clone(&shared)));
            tokio::spawn(refresh_heartbeats(state, ConnectionManager::clone(&shared)));
        });
        true
    }
}

enum ClaimOutcome {
    Admitted(SlotLease),
    Refused,
    NotConnected,
    Failed(redis::RedisError),
    TimedOut,
}

enum RedisCommand {
    Claim {
        scope: &'static str,
        key: String,
        limit: u32,
        release_tx: mpsc::UnboundedSender<RedisCommand>,
        reply: oneshot::Sender<ClaimOutcome>,
    },
    Release {
        scope: &'static str,
        key: String,
        member: String,
    },
    HeartbeatNow(&'static str),
}

struct SlotLease {
    id: u64,
    state: Arc<RedisState>,
    commands: mpsc::UnboundedSender<RedisCommand>,
}

impl Drop for SlotLease {
    fn drop(&mut self) {
        if let Some((_, slot)) = self.state.active_slots.remove(&self.id)
            && slot.in_redis.load(Ordering::Acquire)
        {
            let _ = self.commands.send(RedisCommand::Release {
                scope: slot.scope,
                key: slot.redis_key,
                member: slot.member,
            });
        }
    }
}

pub struct SharedRedis {
    state: Arc<RedisState>,
    commands: mpsc::UnboundedSender<RedisCommand>,
}

impl SharedRedis {
    fn connect(url: &str) -> anyhow::Result<Arc<Self>> {
        let client = redis::Client::open(url)?;
        let runtime = RuntimeBuilder::new_current_thread().enable_all().build()?;
        let (commands, rx) = mpsc::unbounded_channel();
        let state = Arc::new(RedisState {
            pod_id: Uuid::new_v4().to_string(),
            client,
            connection: ArcSwapOption::empty(),
            consecutive_timeouts: AtomicU32::new(0),
            connecting: AtomicBool::new(false),
            next_slot_id: AtomicU64::new(0),
            active_slots: DashMap::new(),
            scopes: DashMap::new(),
        });
        let worker_state = Arc::clone(&state);
        thread::Builder::new()
            .name("inflight-redis".to_string())
            .spawn(move || run_redis_thread(runtime, worker_state, rx))?;
        Ok(Arc::new(Self { state, commands }))
    }

    fn is_connected(&self) -> bool {
        self.state.connection.load().is_some()
    }

    fn activate_scope(&self, scope: &'static str) {
        if !self.state.scopes.contains_key(scope) {
            self.state.scopes.entry(scope).or_insert_with(|| AtomicU32::new(1));
            let _ = self.commands.send(RedisCommand::HeartbeatNow(scope));
        }
    }

    fn live_pods(&self, scope: &'static str) -> u32 {
        self.state
            .scopes
            .get(scope)
            .map(|pods| pods.load(Ordering::Acquire).max(1))
            .unwrap_or(1)
    }

    fn register_fallback_slot(&self, scope: &'static str, redis_key: String) -> SlotLease {
        let id = self.state.insert_slot(scope, redis_key, Uuid::new_v4().to_string(), false);
        SlotLease {
            id,
            state: Arc::clone(&self.state),
            commands: self.commands.clone(),
        }
    }
}

pub struct RealtimeInflightLimiter {
    scope: &'static str,
    enforce: bool,
    redis: Option<Arc<SharedRedis>>,
    pod_counts: PodInflightCounter,
    exempt_account: String,
}

impl fmt::Debug for RealtimeInflightLimiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RealtimeInflightLimiter")
            .field("scope", &self.scope)
            .field("enforce", &self.enforce)
            .field("redis", &self.redis.is_some())
            .finish()
    }
}

impl RealtimeInflightLimiter {
    pub fn from_config(config: &RealtimeInflightLimitsConfig) -> anyhow::Result<Self> {
        Ok(Self::from_parts(
            "realtime",
            config.enforce,
            Self::shared_redis(config.redis_url.as_deref())?,
        ))
    }

    pub(crate) fn shared_redis(redis_url: Option<&str>) -> anyhow::Result<Option<Arc<SharedRedis>>> {
        redis_url.map(SharedRedis::connect).transpose()
    }

    pub fn from_parts(scope: &'static str, enforce: bool, redis: Option<Arc<SharedRedis>>) -> Self {
        Self {
            scope,
            enforce,
            redis,
            pod_counts: PodInflightCounter::default(),
            exempt_account: Uuid::nil().to_string(),
        }
    }

    fn acquire_without_redis(&self, account: &str, model: &str, limit: u32) -> Option<InflightSlot> {
        self.pod_counts.acquire(account, model, limit).map(InflightSlot::new)
    }

    fn acquire_fallback(&self, redis: &SharedRedis, account: &str, model: &str, key: String, limit: u32) -> Option<InflightSlot> {
        let pod_limit = limit.div_ceil(redis.live_pods(self.scope));
        let pod_guard = self.pod_counts.acquire(account, model, pod_limit)?;
        let fallback_lease = redis.register_fallback_slot(self.scope, key);
        Some(InflightSlot::new((fallback_lease, pod_guard)))
    }
}

fn inflight_key(model: &str, account: &str) -> String {
    format!("dwctl:inflight:{model}:{account}")
}

fn heartbeat_key(scope: &str) -> String {
    format!("dwctl:inflight_pods:{scope}")
}

impl InflightLimiter for RealtimeInflightLimiter {
    fn try_acquire<'a>(&'a self, account: &'a str, model: &'a str, limit: u32) -> BoxFuture<'a, Option<InflightSlot>> {
        Box::pin(async move {
            if !self.enforce || account == self.exempt_account {
                return Some(InflightSlot::new(()));
            }
            let Some(redis) = &self.redis else {
                return self.acquire_without_redis(account, model, limit);
            };
            redis.activate_scope(self.scope);
            let key = inflight_key(model, account);
            if !redis.is_connected() {
                warn!(scope = self.scope, "In-flight Redis is not connected; counting in this pod instead");
                counter!("dwctl_realtime_inflight_redis_fallbacks_total", "scope" => self.scope).increment(1);
                return self.acquire_fallback(redis, account, model, key, limit);
            }
            let (reply_tx, reply_rx) = oneshot::channel();
            if redis
                .commands
                .send(RedisCommand::Claim {
                    scope: self.scope,
                    key: key.clone(),
                    limit,
                    release_tx: redis.commands.clone(),
                    reply: reply_tx,
                })
                .is_err()
            {
                warn!(scope = self.scope, "In-flight Redis worker stopped; counting in this pod instead");
                counter!("dwctl_realtime_inflight_redis_fallbacks_total", "scope" => self.scope).increment(1);
                return self.acquire_fallback(redis, account, model, key, limit);
            }
            match reply_rx.await {
                Ok(ClaimOutcome::Admitted(redis_lease)) => {
                    let pod_guard = self.pod_counts.track(account, model);
                    Some(InflightSlot::new((redis_lease, pod_guard)))
                }
                Ok(ClaimOutcome::Refused) => None,
                Ok(ClaimOutcome::NotConnected) => {
                    warn!(scope = self.scope, "In-flight Redis is not connected; counting in this pod instead");
                    counter!("dwctl_realtime_inflight_redis_fallbacks_total", "scope" => self.scope).increment(1);
                    self.acquire_fallback(redis, account, model, key, limit)
                }
                Ok(ClaimOutcome::Failed(error)) => {
                    warn!(scope = self.scope, error = %error, "In-flight claim failed; counting in this pod instead");
                    counter!("dwctl_realtime_inflight_redis_fallbacks_total", "scope" => self.scope).increment(1);
                    self.acquire_fallback(redis, account, model, key, limit)
                }
                Ok(ClaimOutcome::TimedOut) | Err(_) => {
                    warn!(scope = self.scope, "In-flight claim timed out; counting in this pod instead");
                    counter!("dwctl_realtime_inflight_redis_fallbacks_total", "scope" => self.scope).increment(1);
                    self.acquire_fallback(redis, account, model, key, limit)
                }
            }
        })
    }
}

fn run_redis_thread(runtime: Runtime, state: Arc<RedisState>, mut rx: mpsc::UnboundedReceiver<RedisCommand>) {
    runtime.block_on(async move {
        state.reconnect();
        let mut renew_ticks = interval_at(TokioInstant::now() + RENEW_EVERY, RENEW_EVERY);
        let mut heartbeat_ticks = interval_at(TokioInstant::now() + HEARTBEAT_EVERY, HEARTBEAT_EVERY);
        loop {
            tokio::select! {
                command = rx.recv() => {
                    let Some(command) = command else {
                        break;
                    };
                    match command {
                        RedisCommand::Claim {
                            scope,
                            key,
                            limit,
                            release_tx,
                            reply,
                        } => {
                            spawn_claim(Arc::clone(&state), release_tx, scope, key, limit, reply);
                        }
                        RedisCommand::Release { scope, key, member } => {
                            spawn_release(Arc::clone(&state), scope, key, member);
                        }
                        RedisCommand::HeartbeatNow(scope) => {
                            if let Some(connection) = state.connection() {
                                tokio::spawn(heartbeat_scope(Arc::clone(&state), connection, scope));
                            }
                        }
                    }
                }
                _ = renew_ticks.tick() => {
                    if let Some(connection) = state.connection() {
                        tokio::spawn(sync_active_slots(Arc::clone(&state), connection));
                    }
                }
                _ = heartbeat_ticks.tick() => {
                    if let Some(connection) = state.connection() {
                        tokio::spawn(refresh_heartbeats(Arc::clone(&state), connection));
                    }
                }
            }
        }
    });
}

fn spawn_claim(
    state: Arc<RedisState>,
    release_sender: mpsc::UnboundedSender<RedisCommand>,
    scope: &'static str,
    key: String,
    limit: u32,
    reply: oneshot::Sender<ClaimOutcome>,
) {
    let Some(current) = state.current() else {
        let _ = reply.send(ClaimOutcome::NotConnected);
        return;
    };
    tokio::spawn(async move {
        let mut connection = ConnectionManager::clone(&current);
        let member = Uuid::new_v4().to_string();
        match timeout(REDIS_TIMEOUT, claim(&mut connection, &key, &member, limit)).await {
            Ok(Ok(true)) => {
                state.answered();
                let id = state.insert_slot(scope, key, member, true);
                let lease = SlotLease {
                    id,
                    state,
                    commands: release_sender,
                };
                let _ = reply.send(ClaimOutcome::Admitted(lease));
            }
            Ok(Ok(false)) => {
                state.answered();
                let _ = reply.send(ClaimOutcome::Refused);
            }
            Ok(Err(error)) => {
                if error.is_connection_dropped() || error.is_io_error() {
                    state.reconnect_broken(&current);
                }
                let _ = reply.send(ClaimOutcome::Failed(error));
            }
            Err(_) => {
                state.timed_out(&current);
                if state.is_current(&current) {
                    tokio::spawn(async move {
                        if let Err(error) = remove(&mut connection, &key, &member).await {
                            crate::background_error!(
                                REALTIME_INFLIGHT,
                                "claim_withdraw",
                                Warning,
                                scope = scope,
                                error = %error,
                                "Failed to withdraw a timed-out in-flight claim; its lease will expire"
                            );
                        }
                    });
                }
                let _ = reply.send(ClaimOutcome::TimedOut);
            }
        }
    });
}

fn spawn_release(state: Arc<RedisState>, scope: &'static str, key: String, member: String) {
    tokio::spawn(async move {
        let give_up_at = TokioInstant::now() + LEASE;
        loop {
            let error = match state.connection() {
                Some(mut connection) => match timeout(REDIS_TIMEOUT, remove(&mut connection, &key, &member)).await {
                    Ok(Ok(())) => {
                        state.answered();
                        return;
                    }
                    Ok(Err(error)) => anyhow::Error::from(error),
                    Err(_) => anyhow::anyhow!("in-flight Redis release timed out"),
                },
                None => anyhow::anyhow!("in-flight Redis is not connected"),
            };
            if TokioInstant::now() + RELEASE_RETRY_EVERY >= give_up_at {
                crate::background_error!(
                    REALTIME_INFLIGHT,
                    "lease_release",
                    Warning,
                    scope = scope,
                    error = %error,
                    "Could not release an in-flight slot before its lease expired"
                );
                return;
            }
            sleep(RELEASE_RETRY_EVERY).await;
        }
    });
}

async fn sync_active_slots(state: Arc<RedisState>, mut connection: ConnectionManager) {
    let mut by_key: HashMap<String, Vec<SlotSnapshot>> = HashMap::new();
    for entry in state.active_slots.iter() {
        by_key.entry(entry.redis_key.clone()).or_default().push(SlotSnapshot {
            id: *entry.key(),
            scope: entry.scope,
            member: entry.member.clone(),
        });
    }
    for (redis_key, slots) in by_key {
        for chunk in slots.chunks(SYNC_BATCH_LIMIT) {
            let scope = chunk[0].scope;
            let mut cmd = redis::cmd("EVAL");
            cmd.arg(SYNC_KEY_SCRIPT).arg(1).arg(&redis_key).arg(LEASE.as_millis() as u64);
            for slot in chunk {
                cmd.arg(&slot.member);
            }
            match timeout(REDIS_TIMEOUT, cmd.query_async::<i64>(&mut connection)).await {
                Ok(Ok(_)) => {
                    state.answered();
                    for slot in chunk {
                        match state.active_slots.get(&slot.id) {
                            Some(active) => active.in_redis.store(true, Ordering::Release),
                            None => spawn_release(Arc::clone(&state), slot.scope, redis_key.clone(), slot.member.clone()),
                        }
                    }
                }
                Ok(Err(error)) => {
                    crate::background_error!(
                        REALTIME_INFLIGHT,
                        "lease_renew",
                        Warning,
                        scope = scope,
                        error = %error,
                        "Failed to renew an in-flight lease"
                    );
                }
                Err(_) => {
                    crate::background_error!(
                        REALTIME_INFLIGHT,
                        "lease_renew",
                        Warning,
                        scope = scope,
                        error = "timed out",
                        "Failed to renew an in-flight lease"
                    );
                }
            }
        }
    }
}

async fn refresh_heartbeats(state: Arc<RedisState>, connection: ConnectionManager) {
    let active_scopes: Vec<&'static str> = state.scopes.iter().map(|entry| *entry.key()).collect();
    for scope in active_scopes {
        heartbeat_scope(Arc::clone(&state), connection.clone(), scope).await;
    }
}

async fn heartbeat_scope(state: Arc<RedisState>, mut connection: ConnectionManager, scope: &'static str) {
    let key = heartbeat_key(scope);
    let result = timeout(
        REDIS_TIMEOUT,
        redis::cmd("EVAL")
            .arg(HEARTBEAT_SCRIPT)
            .arg(1)
            .arg(&key)
            .arg(&state.pod_id)
            .arg(POD_LEASE.as_millis() as u64)
            .query_async::<i64>(&mut connection),
    )
    .await;
    match result {
        Ok(Ok(live_pods)) => {
            state.answered();
            if let Some(cached) = state.scopes.get(scope) {
                cached.store(live_pods.max(1) as u32, Ordering::Release);
            }
        }
        Ok(Err(error)) => {
            crate::background_error!(
                REALTIME_INFLIGHT,
                "pod_heartbeat",
                Warning,
                scope = scope,
                error = %error,
                "Failed to refresh in-flight pod heartbeat"
            );
        }
        Err(_) => {
            crate::background_error!(
                REALTIME_INFLIGHT,
                "pod_heartbeat",
                Warning,
                scope = scope,
                error = "timed out",
                "Failed to refresh in-flight pod heartbeat"
            );
        }
    }
}

async fn claim(connection: &mut ConnectionManager, key: &str, member: &str, limit: u32) -> redis::RedisResult<bool> {
    let admitted: i64 = redis::cmd("EVAL")
        .arg(CLAIM_SCRIPT)
        .arg(1)
        .arg(key)
        .arg(limit)
        .arg(member)
        .arg(LEASE.as_millis() as u64)
        .query_async(connection)
        .await?;
    Ok(admitted == 1)
}

async fn remove(connection: &mut ConnectionManager, key: &str, member: &str) -> redis::RedisResult<()> {
    redis::cmd("ZREM").arg(key).arg(member).query_async::<i64>(connection).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Barrier, Mutex};
    use std::task::{Context, Waker};
    use std::time::Instant;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;

    fn redis_url() -> String {
        std::env::var("INFLIGHT_TEST_REDIS_URL").expect("INFLIGHT_TEST_REDIS_URL names a Redis server")
    }

    #[test]
    fn batch_and_realtime_count_under_distinct_keys() {
        let account = "11111111-1111-1111-1111-111111111111";
        let realtime = inflight_key("model", account);
        let batch = inflight_key("model", onwards::inflight::BATCH_INFLIGHT_SCOPE);
        assert_ne!(realtime, batch);
        assert!(batch.ends_with(&format!(":{}", onwards::inflight::BATCH_INFLIGHT_SCOPE)));
        assert!(!batch.contains(account), "batch keys are not account-scoped");
    }

    fn limiter(redis_url: Option<String>) -> RealtimeInflightLimiter {
        RealtimeInflightLimiter::from_config(&RealtimeInflightLimitsConfig { enforce: true, redis_url }).unwrap()
    }

    async fn connected(limiter: RealtimeInflightLimiter) -> RealtimeInflightLimiter {
        let redis = limiter.redis.clone().unwrap();
        for _ in 0..500 {
            if redis.is_connected() {
                return limiter;
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("the limiter never connected to Redis");
    }

    async fn test_connection(url: &str) -> redis::aio::MultiplexedConnection {
        redis::Client::open(url).unwrap().get_multiplexed_async_connection().await.unwrap()
    }

    async fn in_flight(url: &str, key: &str) -> i64 {
        redis::cmd("ZCARD")
            .arg(key)
            .query_async(&mut test_connection(url).await)
            .await
            .unwrap()
    }

    type Commands = Arc<Mutex<Vec<(String, String)>>>;
    type Synced = Arc<Mutex<Vec<(String, String)>>>;
    type Pods = Arc<Mutex<HashMap<String, HashSet<String>>>>;

    #[derive(Default, Clone)]
    struct FakeRedisState {
        commands: Commands,
        synced: Synced,
        pods: Pods,
    }

    fn next_command(bytes: &[u8]) -> Option<(Vec<Vec<u8>>, usize)> {
        let line_end = |from: usize| bytes[from..].windows(2).position(|pair| pair == b"\r\n").map(|at| from + at);
        let header_end = line_end(0)?;
        let count: usize = std::str::from_utf8(&bytes[1..header_end]).ok()?.parse().ok()?;
        let mut position = header_end + 2;
        let mut args = Vec::with_capacity(count);
        for _ in 0..count {
            let length_end = line_end(position)?;
            let length: usize = std::str::from_utf8(&bytes[position + 1..length_end]).ok()?.parse().ok()?;
            let start = length_end + 2;
            if bytes.len() < start + length + 2 {
                return None;
            }
            args.push(bytes[start..start + length].to_vec());
            position = start + length + 2;
        }
        Some((args, position))
    }

    async fn serve(mut socket: TcpStream, claim_reply_after: Option<Duration>, state: FakeRedisState) {
        let mut pending = Vec::new();
        let mut buffer = [0u8; 4096];
        let mut silent = false;
        while let Ok(read) = socket.read(&mut buffer).await {
            if read == 0 {
                return;
            }
            pending.extend_from_slice(&buffer[..read]);
            while let Some((args, used)) = next_command(&pending) {
                pending.drain(..used);
                let name = String::from_utf8_lossy(&args[0]).to_ascii_uppercase();
                let script = args.get(1).map(Vec::as_slice);
                let reply: Vec<u8> = match name.as_str() {
                    "EVAL" if script == Some(CLAIM_SCRIPT.as_bytes()) => {
                        state
                            .commands
                            .lock()
                            .unwrap()
                            .push((name, String::from_utf8_lossy(&args[5]).into_owned()));
                        match claim_reply_after {
                            Some(delay) => {
                                if !delay.is_zero() {
                                    sleep(delay).await;
                                }
                                b":1\r\n".to_vec()
                            }
                            None => {
                                silent = true;
                                Vec::new()
                            }
                        }
                    }
                    "EVAL" if script == Some(HEARTBEAT_SCRIPT.as_bytes()) => match claim_reply_after {
                        Some(_) => {
                            let key = String::from_utf8_lossy(&args[3]).into_owned();
                            let pod_id = String::from_utf8_lossy(&args[4]).into_owned();
                            let count = {
                                let mut pods = state.pods.lock().unwrap();
                                let set = pods.entry(key).or_default();
                                set.insert(pod_id);
                                set.len()
                            };
                            format!(":{count}\r\n").into_bytes()
                        }
                        None => {
                            silent = true;
                            Vec::new()
                        }
                    },
                    "EVAL" if script == Some(SYNC_KEY_SCRIPT.as_bytes()) => match claim_reply_after {
                        Some(_) => {
                            let redis_key = String::from_utf8_lossy(&args[3]).into_owned();
                            {
                                let mut synced = state.synced.lock().unwrap();
                                for member in &args[5..] {
                                    synced.push((redis_key.clone(), String::from_utf8_lossy(member).into_owned()));
                                }
                            }
                            format!(":{}\r\n", args.len().saturating_sub(5)).into_bytes()
                        }
                        None => {
                            silent = true;
                            Vec::new()
                        }
                    },
                    "ZREM" => {
                        state
                            .commands
                            .lock()
                            .unwrap()
                            .push((name, String::from_utf8_lossy(&args[2]).into_owned()));
                        b":1\r\n".to_vec()
                    }
                    _ => b"+OK\r\n".to_vec(),
                };
                if !silent && socket.write_all(&reply).await.is_err() {
                    return;
                }
            }
        }
    }

    async fn fake_redis_with_state(claim_reply_after: Option<Duration>, refuse_first: usize) -> (String, Arc<AtomicUsize>, FakeRedisState) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("redis://{}", listener.local_addr().unwrap());
        let accepted = Arc::new(AtomicUsize::new(0));
        let state = FakeRedisState::default();
        tokio::spawn({
            let accepted = Arc::clone(&accepted);
            let state = state.clone();
            async move {
                while let Ok((socket, _)) = listener.accept().await {
                    if accepted.fetch_add(1, Ordering::SeqCst) >= refuse_first {
                        tokio::spawn(serve(socket, claim_reply_after, state.clone()));
                    }
                }
            }
        });
        (url, accepted, state)
    }

    async fn fake_redis(claim_reply_after: Option<Duration>, refuse_first: usize) -> (String, Arc<AtomicUsize>, Commands) {
        let (url, accepted, state) = fake_redis_with_state(claim_reply_after, refuse_first).await;
        (url, accepted, state.commands)
    }

    #[tokio::test]
    async fn without_redis_each_pod_counts_on_its_own() {
        let limiter = limiter(None);
        let held = limiter.try_acquire("acct", "model", 1).await;
        assert!(held.is_some());
        assert!(limiter.try_acquire("acct", "model", 1).await.is_none());
        drop(held);
        assert!(limiter.try_acquire("acct", "model", 1).await.is_some());
    }

    #[tokio::test]
    async fn nothing_is_limited_until_enforcement_is_switched_on() {
        let limiter = RealtimeInflightLimiter::from_config(&RealtimeInflightLimitsConfig::default()).unwrap();
        let mut held = Vec::new();
        for _ in 0..3 {
            held.push(limiter.try_acquire("acct", "model", 1).await.expect("admitted while not enforcing"));
        }
    }

    #[tokio::test]
    async fn the_system_account_is_never_limited() {
        let limiter = limiter(None);
        let system = Uuid::nil().to_string();
        let mut held = Vec::new();
        for _ in 0..3 {
            held.push(limiter.try_acquire(&system, "model", 1).await.expect("system account admitted"));
        }
    }

    #[tokio::test]
    async fn an_unreachable_redis_falls_back_to_counting_in_the_pod() {
        let limiter = limiter(Some("redis://127.0.0.1:1".to_string()));
        let held = limiter.try_acquire("acct", "model", 1).await;
        assert!(held.is_some());
        assert!(limiter.try_acquire("acct", "model", 1).await.is_none());
    }

    #[tokio::test]
    async fn failed_connection_attempts_are_retried_every_second() {
        let (url, accepted, _) = fake_redis(Some(Duration::ZERO), 2).await;
        let started = Instant::now();
        let limiter = limiter(Some(url));
        assert!(!limiter.redis.as_ref().unwrap().is_connected());
        connected(limiter).await;
        let elapsed = started.elapsed();
        assert_eq!(accepted.load(Ordering::SeqCst), 3);
        assert!(
            elapsed >= RECONNECT_BACKOFF * 2 && elapsed < RECONNECT_BACKOFF * 4,
            "connected after {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_release_is_retried_once_redis_is_back() {
        let (url, _, commands) = fake_redis(Some(Duration::ZERO), 0).await;
        let limiter = connected(limiter(Some(url))).await;
        let redis = limiter.redis.clone().unwrap();
        let removed = || {
            commands
                .lock()
                .unwrap()
                .iter()
                .filter(|(name, _)| name == "ZREM")
                .map(|(_, member)| member.clone())
                .collect::<Vec<_>>()
        };

        let slot = limiter.try_acquire("acct", "model", 10).await.expect("slot");
        let member = commands.lock().unwrap()[0].1.clone();
        let connection = redis.state.connection.load_full();
        redis.state.connection.store(None);
        drop(slot);
        for _ in 0..30 {
            assert!(removed().is_empty());
            sleep(Duration::from_millis(10)).await;
        }

        redis.state.connection.store(connection);
        for _ in 0..500 {
            if !removed().is_empty() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(removed(), vec![member]);
    }

    #[tokio::test]
    async fn a_redis_that_stops_answering_is_replaced_with_a_new_connection() {
        let (url, accepted, commands) = fake_redis(None, 0).await;
        let limiter = connected(limiter(Some(url))).await;
        assert_eq!(accepted.load(Ordering::SeqCst), 1);

        let claims = (0..20).map(|_| limiter.try_acquire("acct", "model", 100));
        assert!(futures::future::join_all(claims).await.iter().all(Option::is_some));
        for _ in 0..500 {
            if accepted.load(Ordering::SeqCst) == 2 {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
        connected(limiter).await;
        let withdrawals = || commands.lock().unwrap().iter().filter(|(name, _)| name == "ZREM").count() as u32;
        for _ in 0..500 {
            if withdrawals() >= RECONNECT_AFTER_TIMEOUTS - 1 {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            withdrawals(),
            RECONNECT_AFTER_TIMEOUTS - 1,
            "claims on a replaced connection are not withdrawn"
        );
    }

    #[tokio::test]
    async fn a_claim_that_answers_after_the_deadline_is_withdrawn() {
        let (url, _, commands) = fake_redis(Some(REDIS_TIMEOUT * 2), 0).await;
        let limiter = connected(limiter(Some(url))).await;

        assert!(limiter.try_acquire("acct", "model", 10).await.is_some());
        for _ in 0..500 {
            if commands.lock().unwrap().len() == 2 {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        let commands = commands.lock().unwrap().clone();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].0, "EVAL");
        assert_eq!(commands[1].0, "ZREM");
        assert_eq!(commands[0].1, commands[1].1, "the withdrawal removes the member the claim added");
    }

    #[tokio::test]
    async fn fallback_splits_the_limit_across_live_pods_per_scope() {
        let (url, _, _) = fake_redis(Some(Duration::ZERO), 0).await;
        let first_api_pod = connected(limiter(Some(url.clone()))).await;
        let second_api_pod = connected(limiter(Some(url.clone()))).await;
        let batch_pod = connected(RealtimeInflightLimiter::from_parts(
            "batch",
            true,
            RealtimeInflightLimiter::shared_redis(Some(&url)).unwrap(),
        ))
        .await;

        drop(first_api_pod.try_acquire("acct", "model", 6).await.expect("first api warmup"));
        drop(second_api_pod.try_acquire("acct", "model", 6).await.expect("second api warmup"));
        drop(
            batch_pod
                .try_acquire(onwards::inflight::BATCH_INFLIGHT_SCOPE, "model", 6)
                .await
                .expect("batch warmup"),
        );

        let first_redis = first_api_pod.redis.as_ref().unwrap();
        let batch_redis = batch_pod.redis.as_ref().unwrap();
        let _ = first_redis.commands.send(RedisCommand::HeartbeatNow("realtime"));
        for _ in 0..500 {
            if first_redis.live_pods("realtime") == 2 && batch_redis.live_pods("batch") == 1 {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(first_redis.live_pods("realtime"), 2);
        assert_eq!(batch_redis.live_pods("batch"), 1);

        first_redis.state.connection.store(None);
        let mut held = Vec::new();
        for _ in 0..3 {
            held.push(first_api_pod.try_acquire("acct", "model", 6).await.expect("within pod share"));
        }
        assert!(first_api_pod.try_acquire("acct", "model", 6).await.is_none());
    }

    #[tokio::test]
    async fn fallback_counts_slots_already_admitted_through_redis() {
        let (url, _, _) = fake_redis(Some(Duration::ZERO), 0).await;
        let first_pod = connected(limiter(Some(url.clone()))).await;
        let second_pod = connected(limiter(Some(url))).await;

        let redis_slot_one = first_pod.try_acquire("acct", "model", 6).await.expect("first redis slot");
        let _redis_slot_two = first_pod.try_acquire("acct", "model", 6).await.expect("second redis slot");
        drop(second_pod.try_acquire("acct", "model", 6).await.expect("second pod checkin"));

        let first_redis = first_pod.redis.as_ref().unwrap();
        let saved_connection = first_redis.state.connection.load_full();
        let _ = first_redis.commands.send(RedisCommand::HeartbeatNow("realtime"));
        for _ in 0..500 {
            if first_redis.live_pods("realtime") == 2 {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(first_redis.live_pods("realtime"), 2);

        first_redis.state.connection.store(None);
        let _fallback_slot = first_pod
            .try_acquire("acct", "model", 6)
            .await
            .expect("third slot reaches pod share of 3");
        assert!(first_pod.try_acquire("acct", "model", 6).await.is_none());

        drop(redis_slot_one);
        let _replacement_slot = first_pod
            .try_acquire("acct", "model", 6)
            .await
            .expect("dropping a redis slot frees pod capacity");
        first_redis.state.connection.store(saved_connection);
    }

    #[tokio::test]
    async fn fallback_slots_sync_to_redis_on_reconnect_and_release_on_drop() {
        let (url, _, fake_state) = fake_redis_with_state(Some(Duration::ZERO), 1).await;
        let limiter = limiter(Some(url));
        let held_during_outage = limiter
            .try_acquire("acct", "model", 4)
            .await
            .expect("admitted locally while disconnected");
        assert!(fake_state.synced.lock().unwrap().is_empty());

        let limiter = connected(limiter).await;
        for _ in 0..500 {
            if !fake_state.synced.lock().unwrap().is_empty() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        let synced = fake_state.synced.lock().unwrap().clone();
        assert_eq!(synced.len(), 1);
        assert_eq!(synced[0].0, inflight_key("model", "acct"));
        let synced_member = synced[0].1.clone();

        drop(held_during_outage);
        for _ in 0..500 {
            if fake_state
                .commands
                .lock()
                .unwrap()
                .iter()
                .any(|(name, member)| name == "ZREM" && *member == synced_member)
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert!(
            fake_state
                .commands
                .lock()
                .unwrap()
                .iter()
                .any(|(name, member)| name == "ZREM" && *member == synced_member)
        );
        drop(limiter);
    }

    #[test]
    fn claims_complete_when_caller_runtime_workers_block() {
        let server_runtime = RuntimeBuilder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let (url, _, commands) = server_runtime.block_on(fake_redis(Some(Duration::ZERO), 0));

        let caller_runtime = RuntimeBuilder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        caller_runtime.block_on(async {
            let limiter = Arc::new(connected(limiter(Some(url))).await);
            let mut claim = limiter.try_acquire("acct", "model", 10);
            assert!(claim.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());

            let barrier = Arc::new(Barrier::new(3));
            let worker_one = tokio::spawn({
                let barrier = Arc::clone(&barrier);
                async move {
                    barrier.wait();
                    thread::sleep(REDIS_TIMEOUT * 2);
                }
            });
            let worker_two = tokio::spawn({
                let barrier = Arc::clone(&barrier);
                async move {
                    barrier.wait();
                    thread::sleep(REDIS_TIMEOUT * 2);
                }
            });
            barrier.wait();
            thread::sleep(REDIS_TIMEOUT * 2);
            worker_one.await.unwrap();
            worker_two.await.unwrap();

            let slot = claim.await.expect("admitted on dedicated thread");
            assert_eq!(
                limiter.redis.as_ref().unwrap().state.consecutive_timeouts.load(Ordering::Relaxed),
                0
            );
            assert_eq!(commands.lock().unwrap().len(), 1);
            assert_eq!(commands.lock().unwrap()[0].0, "EVAL");

            drop(slot);
            for _ in 0..500 {
                if commands.lock().unwrap().len() == 2 {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(commands.lock().unwrap()[1].0, "ZREM");
        });
    }

    #[tokio::test]
    #[ignore = "requires a Redis server at INFLIGHT_TEST_REDIS_URL"]
    async fn pods_sharing_redis_share_one_count() {
        let url = redis_url();
        let account = Uuid::new_v4().to_string();
        let key = inflight_key("model", &account);
        let first_pod = connected(limiter(Some(url.clone()))).await;
        let second_pod = connected(limiter(Some(url.clone()))).await;

        let held = first_pod.try_acquire(&account, "model", 2).await.expect("first slot");
        let _also_held = second_pod.try_acquire(&account, "model", 2).await.expect("second slot");
        assert_eq!(in_flight(&url, &key).await, 2);
        assert!(first_pod.try_acquire(&account, "model", 2).await.is_none());
        assert!(second_pod.try_acquire(&account, "model", 2).await.is_none());

        drop(held);
        let mut released = false;
        for _ in 0..100 {
            if in_flight(&url, &key).await == 1 {
                released = true;
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        assert!(released, "dropping a slot removes it from Redis");
        assert!(second_pod.try_acquire(&account, "model", 2).await.is_some());
    }

    #[tokio::test]
    #[ignore = "requires a Redis server at INFLIGHT_TEST_REDIS_URL"]
    async fn a_burst_on_one_pod_is_checked_against_the_shared_count() {
        let url = redis_url();
        let account = Uuid::new_v4().to_string();
        let key = inflight_key("model", &account);
        let limiter = Arc::new(connected(limiter(Some(url.clone()))).await);

        let attempts = (0..500).map(|_| {
            let limiter = Arc::clone(&limiter);
            let account = account.clone();
            tokio::spawn(async move { limiter.try_acquire(&account, "model", 100).await })
        });
        let slots: Vec<_> = futures::future::join_all(attempts)
            .await
            .into_iter()
            .filter_map(|attempt| attempt.unwrap())
            .collect();

        assert_eq!(slots.len(), 100);
        assert_eq!(in_flight(&url, &key).await, 100);
    }

    #[tokio::test]
    #[ignore = "requires a Redis server at INFLIGHT_TEST_REDIS_URL"]
    async fn an_expired_lease_no_longer_counts() {
        let url = redis_url();
        let account = Uuid::new_v4().to_string();
        let key = inflight_key("model", &account);
        let limiter = connected(limiter(Some(url.clone()))).await;
        redis::cmd("ZADD")
            .arg(&key)
            .arg(1)
            .arg("abandoned-by-a-dead-pod")
            .query_async::<i64>(&mut test_connection(&url).await)
            .await
            .unwrap();
        assert_eq!(in_flight(&url, &key).await, 1);
        let held = limiter.try_acquire(&account, "model", 1).await;
        assert!(held.is_some());
        assert_eq!(in_flight(&url, &key).await, 1);
    }

    #[tokio::test]
    #[ignore = "requires a Redis server at INFLIGHT_TEST_REDIS_URL"]
    async fn batch_and_realtime_counts_do_not_share_a_bucket() {
        let url = redis_url();
        let account = Uuid::new_v4().to_string();
        let realtime = connected(limiter(Some(url.clone()))).await;
        let batch = connected(RealtimeInflightLimiter::from_parts(
            "batch",
            true,
            RealtimeInflightLimiter::shared_redis(Some(&url)).unwrap(),
        ))
        .await;

        let _realtime_slot = realtime.try_acquire(&account, "model", 1).await.expect("realtime slot");
        let batch_slot = batch
            .try_acquire(onwards::inflight::BATCH_INFLIGHT_SCOPE, "model", 1)
            .await
            .expect("batch slot");

        assert_eq!(in_flight(&url, &inflight_key("model", &account)).await, 1);
        assert_eq!(
            in_flight(&url, &inflight_key("model", onwards::inflight::BATCH_INFLIGHT_SCOPE)).await,
            1
        );
        assert!(
            batch
                .try_acquire(onwards::inflight::BATCH_INFLIGHT_SCOPE, "model", 1)
                .await
                .is_none()
        );
        assert!(realtime.try_acquire(&account, "model", 1).await.is_none());
        drop(batch_slot);
    }
}
