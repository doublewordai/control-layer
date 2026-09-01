//! Administrative contract for provider-specific reasoning translation.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;
use utoipa::ToSchema;

/// Canonical OpenAI reasoning effort values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

/// Maps canonical effort values to one provider request path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ReasoningWrite {
    /// Absolute JSON pointer to a provider reasoning field.
    pub target_path: String,
    /// Provider value emitted for each supported canonical effort.
    pub values: BTreeMap<ReasoningEffort, Value>,
}

/// Maps every canonical effort to provider writes or an explicit rejection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ReasoningTranslation {
    /// Canonical efforts that this provider does not support.
    pub unsupported_efforts: BTreeSet<ReasoningEffort>,
    /// Provider request writes applied for every supported effort.
    pub writes: Vec<ReasoningWrite>,
}

/// Provider translations for OpenAI-compatible request surfaces.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ReasoningTranslationConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_completions: Option<ReasoningTranslation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responses: Option<ReasoningTranslation>,
}

/// Per-surface model behavior relative to its endpoint reasoning default.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "mode", content = "translation", rename_all = "snake_case")]
pub enum ReasoningSurfaceOverride {
    /// Use the endpoint translation for this surface.
    #[default]
    Inherit,
    /// Suppress translation while leaving the canonical OpenAI field untouched.
    Disabled,
    /// Replace the endpoint translation for this surface.
    Override(ReasoningTranslation),
}

/// Independent model overrides for the two OpenAI-compatible request surfaces.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ReasoningTranslationOverrides {
    #[serde(default)]
    pub chat_completions: ReasoningSurfaceOverride,
    #[serde(default)]
    pub responses: ReasoningSurfaceOverride,
}

/// Canonical efforts that every provider behind a model can accept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SupportedReasoningEfforts {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_completions: Option<Vec<ReasoningEffort>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub responses: Option<Vec<ReasoningEffort>>,
}

/// Effective reasoning mappings for every provider behind a model alias.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelReasoningPolicy {
    providers: Vec<Option<ReasoningTranslationConfig>>,
}

impl ReasoningTranslationConfig {
    /// Validate using the same implementation that applies mappings at runtime.
    pub fn validate(&self) -> Result<(), onwards::reasoning::ReasoningError> {
        onwards::reasoning::ReasoningTranslationConfig::from(self.clone()).validate()
    }
}

impl ModelReasoningPolicy {
    pub fn new(providers: Vec<Option<ReasoningTranslationConfig>>) -> Self {
        Self { providers }
    }

    /// Return capabilities only when support is known for every provider.
    pub fn supported_efforts(&self) -> Option<SupportedReasoningEfforts> {
        let chat_completions = self.intersect_surface(|config| config.chat_completions.as_ref());
        let responses = self.intersect_surface(|config| config.responses.as_ref());

        if chat_completions.is_none() && responses.is_none() {
            None
        } else {
            Some(SupportedReasoningEfforts {
                chat_completions,
                responses,
            })
        }
    }

    /// Validate canonical controls and then every configured provider, exactly
    /// as the realtime Onwards request path does before selecting a provider.
    pub fn validate_request(&self, path: &str, body: &Value) -> Result<(), onwards::reasoning::ReasoningError> {
        let Some(request) = onwards::reasoning::validate_canonical_reasoning(path, body)? else {
            return Ok(());
        };

        for config in self.providers.iter().flatten() {
            onwards::reasoning::ReasoningTranslationConfig::from(config.clone()).validate_request(path, &request)?;
        }

        Ok(())
    }

    fn intersect_surface<'a>(
        &'a self,
        surface: impl Fn(&'a ReasoningTranslationConfig) -> Option<&'a ReasoningTranslation>,
    ) -> Option<Vec<ReasoningEffort>> {
        let mut providers = self.providers.iter();
        let first = providers.next()?.as_ref()?;
        let mut supported: BTreeSet<_> = surface(first)?.writes.first()?.values.keys().copied().collect();

        for provider in providers {
            let translation = surface(provider.as_ref()?)?;
            let provider_efforts: BTreeSet<_> = translation.writes.first()?.values.keys().copied().collect();
            supported.retain(|effort| provider_efforts.contains(effort));
        }

        Some(supported.into_iter().collect())
    }
}

impl ReasoningTranslationOverrides {
    /// Validate every replacement using Onwards' surface-specific contract.
    pub fn validate(&self) -> Result<(), onwards::reasoning::ReasoningError> {
        if let ReasoningSurfaceOverride::Override(translation) = &self.chat_completions {
            ReasoningTranslationConfig {
                chat_completions: Some(translation.clone()),
                responses: None,
            }
            .validate()?;
        }
        if let ReasoningSurfaceOverride::Override(translation) = &self.responses {
            ReasoningTranslationConfig {
                chat_completions: None,
                responses: Some(translation.clone()),
            }
            .validate()?;
        }
        Ok(())
    }

    /// Resolve both surfaces independently against an optional endpoint default.
    pub fn resolve(&self, endpoint_default: Option<&ReasoningTranslationConfig>) -> Option<ReasoningTranslationConfig> {
        let chat_completions = resolve_surface(
            &self.chat_completions,
            endpoint_default.and_then(|config| config.chat_completions.as_ref()),
        );
        let responses = resolve_surface(&self.responses, endpoint_default.and_then(|config| config.responses.as_ref()));

        if chat_completions.is_none() && responses.is_none() {
            None
        } else {
            Some(ReasoningTranslationConfig {
                chat_completions,
                responses,
            })
        }
    }
}

fn resolve_surface(
    model_override: &ReasoningSurfaceOverride,
    endpoint_default: Option<&ReasoningTranslation>,
) -> Option<ReasoningTranslation> {
    match model_override {
        ReasoningSurfaceOverride::Inherit => endpoint_default.cloned(),
        ReasoningSurfaceOverride::Disabled => None,
        ReasoningSurfaceOverride::Override(translation) => Some(translation.clone()),
    }
}

fn parse_endpoint_reasoning_translation(value: Option<Value>, model_alias: &str) -> Option<ReasoningTranslationConfig> {
    let value = value?;
    match serde_json::from_value::<ReasoningTranslationConfig>(value) {
        Ok(config) => match config.validate() {
            Ok(()) => Some(config),
            Err(error) => {
                warn!(model_alias, %error, "ignoring invalid endpoint reasoning translation");
                None
            }
        },
        Err(error) => {
            warn!(model_alias, %error, "ignoring malformed endpoint reasoning translation");
            None
        }
    }
}

fn parse_model_reasoning_overrides(value: Option<Value>, model_alias: &str) -> ReasoningTranslationOverrides {
    let Some(value) = value else {
        return ReasoningTranslationOverrides::default();
    };
    match serde_json::from_value::<ReasoningTranslationOverrides>(value) {
        Ok(overrides) => match overrides.validate() {
            Ok(()) => overrides,
            Err(error) => {
                warn!(model_alias, %error, "ignoring invalid model reasoning translation overrides");
                ReasoningTranslationOverrides::default()
            }
        },
        Err(error) => {
            warn!(model_alias, %error, "ignoring malformed model reasoning translation overrides");
            ReasoningTranslationOverrides::default()
        }
    }
}

/// Resolve raw database JSON using the same fallback behavior as the Onwards sync.
pub(crate) fn resolve_reasoning_translation(
    endpoint_value: Option<Value>,
    model_value: Option<Value>,
    model_alias: &str,
) -> Option<ReasoningTranslationConfig> {
    let endpoint_default = parse_endpoint_reasoning_translation(endpoint_value, model_alias);
    parse_model_reasoning_overrides(model_value, model_alias).resolve(endpoint_default.as_ref())
}

impl From<ReasoningEffort> for onwards::reasoning::ReasoningEffort {
    fn from(value: ReasoningEffort) -> Self {
        match value {
            ReasoningEffort::None => Self::None,
            ReasoningEffort::Minimal => Self::Minimal,
            ReasoningEffort::Low => Self::Low,
            ReasoningEffort::Medium => Self::Medium,
            ReasoningEffort::High => Self::High,
            ReasoningEffort::Xhigh => Self::Xhigh,
            ReasoningEffort::Max => Self::Max,
        }
    }
}

impl From<ReasoningWrite> for onwards::reasoning::ReasoningWrite {
    fn from(value: ReasoningWrite) -> Self {
        Self {
            target_path: value.target_path,
            values: value.values.into_iter().map(|(effort, value)| (effort.into(), value)).collect(),
        }
    }
}

impl From<ReasoningTranslation> for onwards::reasoning::ReasoningTranslation {
    fn from(value: ReasoningTranslation) -> Self {
        Self {
            unsupported_efforts: value.unsupported_efforts.into_iter().map(Into::into).collect(),
            writes: value.writes.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<ReasoningTranslationConfig> for onwards::reasoning::ReasoningTranslationConfig {
    fn from(value: ReasoningTranslationConfig) -> Self {
        Self {
            chat_completions: value.chat_completions.map(Into::into),
            responses: value.responses.map(Into::into),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::{BTreeMap, BTreeSet};

    fn native_translation() -> ReasoningTranslation {
        ReasoningTranslation {
            unsupported_efforts: BTreeSet::new(),
            writes: vec![ReasoningWrite {
                target_path: "/reasoning_effort".to_string(),
                values: BTreeMap::from([
                    (ReasoningEffort::None, json!("none")),
                    (ReasoningEffort::Minimal, json!("minimal")),
                    (ReasoningEffort::Low, json!("low")),
                    (ReasoningEffort::Medium, json!("medium")),
                    (ReasoningEffort::High, json!("high")),
                    (ReasoningEffort::Xhigh, json!("xhigh")),
                    (ReasoningEffort::Max, json!("max")),
                ]),
            }],
        }
    }

    #[test]
    fn accepts_native_reasoning_translation() {
        let config = ReasoningTranslationConfig {
            chat_completions: Some(native_translation()),
            responses: None,
        };

        config.validate().unwrap();

        let onwards_config = onwards::reasoning::ReasoningTranslationConfig::from(config);
        let write = &onwards_config.chat_completions.unwrap().writes[0];
        assert_eq!(write.target_path, "/reasoning_effort");
        assert_eq!(write.values.len(), 7);
    }

    #[test]
    fn accepts_two_write_token_budget_translation() {
        let config: ReasoningTranslationConfig = serde_json::from_value(json!({
            "chat_completions": {
                "unsupported_efforts": ["none", "minimal", "low", "medium", "xhigh", "max"],
                "writes": [
                    {
                        "target_path": "/reasoning_effort",
                        "values": {"high": "high"}
                    },
                    {
                        "target_path": "/thinking_token_budget",
                        "values": {"high": 8192}
                    }
                ]
            }
        }))
        .unwrap();

        config.validate().unwrap();

        let onwards_config = onwards::reasoning::ReasoningTranslationConfig::from(config);
        assert_eq!(onwards_config.chat_completions.unwrap().writes.len(), 2);
    }

    #[test]
    fn rejects_incomplete_effort_accounting() {
        let config: ReasoningTranslationConfig = serde_json::from_value(json!({
            "chat_completions": {
                "unsupported_efforts": ["minimal", "low", "medium", "high", "xhigh"],
                "writes": [{
                    "target_path": "/reasoning_effort",
                    "values": {"none": "none"}
                }]
            }
        }))
        .unwrap();

        let error = config.validate().unwrap_err();

        assert!(error.to_string().contains("every OpenAI reasoning effort"));
    }

    #[test]
    fn rejects_overlapping_mapped_and_unsupported_efforts() {
        let config: ReasoningTranslationConfig = serde_json::from_value(json!({
            "chat_completions": {
                "unsupported_efforts": ["none", "minimal", "low", "medium", "high", "xhigh", "max"],
                "writes": [{
                    "target_path": "/reasoning_effort",
                    "values": {"none": "none"}
                }]
            }
        }))
        .unwrap();

        let error = config.validate().unwrap_err();

        assert!(error.to_string().contains("must not overlap"));
    }

    #[test]
    fn missing_override_surfaces_default_to_inherit() {
        let overrides: ReasoningTranslationOverrides = serde_json::from_value(json!({})).unwrap();

        assert_eq!(overrides.chat_completions, ReasoningSurfaceOverride::Inherit);
        assert_eq!(overrides.responses, ReasoningSurfaceOverride::Inherit);
        assert_eq!(
            serde_json::to_value(ReasoningSurfaceOverride::Override(native_translation())).unwrap()["mode"],
            "override"
        );
    }

    #[test]
    fn resolves_each_override_surface_independently() {
        let endpoint_chat = native_translation();
        let endpoint_responses = native_translation();
        let replacement_chat: ReasoningTranslation = serde_json::from_value(json!({
            "unsupported_efforts": ["minimal", "xhigh", "max"],
            "writes": [{
                "target_path": "/chat_template_kwargs/thinking",
                "values": {"none": false, "low": true, "medium": true, "high": true}
            }]
        }))
        .unwrap();
        let defaults = ReasoningTranslationConfig {
            chat_completions: Some(endpoint_chat),
            responses: Some(endpoint_responses.clone()),
        };
        let overrides = ReasoningTranslationOverrides {
            chat_completions: ReasoningSurfaceOverride::Override(replacement_chat.clone()),
            responses: ReasoningSurfaceOverride::Inherit,
        };

        let resolved = overrides.resolve(Some(&defaults)).unwrap();

        assert_eq!(resolved.chat_completions, Some(replacement_chat));
        assert_eq!(resolved.responses, Some(endpoint_responses));
    }

    #[test]
    fn disabled_surface_does_not_remove_inherited_sibling() {
        let endpoint_responses = native_translation();
        let defaults = ReasoningTranslationConfig {
            chat_completions: Some(native_translation()),
            responses: Some(endpoint_responses.clone()),
        };
        let overrides = ReasoningTranslationOverrides {
            chat_completions: ReasoningSurfaceOverride::Disabled,
            responses: ReasoningSurfaceOverride::Inherit,
        };

        let resolved = overrides.resolve(Some(&defaults)).unwrap();

        assert_eq!(resolved.chat_completions, None);
        assert_eq!(resolved.responses, Some(endpoint_responses));
    }

    #[test]
    fn both_disabled_surfaces_collapse_to_none() {
        let defaults = ReasoningTranslationConfig {
            chat_completions: Some(native_translation()),
            responses: Some(native_translation()),
        };
        let overrides = ReasoningTranslationOverrides {
            chat_completions: ReasoningSurfaceOverride::Disabled,
            responses: ReasoningSurfaceOverride::Disabled,
        };

        assert_eq!(overrides.resolve(Some(&defaults)), None);
    }

    #[test]
    fn model_policy_intersects_supported_efforts_across_providers() {
        let first: ReasoningTranslationConfig = serde_json::from_value(json!({
            "chat_completions": {
                "unsupported_efforts": ["minimal", "xhigh", "max"],
                "writes": [{
                    "target_path": "/reasoning_effort",
                    "values": {"none": "none", "low": "low", "medium": "medium", "high": "high"}
                }]
            }
        }))
        .unwrap();
        let second: ReasoningTranslationConfig = serde_json::from_value(json!({
            "chat_completions": {
                "unsupported_efforts": ["none", "minimal", "low", "xhigh", "max"],
                "writes": [{
                    "target_path": "/thinking",
                    "values": {"medium": {"type": "enabled"}, "high": {"type": "enabled"}}
                }]
            }
        }))
        .unwrap();
        let policy = ModelReasoningPolicy::new(vec![Some(first), Some(second)]);

        let supported = policy.supported_efforts().unwrap();

        assert_eq!(
            supported.chat_completions,
            Some(vec![ReasoningEffort::Medium, ReasoningEffort::High])
        );
        assert_eq!(supported.responses, None);
    }

    #[test]
    fn model_policy_omits_indeterminate_surfaces() {
        let policy = ModelReasoningPolicy::new(vec![
            Some(ReasoningTranslationConfig {
                chat_completions: Some(native_translation()),
                responses: None,
            }),
            None,
        ]);

        assert_eq!(policy.supported_efforts(), None);
    }

    #[test]
    fn model_policy_uses_onwards_canonical_validation() {
        let policy = ModelReasoningPolicy::default();
        let body = json!({"thinking": {"type": "enabled"}});

        let error = policy.validate_request("/v1/chat/completions", &body).unwrap_err();

        assert_eq!(error.status_code(), 400);
        assert_eq!(error.param(), Some("thinking"));
        assert!(error.message().contains("use 'reasoning_effort'"));
    }

    #[test]
    fn model_policy_validates_every_provider_mapping() {
        let supported = ReasoningTranslationConfig {
            chat_completions: Some(native_translation()),
            responses: None,
        };
        let unsupported: ReasoningTranslationConfig = serde_json::from_value(json!({
            "chat_completions": {
                "unsupported_efforts": ["none", "minimal", "low", "high", "xhigh", "max"],
                "writes": [{
                    "target_path": "/reasoning_effort",
                    "values": {"medium": "medium"}
                }]
            }
        }))
        .unwrap();
        let policy = ModelReasoningPolicy::new(vec![Some(supported), Some(unsupported)]);

        let error = policy
            .validate_request("/v1/chat/completions", &json!({"reasoning_effort": "high"}))
            .unwrap_err();

        assert_eq!(error.status_code(), 400);
        assert!(error.message().contains("Reasoning effort 'high' is not supported"));
    }

    #[test]
    fn model_policy_preserves_provider_default_when_effort_is_omitted() {
        let policy = ModelReasoningPolicy::new(vec![Some(ReasoningTranslationConfig {
            chat_completions: Some(native_translation()),
            responses: None,
        })]);

        policy.validate_request("/v1/chat/completions", &json!({"messages": []})).unwrap();
    }
}
