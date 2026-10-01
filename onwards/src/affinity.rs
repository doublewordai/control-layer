//! Conversation affinity for priority pools.
//!
//! An agentic conversation sends many turns, each repeating the conversation so
//! far. The provider that served the previous turn usually still holds that
//! history in its prefix cache; any other provider has to process it again.
//! When a pool's preferred provider cannot take every request, choosing the
//! provider afresh for each turn moves conversations back and forth and loses
//! the cache on both sides.
//!
//! Affinity makes the choice once per conversation instead. Each request yields
//! a conversation key (below), hashed to a point `h` in `[0, 1)`. The preferred
//! provider is tried first when `h < s`, and the alternates otherwise. The share
//! `s` admits about `target_conversations` of the conversations currently
//! active, so the preferred provider keeps a stable set of conversations and the
//! rest stay with the alternates.
//!
//! No state is shared between gateway replicas. Each replica records the keys
//! it has seen within `active_window_ms` and recomputes `s` at the same
//! wall-clock instants (multiples of `update_interval_ms`). A conversation's
//! requests are spread over the replicas, so every replica sees the same active
//! conversations and arrives at the same `s`. The share only moves when the
//! number of admitted conversations leaves `target_conversations ± margin`: new
//! conversations do not displace admitted ones while the preferred provider has
//! room, and conversations stay put while the population is steady.
//!
//! ## The conversation key
//!
//! - The `x-session-id` header, or a `session_id` or `prompt_cache_key` string
//!   in the request body, when the client sends one.
//! - Otherwise the conversation's opening: its first `system` or `developer`
//!   message together with its first message of any other role (chat
//!   completions), or `instructions` with the first `input` item (responses).
//!   Clients resend a conversation's history on every turn, so the opening
//!   identifies the conversation for its whole life.
//!
//! Requests without a key use ordinary priority selection.
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Header carrying an explicit conversation identifier.
pub const SESSION_HEADER: &str = "x-session-id";

/// Opt-in per-conversation preferred/alternate split for a priority pool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AffinityConfig {
    pub enabled: bool,
    /// Conversations to keep on the preferred provider.
    pub target_conversations: usize,
    /// How far the number of admitted conversations may drift from the target
    /// before the share moves. Defaults to a tenth of the target, at least 1.
    pub margin: Option<usize>,
    /// A conversation counts as active for this long after its last request.
    pub active_window_ms: u64,
    /// The share is recomputed at multiples of this much wall-clock time, so
    /// every replica recomputes at the same instants.
    pub update_interval_ms: u64,
    /// Upper bound on the conversations tracked per pool.
    pub max_tracked: usize,
}

impl Default for AffinityConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            target_conversations: 0,
            margin: None,
            active_window_ms: 600_000,
            update_interval_ms: 60_000,
            max_tracked: 100_000,
        }
    }
}

impl AffinityConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.target_conversations == 0 || self.target_conversations > 1_000_000 {
            return Err("affinity target_conversations must be 1..=1000000");
        }
        if self.active_window_ms < 1_000 || self.active_window_ms > 86_400_000 {
            return Err("affinity active_window_ms must be 1000..=86400000");
        }
        if self.update_interval_ms < 1_000 || self.update_interval_ms > self.active_window_ms {
            return Err("affinity update_interval_ms must be 1000..=active_window_ms");
        }
        if self
            .margin
            .is_some_and(|margin| margin > self.target_conversations)
        {
            return Err("affinity margin must be at most target_conversations");
        }
        if self.max_tracked < self.target_conversations + self.margin()
            || self.max_tracked > 10_000_000
        {
            return Err(
                "affinity max_tracked must cover target_conversations + margin and be at most 10000000",
            );
        }
        Ok(())
    }

    pub fn margin(&self) -> usize {
        self.margin
            .unwrap_or_else(|| self.target_conversations.div_ceil(10))
            .max(1)
    }
}

/// The conversation a request belongs to, as a stable 64-bit hash. `None` when
/// the request carries nothing that identifies a conversation.
///
/// `session_header` is the value of [`SESSION_HEADER`], if present.
pub fn conversation_key(session_header: Option<&str>, body: &Value) -> Option<u64> {
    let explicit = session_header
        .filter(|s| !s.is_empty())
        .or_else(|| {
            body.get("session_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            body.get("prompt_cache_key")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        });
    if let Some(id) = explicit {
        return Some(hash(&[b"id", id.as_bytes()]));
    }
    let (system, first) = opening(body)?;
    let system = system.map(|(role, content)| (role, canonical(content)));
    let first_content = canonical(first.1);
    let mut parts: Vec<&[u8]> = vec![b"open"];
    if let Some((role, content)) = &system {
        parts.extend([role.as_bytes(), content.as_bytes()]);
    }
    parts.extend([first.0.as_bytes(), first_content.as_bytes()]);
    Some(hash(&parts))
}

/// `h` in `[0, 1)` for a conversation key.
pub fn point(key: u64) -> f64 {
    (key >> 11) as f64 / (1u64 << 53) as f64
}

type Message<'a> = (&'a str, &'a Value);

/// The first system/developer message (if any) and the first message of
/// another role, from chat-completions `messages` or responses `instructions`
/// and `input`.
fn opening(body: &Value) -> Option<(Option<Message<'_>>, Message<'_>)> {
    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        return split_opening(messages.iter(), None);
    }
    let instructions = body
        .get("instructions")
        .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()))
        .map(|v| ("system", v));
    match body.get("input")? {
        input @ Value::String(s) if !s.is_empty() => Some((instructions, ("user", input))),
        Value::Array(items) => split_opening(items.iter(), instructions),
        _ => None,
    }
}

fn split_opening<'a>(
    items: impl Iterator<Item = &'a Value>,
    mut system: Option<Message<'a>>,
) -> Option<(Option<Message<'a>>, Message<'a>)> {
    for item in items {
        let Some(role) = item.get("role").and_then(Value::as_str) else {
            continue;
        };
        let content = item.get("content").unwrap_or(&Value::Null);
        if role == "system" || role == "developer" {
            system.get_or_insert((role, content));
        } else {
            return Some((system, (role, content)));
        }
    }
    None
}

fn canonical(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// SHA-256 over length-prefixed parts, truncated to 64 bits. Stable across
/// processes, platforms and releases, which per-process hashers are not.
fn hash(parts: &[&[u8]]) -> u64 {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    let digest = hasher.finalize();
    u64::from_le_bytes(digest[..8].try_into().expect("digest has 32 bytes"))
}

/// Milliseconds since the Unix epoch, the clock replicas agree on.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// What the tracker last computed, for metrics.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Snapshot {
    pub share: f64,
    pub active: usize,
    pub admitted: usize,
}

/// One pool's per-process view of its active conversations and the share.
#[derive(Debug)]
pub struct Tracker {
    config: AffinityConfig,
    /// Conversation key → last time a request from it was seen (Unix ms).
    /// Ordered by key, which orders the conversations by point.
    seen: BTreeMap<u64, u64>,
    share: f64,
    epoch: Option<u64>,
    snapshot: Snapshot,
}

impl Tracker {
    pub fn new(config: AffinityConfig) -> Self {
        Self {
            config,
            seen: BTreeMap::new(),
            share: 1.0,
            epoch: None,
            snapshot: Snapshot {
                share: 1.0,
                active: 0,
                admitted: 0,
            },
        }
    }

    pub fn config(&self) -> &AffinityConfig {
        &self.config
    }

    pub fn share(&self) -> f64 {
        self.share
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot
    }

    /// Record a request from conversation `key` at `now_ms`; return whether the
    /// conversation belongs on the preferred provider, and whether the share was
    /// recomputed by this call.
    pub fn route(&mut self, key: u64, now_ms: u64) -> (bool, bool) {
        self.record(key, now_ms);
        let epoch = now_ms / self.config.update_interval_ms;
        let updated = self.epoch != Some(epoch);
        if updated {
            self.epoch = Some(epoch);
            self.update(now_ms);
        }
        (point(key) < self.share, updated)
    }

    /// Track `key`. A full tracker keeps the conversations with the smallest
    /// points: only those decide the share, so it stays exact however many
    /// conversations are active, and only the active count saturates.
    fn record(&mut self, key: u64, now_ms: u64) {
        if let Some(last) = self.seen.get_mut(&key) {
            *last = now_ms;
            return;
        }
        if self.seen.len() >= self.config.max_tracked {
            match self.seen.last_key_value() {
                Some((&largest, _)) if largest > key => {
                    self.seen.remove(&largest);
                }
                _ => return,
            }
        }
        self.seen.insert(key, now_ms);
    }

    fn update(&mut self, now_ms: u64) {
        let cutoff = now_ms.saturating_sub(self.config.active_window_ms);
        self.seen.retain(|_, last| *last >= cutoff);
        // Ascending, because the keys are.
        let points: Vec<f64> = self.seen.keys().map(|k| point(*k)).collect();
        let admitted = points.partition_point(|p| *p < self.share);
        let target = self.config.target_conversations;
        let margin = self.config.margin();
        let over = admitted > target + margin;
        let under = admitted + margin < target && self.share < 1.0;
        if over || under {
            // The (target+1)-th smallest point: exactly `target` lie below it.
            self.share = points.get(target).copied().unwrap_or(1.0);
        }
        self.snapshot = Snapshot {
            share: self.share,
            active: points.len(),
            admitted: points.partition_point(|p| *p < self.share),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(target: usize) -> AffinityConfig {
        AffinityConfig {
            target_conversations: target,
            update_interval_ms: 60_000,
            active_window_ms: 600_000,
            ..AffinityConfig::default()
        }
    }

    fn chat(system: &str, user: &str, later: &[&str]) -> Value {
        let mut messages = vec![
            json!({"role": "system", "content": system}),
            json!({"role": "user", "content": user}),
        ];
        for (i, text) in later.iter().enumerate() {
            let role = if i % 2 == 0 { "assistant" } else { "user" };
            messages.push(json!({"role": role, "content": text}));
        }
        json!({"model": "m", "messages": messages})
    }

    #[test]
    fn opening_key_is_stable_as_the_conversation_grows() {
        let first = conversation_key(None, &chat("be brief", "task 1", &[])).unwrap();
        let later =
            conversation_key(None, &chat("be brief", "task 1", &["ok", "next", "done"])).unwrap();
        assert_eq!(first, later);
        let other = conversation_key(None, &chat("be brief", "task 2", &[])).unwrap();
        assert_ne!(first, other);
    }

    #[test]
    fn explicit_identifiers_take_precedence_over_the_opening() {
        let body = chat("s", "u", &[]);
        let mut with_id = body.clone();
        with_id["session_id"] = json!("abc");
        let mut with_cache_key = body.clone();
        with_cache_key["prompt_cache_key"] = json!("abc");
        let header = conversation_key(Some("abc"), &body).unwrap();
        assert_eq!(header, conversation_key(None, &with_id).unwrap());
        assert_eq!(header, conversation_key(None, &with_cache_key).unwrap());
        assert_ne!(header, conversation_key(None, &body).unwrap());
        // An empty identifier is ignored.
        assert_eq!(
            conversation_key(Some(""), &body),
            conversation_key(None, &body)
        );
    }

    #[test]
    fn developer_messages_count_as_system_and_only_the_first_is_used() {
        let a = json!({"messages": [
            {"role": "developer", "content": "rules"},
            {"role": "system", "content": "more rules"},
            {"role": "user", "content": "hi"}]});
        let b = json!({"messages": [
            {"role": "developer", "content": "rules"},
            {"role": "user", "content": "hi"}]});
        assert_eq!(conversation_key(None, &a), conversation_key(None, &b));
    }

    #[test]
    fn structured_content_and_responses_input_are_keyed() {
        let parts =
            json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}]});
        assert!(conversation_key(None, &parts).is_some());
        let responses = json!({"instructions": "rules", "input": [{"role": "user", "content": "hi"}, {"role": "assistant", "content": "yo"}]});
        let responses_later =
            json!({"instructions": "rules", "input": [{"role": "user", "content": "hi"}]});
        assert_eq!(
            conversation_key(None, &responses),
            conversation_key(None, &responses_later)
        );
        assert!(conversation_key(None, &json!({"input": "hello"})).is_some());
    }

    #[test]
    fn requests_without_a_conversation_have_no_key() {
        assert_eq!(
            conversation_key(None, &json!({"prompt": "complete me"})),
            None
        );
        assert_eq!(
            conversation_key(
                None,
                &json!({"messages": [{"role": "system", "content": "only"}]})
            ),
            None
        );
        assert_eq!(conversation_key(None, &json!({"messages": []})), None);
    }

    #[test]
    fn points_are_uniform_enough() {
        let below = (0..10_000u64)
            .filter(|i| point(hash(&[&i.to_le_bytes()])) < 0.3)
            .count();
        assert!((2_700..3_300).contains(&below), "{below}");
    }

    #[test]
    fn admits_everything_below_target_then_holds_about_target() {
        let mut t = Tracker::new(config(100));
        // 50 conversations: all admitted.
        for k in 0..50u64 {
            assert!(t.route(hash(&[&k.to_le_bytes()]), 1_000).0);
        }
        // 400 conversations active by the next update.
        for k in 0..400u64 {
            t.route(hash(&[&k.to_le_bytes()]), 61_000);
        }
        t.route(hash(&[b"tick"]), 121_000);
        let snapshot = t.snapshot();
        assert_eq!(snapshot.active, 401);
        assert!((100..=101).contains(&snapshot.admitted), "{snapshot:?}");
        assert!(snapshot.share < 1.0);
    }

    #[test]
    fn a_full_tracker_still_admits_about_target() {
        let mut t = Tracker::new(AffinityConfig {
            max_tracked: 50,
            ..config(10)
        });
        let keys: Vec<u64> = (0..1_000u64).map(|k| hash(&[&k.to_le_bytes()])).collect();
        for minute in 0..2 {
            for k in &keys {
                t.route(*k, 1_000 + minute * 60_000);
            }
        }
        let preferred = keys.iter().filter(|k| point(**k) < t.share()).count();
        assert!((10..=11).contains(&preferred), "{preferred}");
        assert_eq!(t.snapshot().active, 50);
    }

    #[test]
    fn decisions_are_stable_while_the_population_is_steady() {
        let mut t = Tracker::new(config(100));
        let keys: Vec<u64> = (0..300u64).map(|k| hash(&[&k.to_le_bytes()])).collect();
        for minute in 0..3 {
            for k in &keys {
                t.route(*k, 1_000 + minute * 60_000);
            }
        }
        let before: Vec<bool> = keys.iter().map(|k| point(*k) < t.share()).collect();
        // Conversations come and go at the same rate: 20 end, 20 new ones start.
        let fresh: Vec<u64> = (1_000..1_020u64)
            .map(|k| hash(&[&k.to_le_bytes()]))
            .collect();
        for minute in 3..20 {
            for k in keys.iter().skip(20).chain(fresh.iter()) {
                t.route(*k, 1_000 + minute * 60_000);
            }
        }
        let after: Vec<bool> = keys.iter().map(|k| point(*k) < t.share()).collect();
        let moved = keys
            .iter()
            .skip(20)
            .zip(before.iter().skip(20).zip(after.iter().skip(20)))
            .filter(|(_, (b, a))| b != a)
            .count();
        assert_eq!(
            moved, 0,
            "admitted conversations should stay while the population is steady"
        );
    }

    #[test]
    fn replicas_that_see_the_same_conversations_agree() {
        let keys: Vec<u64> = (0..500u64).map(|k| hash(&[&k.to_le_bytes()])).collect();
        let mut replicas: Vec<Tracker> = (0..3).map(|_| Tracker::new(config(120))).collect();
        for minute in 0..15u64 {
            for (i, k) in keys.iter().enumerate() {
                // Each request lands on one replica; every conversation reaches
                // every replica within a few minutes.
                let r = (i as u64 + minute) as usize % 3;
                replicas[r].route(*k, 5_000 + minute * 60_000);
            }
        }
        let now = 5_000 + 15 * 60_000;
        for r in replicas.iter_mut() {
            r.route(hash(&[b"tick"]), now);
        }
        let shares: Vec<f64> = replicas.iter().map(Tracker::share).collect();
        assert!(shares.windows(2).all(|w| w[0] == w[1]), "{shares:?}");
    }

    #[test]
    fn share_returns_to_one_when_load_falls() {
        let mut t = Tracker::new(config(10));
        for k in 0..100u64 {
            t.route(hash(&[&k.to_le_bytes()]), 1_000);
        }
        t.route(hash(&[b"x"]), 61_000);
        assert!(t.share() < 1.0);
        // Everyone goes quiet; five conversations remain.
        for minute in 2..15u64 {
            for k in 0..5u64 {
                t.route(hash(&[&k.to_le_bytes()]), 1_000 + minute * 60_000);
            }
        }
        assert_eq!(t.share(), 1.0);
    }

    #[test]
    fn validation() {
        assert!(config(10).validate().is_ok());
        assert!(AffinityConfig::default().validate().is_err());
        for (margin, ok) in [(0, true), (10, true), (11, false), (usize::MAX, false)] {
            let config = AffinityConfig {
                margin: Some(margin),
                ..config(10)
            };
            assert_eq!(config.validate().is_ok(), ok, "margin {margin}");
        }
        assert!(
            AffinityConfig {
                update_interval_ms: 999,
                ..config(10)
            }
            .validate()
            .is_err()
        );
        assert!(
            AffinityConfig {
                update_interval_ms: 700_000,
                ..config(10)
            }
            .validate()
            .is_err()
        );
        assert!(
            AffinityConfig {
                max_tracked: 5,
                ..config(10)
            }
            .validate()
            .is_err()
        );
        let parsed: AffinityConfig =
            serde_json::from_value(json!({"target_conversations": 50})).unwrap();
        assert!(parsed.enabled && parsed.validate().is_ok());
        assert!(serde_json::from_value::<AffinityConfig>(json!({"target": 5})).is_err());
    }
}
