//! Administrative contract for provider-specific reasoning translation.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
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
pub struct ReasoningTranslation {
    /// Absolute JSON pointer to a provider reasoning field.
    pub target_path: String,
    /// Provider value emitted for each supported canonical effort.
    pub values: BTreeMap<ReasoningEffort, Value>,
}

/// Provider translations for OpenAI-compatible request surfaces.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ReasoningTranslationConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_completions: Option<ReasoningTranslation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responses: Option<ReasoningTranslation>,
}

impl ReasoningTranslationConfig {
    /// Validate using the same implementation that applies mappings at runtime.
    pub fn validate(&self) -> Result<(), onwards::reasoning::ReasoningError> {
        onwards::reasoning::ReasoningTranslationConfig::from(self.clone()).validate()
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

impl From<ReasoningTranslation> for onwards::reasoning::ReasoningTranslation {
    fn from(value: ReasoningTranslation) -> Self {
        Self {
            target_path: value.target_path,
            values: value.values.into_iter().map(|(effort, value)| (effort.into(), value)).collect(),
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
    use std::collections::BTreeMap;

    fn sglang_config() -> ReasoningTranslationConfig {
        ReasoningTranslationConfig {
            chat_completions: Some(ReasoningTranslation {
                target_path: "/chat_template_kwargs/thinking".to_string(),
                values: BTreeMap::from([(ReasoningEffort::None, json!(false)), (ReasoningEffort::Low, json!(true))]),
            }),
            responses: None,
        }
    }

    #[test]
    fn accepts_sglang_reasoning_translation() {
        assert!(sglang_config().validate().is_ok());
    }

    #[test]
    fn rejects_non_reasoning_target_paths() {
        let mut config = sglang_config();
        config.chat_completions.as_mut().unwrap().target_path = "/messages/0/content".to_string();

        let error = config.validate().unwrap_err();

        assert!(error.to_string().contains("target_path"));
    }

    #[test]
    fn rejects_unknown_effort_names_during_deserialization() {
        let error = serde_json::from_value::<ReasoningTranslationConfig>(json!({
            "chat_completions": {
                "target_path": "/thinking/type",
                "values": {"ultra": "enabled"}
            }
        }))
        .unwrap_err();

        assert!(error.to_string().contains("unknown variant"));
    }
}
