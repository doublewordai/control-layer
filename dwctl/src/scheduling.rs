//! Scheduling tolerations pinned to an account's inference requests.
//!
//! An operator can affix a fixed, Kubernetes-style toleration list to every
//! request from a given account. The list is written verbatim to
//! `nvext.routing_constraints.tolerations` on the request body before it
//! reaches the upstream inference backend, for realtime, flex and batch
//! traffic alike, so a backend that honours tolerations keeps the account's
//! work on capacity carrying a matching taint. A typical value is the empty
//! list, which asks a backend to keep the account off any tainted capacity.
//!
//! The pin lives on the account's `users` row (`users.pinned_tolerations`),
//! alongside the other account-wide flags (`zero_data_retention`,
//! `disabled_modalities`). `NULL` means "not pinned": the account is left
//! alone. An empty list (`[]`) is a real pin, distinct from `NULL`.
//!
//! This module owns the one canonical shape and the server-side validation.
//! The admin API parses `pinned_tolerations` through [`PinnedTolerations`], and
//! the value stored is exactly what `serde_json::to_value` produces here, so
//! the request path writes a body the client could have sent itself.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// A fixed list of scheduling tolerations pinned to an account's requests.
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
    /// A pin that forbids tainted capacity: the empty list.
    pub fn dedicated_capacity() -> Self {
        Self(Vec::new())
    }

    /// Whether this pin is the empty list — the "dedicated capacity" state.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Validate every entry before the list is stored. Rejects a toleration
    /// the request path could not serialise into a valid body, so a malformed
    /// pin fails the admin request rather than producing an invalid upstream
    /// request later.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(value: serde_json::Value) -> (PinnedTolerations, serde_json::Value) {
        let pinned: PinnedTolerations = serde_json::from_value(value.clone()).unwrap();
        (pinned, value)
    }

    #[test]
    fn an_empty_list_round_trips_as_dedicated_capacity() {
        let (pinned, _) = parse(serde_json::json!([]));
        assert!(pinned.is_empty());
        assert_eq!(pinned.to_json(), serde_json::json!([]));
        assert_eq!(PinnedTolerations::dedicated_capacity().to_json(), serde_json::json!([]));
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
}
