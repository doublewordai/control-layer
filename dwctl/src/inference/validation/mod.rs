//! Ingress request validation: reject inference requests that are provably
//! doomed before they reach an engine or the flex queue.
//!
//! This module is the single home for request rules. The inference
//! middleware parses the body once, builds a [`RequestView`], looks the model up
//! in a [`ModelInfoSource`], and asks [`evaluate`] for a verdict. Rejections are
//! rendered by [`envelope::rejection_response`] in the surface's own error shape.
//!
//! Guardrails:
//! - **Precision over recall.** A rule fires only on provable doom and passes
//!   when metadata is missing ([`ModelLookup::NotLoaded`], `None` fields).
//! - **Shadow before enforce.** Each rule has a [`RuleMode`]; `Shadow` records a
//!   would-have-rejected metric and forwards the request unchanged.
//! - **Lossless.** Unknown request fields are never a reason to reject.
//!
//! Layout:
//! - [`view`]: surface-aware extraction of the fields rules need.
//! - [`rules`]: the rules themselves and [`evaluate`].
//! - [`envelope`]: OpenAI / Anthropic rejection bodies.
//! - [`exact`]: stage-2 exact prompt token count via tokenizer-svc.
//! - [`stage`]: [`ValidationStage`], the one entry point the middleware calls.

use std::collections::HashMap;
use std::sync::Arc;

use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::db::models::deployments::ModelType;

pub mod envelope;
pub mod exact;
pub mod rules;
pub mod stage;
pub mod view;

pub use rules::evaluate;
pub use stage::{ExactCountBudget, Source, ValidationStage};
pub use view::RequestView;

/// The inference API surface a request arrived on. Paths are the nested
/// (`/ai/v1`-relative) paths seen by the inference middleware.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Surface {
    ChatCompletions,
    Completions,
    Responses,
    Messages,
    Embeddings,
}

impl Surface {
    pub fn from_path(path: &str) -> Option<Self> {
        if path.ends_with("/chat/completions") {
            Some(Self::ChatCompletions)
        } else if path.ends_with("/completions") {
            Some(Self::Completions)
        } else if path.ends_with("/responses") {
            Some(Self::Responses)
        } else if path.ends_with("/messages") {
            Some(Self::Messages)
        } else if path.ends_with("/embeddings") {
            Some(Self::Embeddings)
        } else {
            None
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat_completions",
            Self::Completions => "completions",
            Self::Responses => "responses",
            Self::Messages => "messages",
            Self::Embeddings => "embeddings",
        }
    }

    /// Name of the field holding the prompt, for `param` on prompt-shape errors.
    pub fn prompt_param(self) -> &'static str {
        match self {
            Self::ChatCompletions | Self::Messages => "messages",
            Self::Completions => "prompt",
            Self::Responses | Self::Embeddings => "input",
        }
    }

    /// Model type this surface serves. `None` = no constraint.
    pub fn expected_model_type(self) -> Option<ModelType> {
        match self {
            Self::ChatCompletions | Self::Completions | Self::Responses | Self::Messages => Some(ModelType::Chat),
            Self::Embeddings => Some(ModelType::Embeddings),
        }
    }
}

/// Per-model facts the rules need, keyed by the bare model alias.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelInfo {
    pub model_type: Option<ModelType>,
    /// Maximum prompt + completion tokens the serving engine accepts.
    pub context_window: Option<u64>,
    /// Maximum completion tokens the model may be asked for.
    pub max_output_tokens: Option<u64>,
    /// Catalog capabilities (`vision`, `reasoning`, ...). `None` = unknown,
    /// which disables capability-based rules for the model.
    pub capabilities: Option<Vec<String>>,
}

impl ModelInfo {
    pub fn has_capability(&self, capability: &str) -> Option<bool> {
        self.capabilities
            .as_ref()
            .map(|caps| caps.iter().any(|c| c.eq_ignore_ascii_case(capability)))
    }
}

/// Result of looking a model alias up.
#[derive(Debug, Clone)]
pub enum ModelLookup {
    Known(Arc<ModelInfo>),
    /// The metadata is loaded and no deployed model has this alias.
    Unknown,
    /// Metadata is unavailable (cold start, sync disabled). Every rule passes.
    NotLoaded,
}

/// Source of [`ModelInfo`], read on the request hot path. Implementations must
/// not block or do I/O ([`crate::sync::model_metadata::ModelMetadataCache`]).
pub trait ModelInfoSource: Send + Sync {
    fn lookup(&self, alias: &str) -> ModelLookup;
}

/// Stable rule identifiers, used as metric labels and config keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleId {
    ModelTypeMismatch,
    InvalidServiceTier,
    InvalidMaxTokens,
    MaxTokensExceedsLimit,
    UnsupportedModality,
    ContextLengthExceeded,
}

impl RuleId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ModelTypeMismatch => "model_type_mismatch",
            Self::InvalidServiceTier => "invalid_service_tier",
            Self::InvalidMaxTokens => "invalid_max_tokens",
            Self::MaxTokensExceedsLimit => "max_tokens_exceeds_limit",
            Self::UnsupportedModality => "unsupported_modality",
            Self::ContextLengthExceeded => "context_length_exceeded",
        }
    }
}

/// A single proven reason the request cannot succeed.
#[derive(Debug, Clone, PartialEq)]
pub struct Violation {
    pub rule: RuleId,
    pub status: StatusCode,
    /// OpenAI-style machine code, e.g. `context_length_exceeded`.
    pub code: &'static str,
    pub param: Option<&'static str>,
    /// Client-facing, actionable message.
    pub message: String,
}

/// Stage-1 context check could not decide; an exact count is needed.
#[derive(Debug, Clone, PartialEq)]
pub struct ExactCountNeeded {
    /// Token budget the prompt must fit in. Currently the whole context
    /// window: engines disagree on whether `prompt + max_tokens` must fit, so
    /// subtracting the requested output would reject requests some accept.
    pub prompt_token_limit: u64,
    pub context_window: u64,
}

/// Outcome of the synchronous (stage-1) rules.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Evaluation {
    /// Violations in rule order; the first enforced one is returned to the client.
    pub violations: Vec<Violation>,
    pub exact_count: Option<ExactCountNeeded>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RuleMode {
    Off,
    #[default]
    Shadow,
    Enforce,
}

impl RuleMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shadow => "shadow",
            Self::Enforce => "enforce",
        }
    }
}

/// `request_validation` config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ValidationConfig {
    /// Master switch. Off = the middleware never builds a view.
    pub enabled: bool,
    /// Mode for rules without an entry in `rules`.
    pub default_mode: RuleMode,
    /// Per-rule override, keyed by [`RuleId::as_str`].
    pub rules: HashMap<RuleId, RuleMode>,
    /// Stage-2 exact counting (tokenizer-svc). Prompts larger in bytes than the
    /// context window are only ever rejected on an exact count, so with this
    /// off the context rule never rejects.
    pub exact_count_enabled: bool,
    /// Hard deadline for the stage-2 tokenizer call; on expiry the request
    /// passes. Only prompts larger in bytes than the context window are ever
    /// counted, so this bounds the cost of already-huge requests.
    pub exact_count_deadline_ms: u64,
    /// Most exact counts one batch file upload may spend (lines are validated
    /// one after another). Past it, remaining near-limit lines pass.
    pub exact_count_max_per_batch_file: usize,
}

impl Default for ValidationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            default_mode: RuleMode::Shadow,
            rules: HashMap::new(),
            exact_count_enabled: false,
            exact_count_deadline_ms: 2_000,
            exact_count_max_per_batch_file: 1_000,
        }
    }
}

impl ValidationConfig {
    pub fn mode(&self, rule: RuleId) -> RuleMode {
        self.rules.get(&rule).copied().unwrap_or(self.default_mode)
    }
}
