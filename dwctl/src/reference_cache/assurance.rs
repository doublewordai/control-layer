//! Assured cache billing: an organisation's terms for billing an unmarked request's
//! cache read from the reference estimate instead of the engine's own hit.
//!
//! Routing does not always land a conversation on the replica that holds its prefix, so
//! the engine reports fewer cached tokens than an ideal cache would have served. For an
//! organisation with terms, part of that gap is billed as cached anyway, per response:
//!
//! - [`CacheAssurance::GapShare`] (α): `max(engine, engine + α·max(0, reference − engine))`.
//! - [`CacheAssurance::PromptFloor`] (X): `max(engine, min(reference, X·prompt_tokens))`.
//!
//! The result is never below the engine's own hit, never above `prompt_tokens`, and is
//! what the response reports in `cached_tokens`. The physical hit stays in
//! `engine_cached_tokens` and the estimate in `reference_cached_tokens`, so the gap the
//! assurance covered is always `cache_read_input_tokens − engine_cached_tokens`.
//!
//! Terms live in the org overlay catalog (`cache_assurance` next to `cache_tariff`),
//! which writes them to `model_overlays.cache_assurance`. [`AssuranceBook`] holds them in
//! memory and reloads them at most once a minute.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::metrics::errors::component::REFERENCE_CACHE;
use crate::types::UserId;

/// How often the terms are reloaded from the database.
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// An organisation's cache-assurance terms on one model. Written as a map with exactly
/// one of its keys: `{gap_share: 0.4}` or `{prompt_floor: 0.9}`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "AssuranceTerms", into = "AssuranceTerms")]
pub enum CacheAssurance {
    /// α in `[0, 1]`: bill the engine's hit plus this share of the tokens the reference
    /// cache would have served beyond it.
    GapShare(f64),
    /// X in `[0, 1]`: bill at least the reference estimate, up to this share of the
    /// prompt, and never less than the engine's hit.
    PromptFloor(f64),
}

/// The written form of [`CacheAssurance`]. A plain map (rather than a tagged enum) so
/// YAML and JSON read the same.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AssuranceTerms {
    /// α in `[0, 1]`: bill `engine + α·max(0, reference − engine)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap_share: Option<f64>,
    /// X in `[0, 1]`: bill `max(engine, min(reference, X·prompt_tokens))`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_floor: Option<f64>,
}

impl TryFrom<AssuranceTerms> for CacheAssurance {
    type Error = String;

    fn try_from(t: AssuranceTerms) -> Result<Self, String> {
        match (t.gap_share, t.prompt_floor) {
            (Some(alpha), None) => Ok(Self::GapShare(alpha)),
            (None, Some(share)) => Ok(Self::PromptFloor(share)),
            _ => Err("cache_assurance takes exactly one of gap_share or prompt_floor".to_string()),
        }
    }
}

impl From<CacheAssurance> for AssuranceTerms {
    fn from(c: CacheAssurance) -> Self {
        match c {
            CacheAssurance::GapShare(alpha) => Self {
                gap_share: Some(alpha),
                ..Self::default()
            },
            CacheAssurance::PromptFloor(share) => Self {
                prompt_floor: Some(share),
                ..Self::default()
            },
        }
    }
}

impl CacheAssurance {
    pub fn validate(&self, context: &str) -> anyhow::Result<()> {
        let (name, share) = match self {
            Self::GapShare(s) => ("gap_share", *s),
            Self::PromptFloor(s) => ("prompt_floor", *s),
        };
        anyhow::ensure!(
            share.is_finite() && (0.0..=1.0).contains(&share),
            "{context}: cache_assurance.{name} must be between 0 and 1, got {share}"
        );
        Ok(())
    }

    /// The cached tokens billed under these terms. `engine` is the engine's hit and
    /// `reference` the estimate, both already capped at `prompt_tokens`.
    pub fn billed_read(&self, engine: u64, reference: u64, prompt_tokens: u64) -> u64 {
        let engine = engine.min(prompt_tokens);
        let reference = reference.min(prompt_tokens);
        let assured = match *self {
            // Rounded down: the share is a concession, never a token more than it says.
            Self::GapShare(alpha) => engine.saturating_add((alpha * reference.saturating_sub(engine) as f64).floor() as u64),
            Self::PromptFloor(share) => reference.min((share * prompt_tokens as f64).floor() as u64),
        };
        // One candidate per source the read may come from; a later source (tokens held
        // on another replica) is one more element here.
        [engine, assured].into_iter().max().unwrap_or(0).min(prompt_tokens)
    }
}

/// Every organisation's terms, keyed by `(principal, model alias)`.
pub struct AssuranceBook {
    terms: ArcSwap<HashMap<(UserId, String), CacheAssurance>>,
    /// Absent in tests that pin the terms.
    pool: Option<sqlx_pool_router::DynPools>,
    loaded_at: std::sync::Mutex<Option<Instant>>,
    refreshing: AtomicBool,
}

impl std::fmt::Debug for AssuranceBook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssuranceBook").field("terms", &self.terms.load().len()).finish()
    }
}

impl AssuranceBook {
    /// A book read from `model_overlays`; empty until [`Self::refresh`] first runs.
    pub fn new(pool: sqlx_pool_router::DynPools) -> Self {
        Self {
            terms: ArcSwap::from_pointee(HashMap::new()),
            pool: Some(pool),
            loaded_at: std::sync::Mutex::new(None),
            refreshing: AtomicBool::new(false),
        }
    }

    /// A book with fixed terms that never reloads.
    pub fn fixed(terms: HashMap<(UserId, String), CacheAssurance>) -> Self {
        Self {
            terms: ArcSwap::from_pointee(terms),
            pool: None,
            loaded_at: std::sync::Mutex::new(Some(Instant::now())),
            refreshing: AtomicBool::new(false),
        }
    }

    pub fn terms(&self, principal: UserId, model: &str) -> Option<CacheAssurance> {
        self.terms.load().get(&(principal, model.to_string())).copied()
    }

    pub fn len(&self) -> usize {
        self.terms.load().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reload the terms. A failure keeps the ones already loaded.
    pub async fn refresh(&self) -> Result<(), sqlx::Error> {
        let Some(pool) = &self.pool else {
            return Ok(());
        };
        let rows = sqlx::query!(
            r#"
            SELECT mo.user_id, dm.alias, mo.cache_assurance AS "cache_assurance!"
            FROM model_overlays mo
            JOIN deployed_models dm ON dm.id = mo.deployed_model_id
            WHERE mo.cache_assurance IS NOT NULL AND dm.deleted = false
            "#
        )
        // Primary: read right after startup applies the overlay catalog.
        .fetch_all(&pool.write())
        .await?;
        let mut terms = HashMap::with_capacity(rows.len());
        for row in rows {
            match serde_json::from_value::<CacheAssurance>(row.cache_assurance) {
                Ok(t) => {
                    terms.insert((row.user_id, row.alias), t);
                }
                Err(e) => {
                    crate::background_error!(REFERENCE_CACHE, "assurance_terms", Error, error = %e, alias = %row.alias, "unreadable cache_assurance terms; the pair bills from the engine");
                }
            }
        }
        let changed = *self.terms.load_full() != terms;
        self.terms.store(Arc::new(terms));
        *self.loaded_at.lock().expect("not poisoned") = Some(Instant::now());
        if changed {
            info!(pairs = self.len(), "Cache-assurance terms loaded");
        }
        Ok(())
    }

    /// Reload in the background when the terms are older than a minute.
    pub fn refresh_if_stale(self: &Arc<Self>) {
        if self.pool.is_none() {
            return;
        }
        let stale = self
            .loaded_at
            .lock()
            .expect("not poisoned")
            .is_none_or(|t| t.elapsed() >= REFRESH_INTERVAL);
        if !stale || self.refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(e) = this.refresh().await {
                crate::background_error!(REFERENCE_CACHE, "assurance_refresh", Warning, error = %e, "reloading cache-assurance terms failed; keeping the previous ones");
                // Try again after a full interval rather than on the next request.
                *this.loaded_at.lock().expect("not poisoned") = Some(Instant::now());
            }
            this.refreshing.store(false, Ordering::Release);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gap_share_bills_the_engine_plus_a_share_of_the_gap() {
        let alpha = CacheAssurance::GapShare(0.4);
        // Gap of 1_000: 400 of it billed as cached.
        assert_eq!(alpha.billed_read(7_000, 8_000, 10_000), 7_400);
        // Rounded down.
        assert_eq!(alpha.billed_read(0, 3, 10), 1);
        // Never below the engine, even when the reference saw less.
        assert_eq!(alpha.billed_read(7_000, 5_000, 10_000), 7_000);
        // The edges: α = 0 is engine billing, α = 1 is the reference.
        assert_eq!(CacheAssurance::GapShare(0.0).billed_read(7_000, 8_000, 10_000), 7_000);
        assert_eq!(CacheAssurance::GapShare(1.0).billed_read(7_000, 8_000, 10_000), 8_000);
    }

    #[test]
    fn prompt_floor_bills_the_reference_up_to_a_share_of_the_prompt() {
        let floor = CacheAssurance::PromptFloor(0.9);
        assert_eq!(floor.billed_read(2_000, 8_000, 10_000), 8_000, "the reference, under the ceiling");
        assert_eq!(floor.billed_read(2_000, 9_800, 10_000), 9_000, "capped at 90% of the prompt");
        assert_eq!(floor.billed_read(9_500, 9_800, 10_000), 9_500, "never below the engine");
        assert_eq!(floor.billed_read(2_000, 0, 10_000), 2_000, "no reference hit, engine only");
    }

    #[test]
    fn the_billed_read_never_exceeds_the_prompt() {
        for terms in [CacheAssurance::GapShare(1.0), CacheAssurance::PromptFloor(1.0)] {
            assert_eq!(terms.billed_read(20_000, 30_000, 10_000), 10_000);
            assert_eq!(terms.billed_read(0, 30_000, 10_000), 10_000);
        }
    }

    #[test]
    fn terms_parse_strictly_and_validate_their_share() {
        let gap: CacheAssurance = serde_json::from_value(serde_json::json!({"gap_share": 0.4})).unwrap();
        assert_eq!(gap, CacheAssurance::GapShare(0.4));
        let floor: CacheAssurance = serde_yaml::from_str("prompt_floor: 0.9").unwrap();
        assert_eq!(floor, CacheAssurance::PromptFloor(0.9));
        assert!(serde_json::from_value::<CacheAssurance>(serde_json::json!({"gap_share": 0.4, "prompt_floor": 0.9})).is_err());
        assert!(serde_json::from_value::<CacheAssurance>(serde_json::json!({"alpha": 0.4})).is_err());
        assert!(serde_json::from_value::<CacheAssurance>(serde_json::json!({})).is_err());
        let schema = serde_json::to_value(schemars::schema_for!(CacheAssurance)).unwrap();
        assert!(schema["properties"]["gap_share"].is_object(), "{schema}");
        assert!(gap.validate("ctx").is_ok());
        for bad in [-0.1, 1.5, f64::NAN] {
            let err = CacheAssurance::GapShare(bad).validate("ctx").unwrap_err().to_string();
            assert!(err.contains("between 0 and 1"), "{err}");
        }
        assert_eq!(
            serde_json::to_value(CacheAssurance::PromptFloor(0.9)).unwrap(),
            serde_json::json!({"prompt_floor": 0.9}),
            "stored in model_overlays in this shape"
        );
    }

    #[test]
    fn a_fixed_book_answers_per_principal_and_model() {
        let org = uuid::Uuid::new_v4();
        let book = AssuranceBook::fixed(HashMap::from([((org, "chat".to_string()), CacheAssurance::GapShare(0.5))]));
        assert_eq!(book.terms(org, "chat"), Some(CacheAssurance::GapShare(0.5)));
        assert_eq!(book.terms(org, "other"), None);
        assert_eq!(book.terms(uuid::Uuid::new_v4(), "chat"), None);
    }
}
