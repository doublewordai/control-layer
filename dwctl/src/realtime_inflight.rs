use std::fmt;
use std::time::Duration;

use deadpool_redis::{Config as RedisConfig, Pool, Runtime, redis};
use futures::future::BoxFuture;
use onwards::inflight::{InflightLimiter, InflightSlot, LocalInflightLimiter};
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::config::RealtimeInflightLimitsConfig;

const LEASE: Duration = Duration::from_secs(90);
const RENEW_EVERY: Duration = Duration::from_secs(30);
const REDIS_TIMEOUT: Duration = Duration::from_millis(250);

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
    enforce: bool,
    redis: Option<Pool>,
    local: LocalInflightLimiter,
    exempt_account: String,
}

impl fmt::Debug for RealtimeInflightLimiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RealtimeInflightLimiter")
            .field("enforce", &self.enforce)
            .field("redis", &self.redis.is_some())
            .finish()
    }
}

impl RealtimeInflightLimiter {
    pub fn from_config(config: &RealtimeInflightLimitsConfig) -> anyhow::Result<Self> {
        let redis = config
            .redis_url
            .as_ref()
            .map(|url| RedisConfig::from_url(url.clone()).create_pool(Some(Runtime::Tokio1)))
            .transpose()?;
        Ok(Self {
            enforce: config.enforce,
            redis,
            local: LocalInflightLimiter::default(),
            exempt_account: Uuid::nil().to_string(),
        })
    }

    fn acquire_locally(&self, account: &str, model: &str, limit: u32) -> Option<InflightSlot> {
        self.local.acquire(account, model, limit)
    }
}

impl InflightLimiter for RealtimeInflightLimiter {
    fn try_acquire<'a>(&'a self, account: &'a str, model: &'a str, limit: u32) -> BoxFuture<'a, Option<InflightSlot>> {
        Box::pin(async move {
            if !self.enforce || account == self.exempt_account {
                return Some(InflightSlot::new(()));
            }
            let Some(pool) = &self.redis else {
                return self.acquire_locally(account, model, limit);
            };
            let key = format!("dwctl:inflight:{model}:{account}");
            let member = Uuid::new_v4().to_string();
            match tokio::time::timeout(REDIS_TIMEOUT, claim(pool, &key, &member, limit)).await {
                Ok(Ok(true)) => Some(InflightSlot::new(RedisSlot::hold(pool.clone(), key, member))),
                Ok(Ok(false)) => None,
                Ok(Err(error)) => {
                    tracing::warn!(error = %error, "Realtime in-flight claim failed; counting in this pod instead");
                    metrics::counter!("dwctl_realtime_inflight_redis_fallbacks_total").increment(1);
                    self.acquire_locally(account, model, limit)
                }
                Err(_) => {
                    tracing::warn!("Realtime in-flight claim timed out; counting in this pod instead");
                    metrics::counter!("dwctl_realtime_inflight_redis_fallbacks_total").increment(1);
                    self.acquire_locally(account, model, limit)
                }
            }
        })
    }
}

async fn claim(pool: &Pool, key: &str, member: &str, limit: u32) -> anyhow::Result<bool> {
    let mut conn = pool.get().await?;
    let admitted: i64 = redis::cmd("EVAL")
        .arg(CLAIM_SCRIPT)
        .arg(1)
        .arg(key)
        .arg(limit)
        .arg(member)
        .arg(LEASE.as_millis() as u64)
        .query_async(&mut conn)
        .await?;
    Ok(admitted == 1)
}

async fn renew(pool: &Pool, key: &str, member: &str) -> anyhow::Result<()> {
    let mut conn = pool.get().await?;
    redis::cmd("EVAL")
        .arg(RENEW_SCRIPT)
        .arg(1)
        .arg(key)
        .arg(member)
        .arg(LEASE.as_millis() as u64)
        .query_async::<i64>(&mut conn)
        .await?;
    Ok(())
}

async fn release(pool: &Pool, key: &str, member: &str) -> anyhow::Result<()> {
    let mut conn = pool.get().await?;
    redis::cmd("ZREM").arg(key).arg(member).query_async::<i64>(&mut conn).await?;
    Ok(())
}

struct RedisSlot {
    pool: Pool,
    key: String,
    member: String,
    renewal: JoinHandle<()>,
}

impl RedisSlot {
    fn hold(pool: Pool, key: String, member: String) -> Self {
        let renewal = tokio::spawn({
            let pool = pool.clone();
            let key = key.clone();
            let member = member.clone();
            async move {
                let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + RENEW_EVERY, RENEW_EVERY);
                loop {
                    ticks.tick().await;
                    if let Err(error) = renew(&pool, &key, &member).await {
                        crate::background_error!(
                            crate::metrics::errors::component::REALTIME_INFLIGHT,
                            "lease_renew",
                            Warning,
                            error = %error,
                            "Failed to renew a realtime in-flight lease"
                        );
                    }
                }
            }
        });
        Self {
            pool,
            key,
            member,
            renewal,
        }
    }
}

impl Drop for RedisSlot {
    fn drop(&mut self) {
        self.renewal.abort();
        let pool = self.pool.clone();
        let key = std::mem::take(&mut self.key);
        let member = std::mem::take(&mut self.member);
        tokio::spawn(async move {
            if let Err(error) = release(&pool, &key, &member).await {
                crate::background_error!(
                    crate::metrics::errors::component::REALTIME_INFLIGHT,
                    "lease_release",
                    Warning,
                    error = %error,
                    "Failed to release a realtime in-flight slot; its lease will expire"
                );
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redis_url() -> String {
        std::env::var("INFLIGHT_TEST_REDIS_URL").expect("INFLIGHT_TEST_REDIS_URL names a Redis server")
    }

    fn limiter(redis_url: Option<String>) -> RealtimeInflightLimiter {
        RealtimeInflightLimiter::from_config(&RealtimeInflightLimitsConfig { enforce: true, redis_url }).unwrap()
    }

    async fn in_flight(pool: &Pool, key: &str) -> i64 {
        let mut conn = pool.get().await.unwrap();
        redis::cmd("ZCARD").arg(key).query_async(&mut conn).await.unwrap()
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
    #[ignore = "requires a Redis server at INFLIGHT_TEST_REDIS_URL"]
    async fn pods_sharing_redis_share_one_count() {
        let url = redis_url();
        let account = Uuid::new_v4().to_string();
        let key = format!("dwctl:inflight:model:{account}");
        let first_pod = limiter(Some(url.clone()));
        let second_pod = limiter(Some(url));
        let pool = first_pod.redis.clone().unwrap();

        let held = first_pod.try_acquire(&account, "model", 2).await.expect("first slot");
        let _also_held = second_pod.try_acquire(&account, "model", 2).await.expect("second slot");
        assert_eq!(in_flight(&pool, &key).await, 2);
        assert!(first_pod.try_acquire(&account, "model", 2).await.is_none());
        assert!(second_pod.try_acquire(&account, "model", 2).await.is_none());

        drop(held);
        let mut released = false;
        for _ in 0..100 {
            if in_flight(&pool, &key).await == 1 {
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
    async fn an_expired_lease_no_longer_counts() {
        let url = redis_url();
        let account = Uuid::new_v4().to_string();
        let key = format!("dwctl:inflight:model:{account}");
        let limiter = limiter(Some(url));
        let pool = limiter.redis.clone().unwrap();
        {
            let mut conn = pool.get().await.unwrap();
            redis::cmd("ZADD")
                .arg(&key)
                .arg(1)
                .arg("abandoned-by-a-dead-pod")
                .query_async::<i64>(&mut conn)
                .await
                .unwrap();
        }
        assert_eq!(in_flight(&pool, &key).await, 1);
        let held = limiter.try_acquire(&account, "model", 1).await;
        assert!(held.is_some());
        assert_eq!(in_flight(&pool, &key).await, 1);
    }
}
