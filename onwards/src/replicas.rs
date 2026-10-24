//! Replica affinity: send each conversation to the same replica of a provider.
//!
//! Some upstreams are a set of interchangeable replicas behind one Service
//! address, each holding a per-process cache that only helps when a
//! conversation's turns return to it (per-conversation state kept in memory,
//! for example). Behind a plain Service, every new connection lands on a random
//! replica, so a conversation's turns are spread over all of them and each
//! replica sees a growing prefix it has never seen.
//!
//! A provider may list its replicas' direct addresses under `replicas`. For a
//! request with a conversation key (see [`crate::affinity`]) the replicas are
//! ranked by rendezvous hashing of the key against each replica's address, and
//! the request goes to the highest-ranked replica that is not overloaded:
//!
//! - **Stable and minimal**: the ranking depends only on the key and the
//!   replica addresses, so every gateway replica picks the same replica for a
//!   conversation without coordination, and adding or removing a replica moves
//!   only the conversations that rank it first.
//! - **Bounded load**: a replica is skipped while its in-flight requests from
//!   this process reach `load_factor` times the mean over the set
//!   (consistent hashing with bounded loads). A conversation, or a group of
//!   conversations that share a key, that would otherwise pile onto one
//!   replica spills to its next-ranked replica, which is again the same for
//!   every request of that key.
//! - **Unready replicas**: a replica that refuses the connection (its address
//!   no longer resolves, or nothing behind it accepts) is skipped for that
//!   request in favour of the next-ranked one. Nothing has been sent, so this
//!   is always safe. So is a status listed in `failover_on_status`, which is
//!   for statuses the replicas return before doing any work (a draining
//!   server's 503, say). The provider's own `url` is the last resort, so a
//!   request is never worse off than without replica affinity.
//!
//! Requests without a conversation key go to the provider's `url` unchanged.
//! In-flight counts are per process and per replica address, shared by every
//! provider and pool that names the same address, and they survive reloads.
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use url::Url;

/// Most replicas a provider may list.
pub const MAX_REPLICAS: usize = 64;

/// Per-provider replica affinity configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplicaAffinityConfig {
    /// Each replica's direct base URL. The path must match the provider's
    /// `url`, since requests are joined onto it the same way.
    pub urls: Vec<Url>,
    /// A replica takes a request while its in-flight count is below this many
    /// times the mean over the replicas (counting the new request). 1.0 keeps
    /// the replicas exactly level and so moves conversations most; larger
    /// values keep conversations in place under more skew.
    #[serde(default = "default_load_factor")]
    pub load_factor: f64,
    /// Response statuses at which the request moves on to the next candidate
    /// instead of returning. Only for statuses the replicas send before doing
    /// any work on the request, since the request is sent again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failover_on_status: Vec<u16>,
}

fn default_load_factor() -> f64 {
    1.25
}

impl ReplicaAffinityConfig {
    /// Check the configuration against the provider it belongs to.
    pub fn validate(&self, provider_url: &Url) -> Result<(), String> {
        if self.urls.is_empty() || self.urls.len() > MAX_REPLICAS {
            return Err(format!("replicas.urls must list 1..={MAX_REPLICAS} URLs"));
        }
        if !self.load_factor.is_finite() || !(1.0..=10.0).contains(&self.load_factor) {
            return Err("replicas.load_factor must be between 1.0 and 10.0".to_string());
        }
        if self
            .failover_on_status
            .iter()
            .any(|status| !(400..=599).contains(status))
        {
            return Err("replicas.failover_on_status must list 4xx or 5xx statuses".to_string());
        }
        let path = |url: &Url| url.path().trim_end_matches('/').to_string();
        let mut seen = HashSet::new();
        for url in &self.urls {
            if path(url) != path(provider_url) {
                return Err(format!(
                    "replica URL {url} must have the same path as the provider URL {provider_url}"
                ));
            }
            if !seen.insert(normalized(url).to_string()) {
                return Err(format!("replica URL {url} is listed twice"));
            }
        }
        Ok(())
    }
}

/// A URL with a trailing slash on its path, as targets hold theirs.
fn normalized(url: &Url) -> Url {
    let mut url = url.clone();
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    url
}

/// In-flight requests per replica address, for the whole process.
static LOADS: LazyLock<DashMap<String, Arc<AtomicUsize>>> = LazyLock::new(DashMap::new);

fn load_counter(url: &Url) -> Arc<AtomicUsize> {
    LOADS
        .entry(url.to_string())
        .or_insert_with(|| Arc::new(AtomicUsize::new(0)))
        .clone()
}

/// One replica of a provider.
#[derive(Debug)]
pub struct Replica {
    /// Base URL, with a trailing slash.
    pub url: Url,
    /// `host:port`, for metrics and traces.
    pub label: String,
    seed: u64,
    load: Arc<AtomicUsize>,
}

/// A provider's replicas, ready to route.
#[derive(Debug)]
pub struct ReplicaSet {
    replicas: Vec<Replica>,
    load_factor: f64,
    failover_on_status: Vec<u16>,
}

/// Where a request should go, in order of preference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Replica indices to try in turn; the provider's own URL follows them.
    pub order: Vec<usize>,
    /// The first replica is not the key's top-ranked one, because that one
    /// was over the load bound.
    pub spilled: bool,
}

/// Holds one in-flight slot on a replica until dropped.
#[derive(Debug)]
pub struct ReplicaGuard {
    load: Arc<AtomicUsize>,
}

impl Drop for ReplicaGuard {
    fn drop(&mut self) {
        self.load.fetch_sub(1, Ordering::AcqRel);
    }
}

impl ReplicaSet {
    pub fn new(config: &ReplicaAffinityConfig) -> Self {
        let replicas = config
            .urls
            .iter()
            .map(|url| {
                let url = normalized(url);
                let label = match (url.host_str(), url.port_or_known_default()) {
                    (Some(host), Some(port)) => format!("{host}:{port}"),
                    (Some(host), None) => host.to_string(),
                    _ => url.to_string(),
                };
                Replica {
                    seed: fnv1a(url.as_str().as_bytes()),
                    load: load_counter(&url),
                    label,
                    url,
                }
            })
            .collect();
        Self {
            replicas,
            load_factor: config.load_factor,
            failover_on_status: config.failover_on_status.clone(),
        }
    }

    /// Whether a replica's response with `status` should go to the next
    /// candidate instead of the client.
    pub fn fails_over_on(&self, status: u16) -> bool {
        self.failover_on_status.contains(&status)
    }

    pub fn replica(&self, index: usize) -> &Replica {
        &self.replicas[index]
    }

    pub fn len(&self) -> usize {
        self.replicas.len()
    }

    pub fn is_empty(&self) -> bool {
        self.replicas.is_empty()
    }

    /// The replicas ranked for `key`, best first (rendezvous hashing).
    pub fn rank(&self, key: u64) -> Vec<usize> {
        let mut ranked: Vec<(u64, usize)> = self
            .replicas
            .iter()
            .enumerate()
            .map(|(index, replica)| (mix(key ^ replica.seed), index))
            .collect();
        ranked.sort_unstable_by(|a, b| b.cmp(a));
        ranked.into_iter().map(|(_, index)| index).collect()
    }

    /// The order in which to try the replicas for `key`: the best-ranked
    /// replica under the load bound first, then the rest in rank order.
    pub fn plan(&self, key: u64) -> Plan {
        let loads: Vec<usize> = self
            .replicas
            .iter()
            .map(|r| r.load.load(Ordering::Acquire))
            .collect();
        self.plan_with_loads(key, &loads)
    }

    fn plan_with_loads(&self, key: u64, loads: &[usize]) -> Plan {
        let ranked = self.rank(key);
        let total: usize = loads.iter().sum();
        // Counting the new request, so an idle set admits it anywhere. Some
        // replica is always under the bound, because the loads cannot all be
        // above their mean.
        let bound = (self.load_factor * (total + 1) as f64 / self.replicas.len() as f64).ceil();
        let first = ranked
            .iter()
            .position(|&index| ((loads[index] + 1) as f64) <= bound)
            .unwrap_or(0);
        let mut order = Vec::with_capacity(ranked.len());
        order.push(ranked[first]);
        order.extend(
            ranked
                .iter()
                .enumerate()
                .filter(|&(position, _)| position != first)
                .map(|(_, &index)| index),
        );
        Plan {
            order,
            spilled: first != 0,
        }
    }

    /// Count a request against a replica until the guard drops.
    pub fn acquire(&self, index: usize) -> ReplicaGuard {
        let load = self.replicas[index].load.clone();
        load.fetch_add(1, Ordering::AcqRel);
        ReplicaGuard { load }
    }
}

/// FNV-1a: a stable 64-bit hash of a replica address.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// SplitMix64's finaliser: spreads `key ^ seed` over all 64 bits, so the
/// ranking for a key is independent across replicas.
fn mix(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(n: usize, load_factor: f64) -> ReplicaAffinityConfig {
        ReplicaAffinityConfig {
            urls: (0..n)
                .map(|i| {
                    format!("http://replica-{i}.test.invalid:8000/v1")
                        .parse()
                        .unwrap()
                })
                .collect(),
            load_factor,
            failover_on_status: Vec::new(),
        }
    }

    fn keys(n: u64) -> impl Iterator<Item = u64> {
        (0..n).map(|i| mix(i.wrapping_add(0x1234_5678)))
    }

    #[test]
    fn removing_a_replica_moves_only_its_conversations() {
        let four = ReplicaSet::new(&config(4, 1.25));
        let mut three_config = config(4, 1.25);
        three_config.urls.remove(2);
        let three = ReplicaSet::new(&three_config);
        let label = |set: &ReplicaSet, key| set.replica(set.rank(key)[0]).label.clone();
        let mut moved = 0;
        for key in keys(10_000) {
            let before = label(&four, key);
            let after = label(&three, key);
            if before != "replica-2.test.invalid:8000" {
                assert_eq!(
                    before, after,
                    "a conversation moved off a replica that stayed"
                );
            } else {
                moved += 1;
            }
        }
        // About a quarter of the conversations ranked the removed replica first.
        assert!((2_000..3_000).contains(&moved), "moved {moved}");
    }

    #[test]
    fn idle_replicas_share_conversations_evenly() {
        let set = ReplicaSet::new(&config(4, 1.25));
        let mut counts = [0usize; 4];
        for key in keys(40_000) {
            counts[set.plan_with_loads(key, &[0, 0, 0, 0]).order[0]] += 1;
        }
        for count in counts {
            assert!((9_000..11_000).contains(&count), "{counts:?}");
        }
    }

    #[test]
    fn a_hot_key_spills_once_its_replica_reaches_the_bound() {
        let set = ReplicaSet::new(&config(4, 1.25));
        let key = 42;
        let ranked = set.rank(key);
        let mut loads = vec![0usize; 4];
        let mut served = vec![0usize; 4];
        for _ in 0..400 {
            let plan = set.plan_with_loads(key, &loads);
            loads[plan.order[0]] += 1;
            served[plan.order[0]] += 1;
        }
        // No replica ends above the bound for the final total.
        let bound = (1.25 * 400.0 / 4.0_f64).ceil() as usize;
        assert!(served.iter().all(|&s| s <= bound), "{served:?}");
        // The overflow goes to the key's next-ranked replica first, not at random.
        assert!(served[ranked[0]] >= served[ranked[1]], "{served:?}");
        assert!(served[ranked[1]] >= served[ranked[2]], "{served:?}");
    }

    #[test]
    fn under_the_bound_the_top_ranked_replica_is_kept() {
        let set = ReplicaSet::new(&config(3, 1.25));
        let key = 7;
        let ranked = set.rank(key);
        let mut loads = vec![10usize; 3];
        loads[ranked[0]] = 11;
        let plan = set.plan_with_loads(key, &loads);
        assert_eq!(plan.order, ranked);
        assert!(!plan.spilled);
    }

    #[test]
    fn the_ranking_is_stable_across_processes() {
        // Gateway replicas must agree, so the ranking may depend only on the
        // key and the addresses: pin one result.
        let set = ReplicaSet::new(&config(3, 1.25));
        let first = set.rank(0xdead_beef)[0];
        assert_eq!(set.replica(first).label, "replica-2.test.invalid:8000");
    }

    #[test]
    fn guards_release_their_slot() {
        let set = ReplicaSet::new(&ReplicaAffinityConfig {
            urls: vec!["http://guard-test.test.invalid:8000/v1".parse().unwrap()],
            load_factor: 1.0,
            failover_on_status: Vec::new(),
        });
        let guard = set.acquire(0);
        assert_eq!(set.replicas[0].load.load(Ordering::Acquire), 1);
        drop(guard);
        assert_eq!(set.replicas[0].load.load(Ordering::Acquire), 0);
    }

    #[test]
    fn validation_rejects_a_path_mismatch_and_duplicates() {
        let provider: Url = "http://service.test.invalid:8000/v1".parse().unwrap();
        assert!(config(3, 1.25).validate(&provider).is_ok());
        let mut wrong_path = config(1, 1.25);
        wrong_path.urls[0] = "http://replica-0.test.invalid:8000/v2".parse().unwrap();
        assert!(wrong_path.validate(&provider).is_err());
        let mut duplicate = config(2, 1.25);
        duplicate.urls[1] = "http://replica-0.test.invalid:8000/v1/".parse().unwrap();
        assert!(duplicate.validate(&provider).is_err());
        assert!(config(2, 0.5).validate(&provider).is_err());
        let mut success_status = config(2, 1.25);
        success_status.failover_on_status = vec![200];
        assert!(success_status.validate(&provider).is_err());
        assert!(config(0, 1.25).validate(&provider).is_err());
    }
}
