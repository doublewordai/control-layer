use std::any::Any;
use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use dashmap::DashMap;
use futures_util::future::BoxFuture;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InflightLimits {
    pub default: u32,
    pub accounts: HashMap<String, u32>,
}

impl InflightLimits {
    pub fn for_account(&self, account: &str) -> u32 {
        self.accounts.get(account).copied().unwrap_or(self.default)
    }
}

pub struct InflightSlot {
    _release_on_drop: Box<dyn Any + Send + Sync>,
}

impl InflightSlot {
    pub fn new(release_on_drop: impl Any + Send + Sync) -> Self {
        Self {
            _release_on_drop: Box::new(release_on_drop),
        }
    }
}

impl Debug for InflightSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InflightSlot")
    }
}

pub trait InflightLimiter: Debug + Send + Sync {
    fn try_acquire<'a>(
        &'a self,
        account: &'a str,
        model: &'a str,
        limit: u32,
    ) -> BoxFuture<'a, Option<InflightSlot>>;
}

#[derive(Debug, Default)]
pub struct LocalInflightLimiter {
    counts: DashMap<(String, String), Arc<AtomicU32>>,
}

struct LocalSlot(Arc<AtomicU32>);

impl Drop for LocalSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl LocalInflightLimiter {
    pub fn acquire(&self, account: &str, model: &str, limit: u32) -> Option<InflightSlot> {
        let count = self
            .counts
            .entry((account.to_owned(), model.to_owned()))
            .or_default()
            .clone();
        count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < limit).then_some(current + 1)
            })
            .ok()?;
        Some(InflightSlot::new(LocalSlot(count)))
    }

    pub fn in_flight(&self, account: &str, model: &str) -> u32 {
        self.counts
            .get(&(account.to_owned(), model.to_owned()))
            .map(|count| count.load(Ordering::Acquire))
            .unwrap_or(0)
    }
}

impl InflightLimiter for LocalInflightLimiter {
    fn try_acquire<'a>(
        &'a self,
        account: &'a str,
        model: &'a str,
        limit: u32,
    ) -> BoxFuture<'a, Option<InflightSlot>> {
        Box::pin(std::future::ready(self.acquire(account, model, limit)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_limiter_admits_up_to_the_limit_and_frees_on_drop() {
        let limiter = LocalInflightLimiter::default();
        let first = limiter.acquire("acct", "model-a", 2);
        let second = limiter.acquire("acct", "model-a", 2);
        assert!(first.is_some());
        assert!(second.is_some());
        assert!(limiter.acquire("acct", "model-a", 2).is_none());
        assert_eq!(limiter.in_flight("acct", "model-a"), 2);

        drop(first);
        assert_eq!(limiter.in_flight("acct", "model-a"), 1);
        assert!(limiter.acquire("acct", "model-a", 2).is_some());
    }

    #[test]
    fn local_limiter_counts_each_account_and_model_separately() {
        let limiter = LocalInflightLimiter::default();
        let _held = limiter.acquire("acct", "model-a", 1);
        assert!(limiter.acquire("acct", "model-a", 1).is_none());
        assert!(limiter.acquire("acct", "model-b", 1).is_some());
        assert!(limiter.acquire("other", "model-a", 1).is_some());
    }

    #[test]
    fn account_override_replaces_the_default() {
        let limits = InflightLimits {
            default: 10,
            accounts: HashMap::from([("big".to_string(), 50)]),
        };
        assert_eq!(limits.for_account("big"), 50);
        assert_eq!(limits.for_account("small"), 10);
    }
}
