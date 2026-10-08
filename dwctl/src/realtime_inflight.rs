use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use anyhow::Context;
use arc_swap::ArcSwapOption;
use futures::future::BoxFuture;
use onwards::inflight::{InflightLimiter, InflightSlot, LocalInflightLimiter};
use redis::aio::{ConnectionManager, ConnectionManagerConfig};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::config::RealtimeInflightLimitsConfig;

const LEASE: Duration = Duration::from_secs(90);
const RENEW_EVERY: Duration = Duration::from_secs(30);
const REDIS_TIMEOUT: Duration = Duration::from_millis(250);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(1);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const RECONNECT_AFTER_TIMEOUTS: u32 = 3;
const RECONNECT_BACKOFF: Duration = Duration::from_secs(1);

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

const RENEW_SCRIPT: &str = r"
local now = redis.call('TIME')
local now_ms = tonumber(now[1]) * 1000 + math.floor(tonumber(now[2]) / 1000)
redis.call('ZADD', KEYS[1], 'XX', now_ms + tonumber(ARGV[2]), ARGV[1])
redis.call('PEXPIRE', KEYS[1], tonumber(ARGV[2]))
return 1
";

pub struct RealtimeInflightLimiter {
    /// Which limit class this limiter serves, as a stable metric/log label:
    /// `"realtime"` for per-account realtime limits, `"batch"` for the global
    /// per-model batch cap. Both share the type and Redis but are constructed
    /// separately.
    scope: &'static str,
    enforce: bool,
    redis: Option<Arc<SharedRedis>>,
    local: LocalInflightLimiter,
    exempt_account: String,
}

pub struct SharedRedis {
    client: redis::Client,
    connection: ArcSwapOption<ConnectionManager>,
    consecutive_timeouts: AtomicU32,
    connecting: AtomicBool,
}

impl SharedRedis {
    fn connect(url: &str) -> anyhow::Result<Arc<Self>> {
        let redis = Arc::new(Self {
            client: redis::Client::open(url)?,
            connection: ArcSwapOption::empty(),
            consecutive_timeouts: AtomicU32::new(0),
            connecting: AtomicBool::new(false),
        });
        redis.reconnect();
        Ok(redis)
    }

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

    fn timed_out(self: &Arc<Self>) {
        if self.consecutive_timeouts.fetch_add(1, Ordering::Relaxed) + 1 >= RECONNECT_AFTER_TIMEOUTS && self.reconnect() {
            metrics::counter!("dwctl_realtime_inflight_redis_reconnects_total").increment(1);
        }
    }

    fn reconnect(self: &Arc<Self>) -> bool {
        if self.connecting.swap(true, Ordering::AcqRel) {
            return false;
        }
        self.connection.store(None);
        self.consecutive_timeouts.store(0, Ordering::Relaxed);
        let redis = Arc::clone(self);
        tokio::spawn(async move {
            let config = ConnectionManagerConfig::new()
                .set_connection_timeout(CONNECT_TIMEOUT)
                .set_response_timeout(RESPONSE_TIMEOUT)
                .set_number_of_retries(0);
            loop {
                match ConnectionManager::new_with_config(redis.client.clone(), config.clone()).await {
                    Ok(connection) => {
                        redis.connection.store(Some(Arc::new(connection)));
                        break;
                    }
                    Err(error) => {
                        crate::background_error!(
                            crate::metrics::errors::component::REALTIME_INFLIGHT,
                            "redis_connect",
                            Warning,
                            error = %error,
                            "Failed to connect to the in-flight Redis; counting in this pod until it connects"
                        );
                        tokio::time::sleep(RECONNECT_BACKOFF).await;
                    }
                }
            }
            redis.connecting.store(false, Ordering::Release);
        });
        true
    }
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

    /// Open the shared Redis connection once so the realtime and batch limiters
    /// use the same one. `None` when no URL is configured, in which case each
    /// limiter counts in this pod.
    pub(crate) fn shared_redis(redis_url: Option<&str>) -> anyhow::Result<Option<Arc<SharedRedis>>> {
        redis_url.map(SharedRedis::connect).transpose()
    }

    /// Build a limiter from an explicit scope, switch and shared Redis connection.
    /// The batch in-flight cap reuses this type and the realtime Redis, but has
    /// its own scope and counts under a reserved key space.
    pub fn from_parts(scope: &'static str, enforce: bool, redis: Option<Arc<SharedRedis>>) -> Self {
        Self {
            scope,
            enforce,
            redis,
            local: LocalInflightLimiter::default(),
            exempt_account: Uuid::nil().to_string(),
        }
    }

    fn acquire_locally(&self, account: &str, model: &str, limit: u32) -> Option<InflightSlot> {
        self.local.acquire(account, model, limit)
    }
}

/// Key for a shared in-flight count. Realtime scopes by account id; batch uses
/// the reserved [`onwards::inflight::BATCH_INFLIGHT_SCOPE`] account, so batch
/// keys and realtime keys can never collide.
fn inflight_key(model: &str, account: &str) -> String {
    format!("dwctl:inflight:{model}:{account}")
}

impl InflightLimiter for RealtimeInflightLimiter {
    fn try_acquire<'a>(&'a self, account: &'a str, model: &'a str, limit: u32) -> BoxFuture<'a, Option<InflightSlot>> {
        Box::pin(async move {
            if !self.enforce || account == self.exempt_account {
                return Some(InflightSlot::new(()));
            }
            let Some(redis) = &self.redis else {
                return self.acquire_locally(account, model, limit);
            };
            let Some(current) = redis.current() else {
                tracing::warn!(scope = self.scope, "In-flight Redis is not connected; counting in this pod instead");
                metrics::counter!("dwctl_realtime_inflight_redis_fallbacks_total", "scope" => self.scope).increment(1);
                return self.acquire_locally(account, model, limit);
            };
            let mut connection = ConnectionManager::clone(&current);
            let key = inflight_key(model, account);
            let member = Uuid::new_v4().to_string();
            match tokio::time::timeout(REDIS_TIMEOUT, claim(&mut connection, &key, &member, limit)).await {
                Ok(Ok(true)) => {
                    redis.answered();
                    Some(InflightSlot::new(RedisSlot::hold(self.scope, Arc::clone(redis), key, member)))
                }
                Ok(Ok(false)) => {
                    redis.answered();
                    None
                }
                Ok(Err(error)) => {
                    tracing::warn!(scope = self.scope, error = %error, "In-flight claim failed; counting in this pod instead");
                    metrics::counter!("dwctl_realtime_inflight_redis_fallbacks_total", "scope" => self.scope).increment(1);
                    self.acquire_locally(account, model, limit)
                }
                Err(_) => {
                    redis.timed_out();
                    if redis.is_current(&current) {
                        let scope = self.scope;
                        tokio::spawn(async move {
                            if let Err(error) = remove(&mut connection, &key, &member).await {
                                crate::background_error!(
                                    crate::metrics::errors::component::REALTIME_INFLIGHT,
                                    "claim_withdraw",
                                    Warning,
                                    scope = scope,
                                    error = %error,
                                    "Failed to withdraw a timed-out in-flight claim; its lease will expire"
                                );
                            }
                        });
                    }
                    tracing::warn!(scope = self.scope, "In-flight claim timed out; counting in this pod instead");
                    metrics::counter!("dwctl_realtime_inflight_redis_fallbacks_total", "scope" => self.scope).increment(1);
                    self.acquire_locally(account, model, limit)
                }
            }
        })
    }
}

async fn claim(connection: &mut ConnectionManager, key: &str, member: &str, limit: u32) -> anyhow::Result<bool> {
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

async fn renew(redis: &SharedRedis, key: &str, member: &str) -> anyhow::Result<()> {
    let mut connection = redis.connection().context("in-flight Redis is not connected")?;
    redis::cmd("EVAL")
        .arg(RENEW_SCRIPT)
        .arg(1)
        .arg(key)
        .arg(member)
        .arg(LEASE.as_millis() as u64)
        .query_async::<i64>(&mut connection)
        .await?;
    Ok(())
}

async fn release(redis: &SharedRedis, key: &str, member: &str) -> anyhow::Result<()> {
    let mut connection = redis.connection().context("in-flight Redis is not connected")?;
    remove(&mut connection, key, member).await
}

async fn remove(connection: &mut ConnectionManager, key: &str, member: &str) -> anyhow::Result<()> {
    redis::cmd("ZREM").arg(key).arg(member).query_async::<i64>(connection).await?;
    Ok(())
}

struct RedisSlot {
    scope: &'static str,
    redis: Arc<SharedRedis>,
    key: String,
    member: String,
    renewal: JoinHandle<()>,
}

impl RedisSlot {
    fn hold(scope: &'static str, redis: Arc<SharedRedis>, key: String, member: String) -> Self {
        let renewal = tokio::spawn({
            let redis = Arc::clone(&redis);
            let key = key.clone();
            let member = member.clone();
            async move {
                let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + RENEW_EVERY, RENEW_EVERY);
                loop {
                    ticks.tick().await;
                    if let Err(error) = renew(&redis, &key, &member).await {
                        crate::background_error!(
                            crate::metrics::errors::component::REALTIME_INFLIGHT,
                            "lease_renew",
                            Warning,
                            scope = scope,
                            error = %error,
                            "Failed to renew an in-flight lease"
                        );
                    }
                }
            }
        });
        Self {
            scope,
            redis,
            key,
            member,
            renewal,
        }
    }
}

impl Drop for RedisSlot {
    fn drop(&mut self) {
        self.renewal.abort();
        let scope = self.scope;
        let redis = Arc::clone(&self.redis);
        let key = std::mem::take(&mut self.key);
        let member = std::mem::take(&mut self.member);
        tokio::spawn(async move {
            if let Err(error) = release(&redis, &key, &member).await {
                crate::background_error!(
                    crate::metrics::errors::component::REALTIME_INFLIGHT,
                    "lease_release",
                    Warning,
                    scope = scope,
                    error = %error,
                    "Failed to release an in-flight slot; its lease will expire"
                );
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

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
            if redis.connection().is_some() {
                return limiter;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
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

    async fn serve(mut socket: TcpStream, claim_reply_after: Option<Duration>, commands: Commands) {
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
                let reply: &[u8] = match name.as_str() {
                    "EVAL" => {
                        commands
                            .lock()
                            .unwrap()
                            .push((name, String::from_utf8_lossy(&args[5]).into_owned()));
                        match claim_reply_after {
                            Some(delay) => {
                                tokio::time::sleep(delay).await;
                                b":1\r\n"
                            }
                            None => {
                                silent = true;
                                b""
                            }
                        }
                    }
                    "ZREM" => {
                        commands
                            .lock()
                            .unwrap()
                            .push((name, String::from_utf8_lossy(&args[2]).into_owned()));
                        b":1\r\n"
                    }
                    _ => b"+OK\r\n",
                };
                if !silent && socket.write_all(reply).await.is_err() {
                    return;
                }
            }
        }
    }

    async fn fake_redis(claim_reply_after: Option<Duration>, refuse_first: usize) -> (String, Arc<AtomicUsize>, Commands) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("redis://{}", listener.local_addr().unwrap());
        let accepted = Arc::new(AtomicUsize::new(0));
        let commands = Commands::default();
        tokio::spawn({
            let accepted = Arc::clone(&accepted);
            let commands = Arc::clone(&commands);
            async move {
                while let Ok((socket, _)) = listener.accept().await {
                    if accepted.fetch_add(1, Ordering::SeqCst) >= refuse_first {
                        tokio::spawn(serve(socket, claim_reply_after, Arc::clone(&commands)));
                    }
                }
            }
        });
        (url, accepted, commands)
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
        let limiter = limiter(Some(url));
        assert!(limiter.redis.as_ref().unwrap().connection().is_none());
        connected(limiter).await;
        assert_eq!(accepted.load(Ordering::SeqCst), 3);
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
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
        connected(limiter).await;
        let withdrawals = || commands.lock().unwrap().iter().filter(|(name, _)| name == "ZREM").count() as u32;
        for _ in 0..500 {
            if withdrawals() >= RECONNECT_AFTER_TIMEOUTS - 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
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
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let commands = commands.lock().unwrap().clone();
        assert_eq!(commands.len(), 2);
        assert_eq!(commands[0].0, "EVAL");
        assert_eq!(commands[1].0, "ZREM");
        assert_eq!(commands[0].1, commands[1].1, "the withdrawal removes the member the claim added");
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
            tokio::time::sleep(Duration::from_millis(10)).await;
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

        // Both caps are 1, yet one realtime request and one batch request can
        // be in flight at once: they occupy different Redis keys.
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
        // ...and the realtime scope is still held independently.
        assert!(realtime.try_acquire(&account, "model", 1).await.is_none());
        drop(batch_slot);
    }
}
