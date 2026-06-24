//! The `CacheIndex` abstraction (plan §6.4): a Postgres baseline plus an optional
//! Redis accelerator behind one trait. It is a **cache, not a ledger** — billing
//! truth is `credits_transactions`. A lost entry degrades to "cache miss / full
//! price" (safe); the index is never walked to reprice.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::types::UserId;

/// A cumulative prefix-content hash: the content up to and including a breakpoint
/// block, **excluding** the `cache_control` directive (plan §3). Identical content
/// carrying different markers therefore matches — and it is the same byte span
/// onwards forwards upstream after stripping markers.
pub type PrefixHash = Vec<u8>;

/// TTL tier of a cache entry. The window is *sliding*: every read resets expiry to
/// `now + duration(tier)` (plan §1), so a tier is the max tolerated gap between
/// uses, not a fixed lifetime. The tier sets the write premium; the read discount
/// is flat across tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtlTier {
    FiveMinutes,
    OneHour,
    TwentyFourHours,
}

impl TtlTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FiveMinutes => "5m",
            Self::OneHour => "1h",
            Self::TwentyFourHours => "24h",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "5m" => Some(Self::FiveMinutes),
            "1h" => Some(Self::OneHour),
            "24h" => Some(Self::TwentyFourHours),
            _ => None,
        }
    }

    /// The window added to `now()` on write and on every read refresh.
    pub fn duration(self) -> chrono::Duration {
        match self {
            Self::FiveMinutes => chrono::Duration::minutes(5),
            Self::OneHour => chrono::Duration::hours(1),
            Self::TwentyFourHours => chrono::Duration::hours(24),
        }
    }
}

/// The cache scope keying every entry (plan §8.1):
/// - `principal_id` = `target_user_id` (org or personal user = `api_key.user_id`), so all
///   of a customer's modalities share one cache scope.
/// - `virtual_model` = the user-facing alias (`deployed_models.alias`), not the
///   rewritten underlying `model_name`.
/// - `tokenizer_version` = emitted by tokenizer-svc; re-keys entries on a tokenizer
///   change so stale prefixes age out by TTL.
#[derive(Debug, Clone)]
pub struct IndexScope {
    pub principal_id: UserId,
    pub virtual_model: String,
    pub tokenizer_version: String,
}

/// A live entry returned by [`CacheIndex::lookup`] for one candidate hash (a read
/// hit). The stored `cumulative_token_count` is reused with no tokenization.
#[derive(Debug, Clone)]
pub struct CacheMatch {
    pub prefix_hash: PrefixHash,
    pub cumulative_token_count: u32,
    pub ttl_tier: TtlTier,
    pub expires_at: DateTime<Utc>,
}

/// A new cache write to record. Committed post-response, success-gated (plan §6.3
/// step 8): gap-capping + billing integrity.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub scope: IndexScope,
    pub prefix_hash: PrefixHash,
    pub cumulative_token_count: u32,
    pub ttl_tier: TtlTier,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("cache index database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("cache principal lookup failed: {0}")]
    Principal(#[from] crate::db::errors::DbError),
    #[error("invalid cache data: {0}")]
    Invalid(String),
}

pub type CacheResult<T> = std::result::Result<T, CacheError>;

/// The prefix index. Postgres is the always-correct baseline; a Redis accelerator
/// (later) sits in front, read-through with write-behind refreshes. Callers treat
/// any `Err` as "no cache" and bill full price — never a wrong charge.
#[async_trait]
pub trait CacheIndex: Send + Sync {
    /// Which of `candidate_hashes` are live entries for `scope`. No tokenization —
    /// the stored token count rides on the match (plan §3).
    async fn lookup(&self, scope: &IndexScope, candidate_hashes: &[PrefixHash]) -> CacheResult<Vec<CacheMatch>>;

    /// Record a new write. Write-through (durable immediately); upsert on the
    /// scope+hash unique key.
    async fn write(&self, entry: &CacheEntry) -> CacheResult<()>;

    /// Slide an entry's expiry forward on read (the sliding window). A direct
    /// `UPDATE` in Postgres; write-behind / debounced in the Redis accelerator.
    async fn refresh(&self, scope: &IndexScope, prefix_hash: &PrefixHash, new_expires_at: DateTime<Utc>) -> CacheResult<()>;
}
