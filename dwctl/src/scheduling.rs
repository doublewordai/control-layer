//! Scheduling tolerations pinned to an account's inference requests.
//!
//! An operator can pin an account to a named *preset* of scheduling
//! tolerations, declared in the deployment's config
//! (`scheduling.toleration_presets`, see [`crate::config::SchedulingConfig`]).
//! Every inference request from a pinned account then carries that preset's
//! list verbatim at `nvext.routing_constraints.tolerations`, before the request
//! reaches the upstream inference backend, for realtime, flex and batch traffic
//! alike, so a backend that honours tolerations keeps the account's work on the
//! capacity the operator intends. A typical preset is the empty list, which
//! asks a backend to keep the account off any tainted capacity.
//!
//! The account stores only the preset's id (`users.toleration_preset`);
//! resolving id to tolerations happens in
//! [`crate::sync::key_policy`] (honouring the preset's definition at sync time)
//! and at batch upload. `NULL` means "Default": nothing is pinned and the
//! backend decides. An id the config no longer defines fails closed to the
//! empty list (see [`TolerationPresets`]).
//!
//! This module owns the one canonical toleration shape and its validation. The
//! config loader parses presets through it, and the value the request path
//! writes is exactly what `serde_json::to_value` produces here, so the body is
//! one the client could have sent itself.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The sentinel the dashboard's account-settings select uses for "Default": no
/// preset id is stored, so nothing is pinned. Radix Select forbids an
/// empty-string item value, hence a sentinel. Reserved here so config load
/// refuses a preset that would collide with it, rendering duplicate options and
/// making that preset unselectable. Must stay equal to the dashboard's
/// `DEFAULT_TOLERATION_PRESET`.
pub const DEFAULT_PRESET_SENTINEL: &str = "__default__";

/// A fixed list of scheduling tolerations.
///
/// A newtype over `Vec<SchedulingToleration>` so it is a distinct schema in
/// the OpenAPI document and can grow validation without changing the wire
/// shape. Serialises to a JSON array, matching what the request path writes
/// to `nvext.routing_constraints.tolerations`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(transparent)]
pub struct PinnedTolerations(pub Vec<SchedulingToleration>);

/// One Kubernetes-style scheduling toleration, the shape written to
/// `nvext.routing_constraints.tolerations`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SchedulingToleration {
    /// Taint key the toleration matches.
    pub key: String,
    /// How `key` is matched. Defaults to `Equal` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator: Option<SchedulingOperator>,
    /// Value matched when `operator` is `Equal`; must be absent for `Exists`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// How strictly the taint must be tolerated. Defaults to `NoSchedule`
    /// when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<SchedulingEffect>,
}

/// Operator for [`SchedulingToleration`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum SchedulingOperator {
    Equal,
    Exists,
}

/// Effect for [`SchedulingToleration`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum SchedulingEffect {
    #[serde(rename = "NoSchedule")]
    #[schema(rename = "NoSchedule")]
    NoSchedule,
    #[serde(rename = "PreferNoSchedule")]
    #[schema(rename = "PreferNoSchedule")]
    PreferNoSchedule,
}

/// A toleration the request path could not send.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TolerationError {
    #[error("toleration {index}: key must not be empty")]
    EmptyKey { index: usize },
    #[error("toleration {index}: operator Equal needs a value (or set operator: Exists)")]
    EqualNeedsValue { index: usize },
    #[error("toleration {index}: operator Exists must not carry a value")]
    ExistsNeedsNoValue { index: usize },
}

impl PinnedTolerations {
    /// Validate every entry before the list is stored or served. Rejects a
    /// toleration the request path could not serialise into a valid body, so a
    /// malformed preset fails config load rather than producing an invalid
    /// upstream request later.
    pub fn validate(&self) -> Result<(), TolerationError> {
        for (index, toleration) in self.0.iter().enumerate() {
            if toleration.key.is_empty() {
                return Err(TolerationError::EmptyKey { index });
            }
            match toleration.operator.unwrap_or(SchedulingOperator::Equal) {
                SchedulingOperator::Equal => {
                    if toleration.value.is_none() {
                        return Err(TolerationError::EqualNeedsValue { index });
                    }
                }
                SchedulingOperator::Exists => {
                    if toleration.value.is_some() {
                        return Err(TolerationError::ExistsNeedsNoValue { index });
                    }
                }
            }
        }
        Ok(())
    }

    /// The list exactly as it will be written to the request body.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("PinnedTolerations always serialises")
    }
}

/// The deployment's presets, resolved to the JSON the request path writes and
/// keyed by preset id. Built at startup from
/// [`crate::config::SchedulingConfig`], and shared read-only on the request
/// hot path. Held behind an [`arc_swap::ArcSwap`] so a config reload can swap in new
/// presets without a restart.
#[derive(Debug, Clone)]
pub struct TolerationPresets {
    /// Preset id → the toleration list as the request body JSON. The list is
    /// pre-serialised so the hot path clones an `Arc` and never re-serialises.
    /// Behind an [`arc_swap::ArcSwap`] so a reload swaps the whole map atomically.
    by_id: Arc<arc_swap::ArcSwap<HashMap<String, Arc<serde_json::Value>>>>,
    /// The pre-resolved fail-closed value (`[]`, the most restrictive list),
    /// shared so a retired preset does not allocate a fresh `[]` on every
    /// request.
    empty: Arc<serde_json::Value>,
    /// Ids that already logged a fail-closed miss, so a single retired id a
    /// request path references repeatedly logs once rather than on every
    /// request. Shared across clones (a reload keeps the log history).
    reported_unknown: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
}

impl Default for TolerationPresets {
    /// No presets: every configured id fails closed to the empty list.
    fn default() -> Self {
        Self {
            by_id: Arc::new(arc_swap::ArcSwap::from_pointee(HashMap::new())),
            empty: Arc::new(serde_json::json!([])),
            reported_unknown: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        }
    }
}

impl TolerationPresets {
    /// Build the resolved map from the configured presets. Validation happens
    /// at config load (`SchedulingConfig::validate`), so a duplicate id here is
    /// a programming error: the last one wins.
    pub fn new(config: &crate::config::SchedulingConfig) -> Self {
        let by_id: HashMap<String, Arc<serde_json::Value>> = config
            .toleration_presets
            .iter()
            .map(|preset| (preset.id.clone(), Arc::new(PinnedTolerations(preset.tolerations.clone()).to_json())))
            .collect();
        Self {
            by_id: Arc::new(arc_swap::ArcSwap::from_pointee(by_id)),
            empty: Arc::new(serde_json::json!([])),
            reported_unknown: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        }
    }

    /// Swap in a new set of presets, so a config reload can add, change or drop
    /// them without a restart. A dropped id then fails closed (the next request
    /// that references it resolves to the empty list) rather than continuing to
    /// resolve to a stale value.
    pub fn store(&self, presets: Self) {
        let by_id = presets.by_id.load_full();
        self.by_id.store(by_id);
    }

    /// Resolve an account's stored preset id to the JSON the request path
    /// writes, or `None` when the account is on "Default" (nothing pinned).
    ///
    /// An id that is absent from this deployment's config fails closed: the
    /// empty list (`[]`, the most restrictive option) is returned rather than
    /// silently leaving the request unpinned, and the miss is logged once per
    /// distinct id so a single retired id does not flood the log on every
    /// request.
    pub fn resolve(&self, preset_id: Option<&str>) -> Option<Arc<serde_json::Value>> {
        let id = preset_id?;
        match self.by_id.load().get(id) {
            Some(tolerations) => Some(tolerations.clone()),
            None => {
                if self.mark_unknown_reported(id) {
                    tracing::error!(
                        preset_id = id,
                        "account references a scheduling toleration preset that is not configured; \
                         failing closed to the empty (most restrictive) toleration list"
                    );
                }
                Some(self.empty.clone())
            }
        }
    }

    /// Record that `id` is a fail-closed miss, returning `true` the first time
    /// it is seen (and so should be logged) and `false` afterwards, so a
    /// retired id read on a hot path logs once rather than on every request.
    fn mark_unknown_reported(&self, id: &str) -> bool {
        self.reported_unknown
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{SchedulingConfig, TolerationPreset};

    fn parse(value: serde_json::Value) -> (PinnedTolerations, serde_json::Value) {
        let pinned: PinnedTolerations = serde_json::from_value(value.clone()).unwrap();
        (pinned, value)
    }

    #[test]
    fn an_empty_list_round_trips() {
        let (pinned, _) = parse(serde_json::json!([]));
        assert_eq!(pinned.to_json(), serde_json::json!([]));
        pinned.validate().unwrap();
    }

    #[test]
    fn entries_keep_only_the_fields_the_client_sent() {
        let (pinned, original) = parse(serde_json::json!([
            {"key": "dedicated", "value": "only", "effect": "NoSchedule"},
            {"key": "gpu", "operator": "Exists"},
        ]));
        // Serialisation preserves the shape; omitted operator/value/effect
        // stay omitted rather than being padded with defaults.
        assert_eq!(pinned.to_json(), original);
        pinned.validate().unwrap();
    }

    #[test]
    fn equal_without_a_value_and_exists_with_one_are_refused() {
        let (equal, _) = parse(serde_json::json!([{"key": "dedicated"}]));
        assert_eq!(equal.validate(), Err(TolerationError::EqualNeedsValue { index: 0 }));

        let (exists, _) = parse(serde_json::json!([{"key": "dedicated", "operator": "Exists", "value": "nope"}]));
        assert_eq!(exists.validate(), Err(TolerationError::ExistsNeedsNoValue { index: 0 }));

        let (empty_key, _) = parse(serde_json::json!([{"key": ""}]));
        assert_eq!(empty_key.validate(), Err(TolerationError::EmptyKey { index: 0 }));
    }

    #[test]
    fn unknown_fields_and_spellings_are_refused_at_parse() {
        assert!(serde_json::from_value::<PinnedTolerations>(serde_json::json!([{"key": "k", "value": "v", "priority": 1}])).is_err());
        assert!(serde_json::from_value::<PinnedTolerations>(serde_json::json!([{"key": "k", "value": "v", "effect": "Bogus"}])).is_err());
        assert!(
            serde_json::from_value::<PinnedTolerations>(serde_json::json!([{"key": "k", "value": "v", "operator": "Sometimes"}])).is_err()
        );
        // A non-array body is not a toleration list.
        assert!(serde_json::from_value::<PinnedTolerations>(serde_json::json!({"key": "k"})).is_err());
    }

    #[test]
    fn presets_resolve_by_id_and_default_is_unpinned() {
        let config = SchedulingConfig {
            toleration_presets: vec![
                SchedulingConfig::default_preset(),
                TolerationPreset {
                    id: "gpu-only".to_string(),
                    label: "GPU only".to_string(),
                    description: "Pin to GPU capacity.".to_string(),
                    tolerations: vec![SchedulingToleration {
                        key: "gpu".to_string(),
                        operator: Some(SchedulingOperator::Exists),
                        value: None,
                        effect: None,
                    }],
                },
            ],
        };
        let presets = TolerationPresets::new(&config);
        // Default: nothing pinned.
        assert!(presets.resolve(None).is_none());
        // The empty-list preset resolves to a real, empty list.
        assert_eq!(presets.resolve(Some("no-overflow")).as_deref(), Some(&serde_json::json!([])));
        // A custom preset resolves to its exact list.
        assert_eq!(
            presets.resolve(Some("gpu-only")).as_deref(),
            Some(&serde_json::json!([{"key": "gpu", "operator": "Exists"}]))
        );
    }

    #[test]
    fn an_unconfigured_id_fails_closed_to_the_empty_list() {
        let presets = TolerationPresets::new(&SchedulingConfig::default());
        // A preset an operator has dropped from config still resolves, to the
        // most restrictive option, rather than leaving the account unpinned.
        assert_eq!(presets.resolve(Some("retired")).as_deref(), Some(&serde_json::json!([])));
    }

    #[test]
    fn a_repeated_unconfigured_id_logs_its_diagnostic_once() {
        let presets = TolerationPresets::new(&SchedulingConfig::default());
        // First sight of the id is logged; a repeat is not, so a retired id a
        // hot request path references does not flood the log. The fail-closed
        // value is returned either way.
        assert!(presets.mark_unknown_reported("retired"));
        assert!(!presets.mark_unknown_reported("retired"));
        // A different id is still reported once.
        assert!(presets.mark_unknown_reported("also-retired"));
        assert_eq!(presets.resolve(Some("retired")).as_deref(), Some(&serde_json::json!([])));
    }

    #[test]
    fn preset_config_validation_rejects_duplicates_and_bad_shape() {
        let duplicate = SchedulingConfig {
            toleration_presets: vec![SchedulingConfig::default_preset(), SchedulingConfig::default_preset()],
        };
        assert!(duplicate.validate().unwrap_err().contains("duplicate id"));

        let empty_id = SchedulingConfig {
            toleration_presets: vec![TolerationPreset {
                id: "  ".to_string(),
                label: "x".to_string(),
                description: "y".to_string(),
                tolerations: vec![],
            }],
        };
        assert!(empty_id.validate().unwrap_err().contains("empty id"));

        // The dashboard's "Default" sentinel is reserved: a preset using it
        // would render duplicate select options and be unselectable.
        let sentinel = SchedulingConfig {
            toleration_presets: vec![TolerationPreset {
                id: DEFAULT_PRESET_SENTINEL.to_string(),
                label: "Collides".to_string(),
                description: "y".to_string(),
                tolerations: vec![],
            }],
        };
        assert!(sentinel.validate().unwrap_err().contains("reserved"));

        // An `Equal` toleration with no value would not serialise into a valid
        // body, so it is refused at load.
        let bad_shape = SchedulingConfig {
            toleration_presets: vec![TolerationPreset {
                id: "bad".to_string(),
                label: "Bad".to_string(),
                description: "Bad shape.".to_string(),
                tolerations: vec![SchedulingToleration {
                    key: "dedicated".to_string(),
                    operator: None,
                    value: None,
                    effect: None,
                }],
            }],
        };
        assert!(bad_shape.validate().unwrap_err().contains("is invalid"));

        // The shipped default is valid.
        SchedulingConfig::default().validate().unwrap();
    }
}
