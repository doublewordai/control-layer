//! Administrative contract for provider-specific reasoning translation.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
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

impl ReasoningTranslationConfig {
    /// Validate using the same implementation that applies mappings at runtime.
    pub fn validate(&self) -> Result<(), onwards::reasoning::ReasoningError> {
        onwards::reasoning::ReasoningTranslationConfig::from(self.clone()).validate()
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
}
