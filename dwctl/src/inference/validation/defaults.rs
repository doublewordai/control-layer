//! Product-level default output cap.
//!
//! A request that sets no output cap is given one, so it ends at the same
//! point whichever backend serves it. Without one, a primary engine fills the
//! omitted cap with the rest of the context window, while an external provider
//! applies its own default (anywhere from a few thousand tokens to most of the
//! window), so the same request could stop at different lengths depending on
//! where it was placed.
//!
//! The default is ours, not the client's, so it is shrunk to fit: a request
//! whose prompt leaves less room than the default is given the room that is
//! left instead. Fitting needs the prompt's size. A prompt whose text bytes,
//! plus [`DefaultsConfig::prompt_overhead_tokens`] for the chat template, fit
//! beside the default needs no count (byte-level BPE never produces more
//! tokens than bytes). A larger one is counted exactly by tokenizer-svc when
//! [`DefaultsConfig::exact_count_enabled`] is on. A request whose fit cannot be
//! established (no context window in the catalog, media input, a prompt that
//! cannot be counted) is left without a cap, as before.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::exact::{ExactCounter, ExactOutcome};
use super::stage::ExactCountBudget;
use super::{ModelInfoSource, ModelLookup, Surface, view};

const METRIC: &str = "dwctl_default_max_tokens_total";

/// Most exact counts one batch file upload may spend; past it, remaining
/// lines too large to bound by bytes are left without a cap.
const BATCH_FILE_EXACT_COUNTS: usize = 1_000;

/// `request_defaults` in the config.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct DefaultsConfig {
    /// Master switch. Off, no request is given a cap and the catalog's
    /// `default_max_tokens` is ignored.
    pub enabled: bool,
    /// Output cap given to a request that sets none, unless the model's
    /// catalog metadata sets `default_max_tokens`. `null` turns the default
    /// off for models without their own.
    pub max_tokens: Option<u64>,
    /// Tokens added to a prompt's byte count to cover what the chat template
    /// adds (role markers, special tokens, tool preamble) when deciding
    /// without a count whether the default fits.
    pub prompt_overhead_tokens: u64,
    /// Tokens left unused when the default is shrunk to an exactly counted
    /// prompt: tokenizer-svc counts only the message text for a model without
    /// a chat template, and some engines reserve a few tokens of the window.
    pub fit_slack_tokens: u64,
    /// Count a prompt too large to bound by its bytes with tokenizer-svc
    /// (under `request_validation.exact_count_deadline_ms`) and shrink the
    /// default to fit. Off, such a request is left without a cap. Needs
    /// `cache.tokenizer_url`.
    pub exact_count_enabled: bool,
}

impl Default for DefaultsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_tokens: Some(65_536),
            prompt_overhead_tokens: 2_048,
            fit_slack_tokens: 256,
            exact_count_enabled: false,
        }
    }
}

/// Fills an omitted output cap. Cheap to clone.
#[derive(Clone)]
pub struct OutputCapDefaults {
    config: Arc<DefaultsConfig>,
    models: Arc<dyn ModelInfoSource>,
    /// `None`: a prompt too large to bound by its bytes is left without a cap.
    exact: Option<ExactCounter>,
}

/// What [`OutputCapDefaults::apply`] did, recorded as the metric's `outcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The client set a cap (or the surface needs none).
    ClientCapped,
    /// The default was set in full.
    Applied,
    /// The default was shrunk to the room the prompt leaves.
    Fitted,
    /// No default for this model, or no catalog entry.
    NoDefault,
    /// The model's context window is unknown, so the fit cannot be checked.
    NoContextWindow,
    /// Media input, which the byte bound does not cover.
    Media,
    /// The prompt was too large to bound by bytes and could not be counted.
    Uncounted,
    /// The prompt fills the context window: the engine refuses it anyway.
    NoRoom,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClientCapped => "client_capped",
            Self::Applied => "applied",
            Self::Fitted => "fitted",
            Self::NoDefault => "no_default",
            Self::NoContextWindow => "no_context_window",
            Self::Media => "media",
            Self::Uncounted => "uncounted",
            Self::NoRoom => "no_room",
        }
    }

    pub fn changed(self) -> bool {
        matches!(self, Self::Applied | Self::Fitted)
    }
}

/// The field a surface's output cap is written to. Messages requires
/// `max_tokens` from the client, and embeddings produce no output.
fn cap_field(surface: Surface) -> Option<&'static str> {
    match surface {
        Surface::ChatCompletions | Surface::Completions => Some("max_tokens"),
        Surface::Responses => Some("max_output_tokens"),
        Surface::Messages | Surface::Embeddings => None,
    }
}

impl OutputCapDefaults {
    pub fn new(config: DefaultsConfig, models: Arc<dyn ModelInfoSource>, exact: Option<ExactCounter>) -> Self {
        metrics::describe_counter!(
            METRIC,
            "Requests without an output cap, by what the product default did (applied, fitted, or why it was not set)"
        );
        Self {
            config: Arc::new(config),
            models,
            exact,
        }
    }

    /// A fresh exact-count budget for one batch file upload, whose lines are
    /// given the default one after another.
    pub fn batch_file_budget(&self) -> ExactCountBudget {
        ExactCountBudget::new(BATCH_FILE_EXACT_COUNTS)
    }

    /// Give `body` the default output cap when it sets none. `budget`, when
    /// given, caps the exact counts this call may draw on (batch uploads).
    pub async fn apply(&self, surface: Surface, body: &mut Value, budget: Option<&ExactCountBudget>) -> Outcome {
        let outcome = self.decide(surface, body, budget).await;
        if let (Some(cap), Some(field)) = (outcome.1, cap_field(surface))
            && let Some(object) = body.as_object_mut()
        {
            // `null` means "not set": drop the other chat field so only ours is read.
            if surface == Surface::ChatCompletions {
                object.remove("max_completion_tokens");
            }
            object.insert(field.to_string(), Value::from(cap));
        }
        if outcome.0 != Outcome::ClientCapped {
            metrics::counter!(METRIC, "surface" => surface.as_str(), "outcome" => outcome.0.as_str()).increment(1);
        }
        outcome.0
    }

    async fn decide(&self, surface: Surface, body: &Value, budget: Option<&ExactCountBudget>) -> (Outcome, Option<u64>) {
        if cap_field(surface).is_none() {
            return (Outcome::ClientCapped, None);
        }
        let view = view::extract(surface, body);
        if view.max_output_tokens.as_ref().is_some_and(|value| !value.is_null()) {
            return (Outcome::ClientCapped, None);
        }
        let Some(alias) = view.model.as_deref() else {
            return (Outcome::NoDefault, None);
        };
        let ModelLookup::Known(info) = self.models.lookup(alias) else {
            return (Outcome::NoDefault, None);
        };
        let Some(mut cap) = info.default_max_tokens.or(self.config.max_tokens) else {
            return (Outcome::NoDefault, None);
        };
        if let Some(max_output) = info.max_output_tokens {
            cap = cap.min(max_output);
        }
        let Some(context_window) = info.context_window else {
            return (Outcome::NoContextWindow, None);
        };
        if view.has_image_input || view.has_audio_input || view.has_file_input {
            return (Outcome::Media, None);
        }

        let bound = (view.prompt_text_bytes as u64).saturating_add(self.config.prompt_overhead_tokens);
        if bound.saturating_add(cap) <= context_window {
            return (Outcome::Applied, Some(cap));
        }

        let Some(exact) = &self.exact else {
            return (Outcome::Uncounted, None);
        };
        if !budget.is_none_or(ExactCountBudget::try_take) {
            return (Outcome::Uncounted, None);
        }
        let ExactOutcome::Counted(prompt_tokens) = exact.prompt_tokens(alias, surface, body).await else {
            return (Outcome::Uncounted, None);
        };
        let room = context_window
            .saturating_sub(prompt_tokens)
            .saturating_sub(self.config.fit_slack_tokens);
        if room == 0 {
            (Outcome::NoRoom, None)
        } else if room < cap {
            (Outcome::Fitted, Some(room))
        } else {
            (Outcome::Applied, Some(cap))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::*;
    use crate::inference::validation::ModelInfo;

    struct Models(HashMap<String, Arc<ModelInfo>>);

    impl ModelInfoSource for Models {
        fn lookup(&self, alias: &str) -> ModelLookup {
            self.0.get(alias).cloned().map_or(ModelLookup::Unknown, ModelLookup::Known)
        }
    }

    fn defaults(models: &[(&str, ModelInfo)]) -> OutputCapDefaults {
        let models = models
            .iter()
            .map(|(alias, info)| (alias.to_string(), Arc::new(info.clone())))
            .collect();
        OutputCapDefaults::new(DefaultsConfig::default(), Arc::new(Models(models)), None)
    }

    fn window(context_window: u64) -> ModelInfo {
        ModelInfo {
            context_window: Some(context_window),
            ..ModelInfo::default()
        }
    }

    fn chat(model: &str, text: &str) -> Value {
        json!({"model": model, "messages": [{"role": "user", "content": text}]})
    }

    #[tokio::test]
    async fn uncapped_chat_gets_the_default() {
        let defaults = defaults(&[("m", window(1_048_576))]);
        let mut body = chat("m", "hello");
        assert_eq!(defaults.apply(Surface::ChatCompletions, &mut body, None).await, Outcome::Applied);
        assert_eq!(body["max_tokens"], 65_536);
    }

    #[tokio::test]
    async fn a_client_cap_is_left_alone() {
        let defaults = defaults(&[("m", window(1_048_576))]);
        for field in ["max_tokens", "max_completion_tokens"] {
            let mut body = chat("m", "hello");
            body[field] = json!(500_000);
            assert_eq!(
                defaults.apply(Surface::ChatCompletions, &mut body, None).await,
                Outcome::ClientCapped
            );
            assert_eq!(body[field], 500_000);
        }
        let mut body = json!({"model": "m", "input": "hello", "max_output_tokens": 10});
        assert_eq!(defaults.apply(Surface::Responses, &mut body, None).await, Outcome::ClientCapped);
    }

    #[tokio::test]
    async fn a_null_cap_counts_as_unset() {
        let defaults = defaults(&[("m", window(1_048_576))]);
        let mut body = chat("m", "hello");
        body["max_completion_tokens"] = Value::Null;
        assert_eq!(defaults.apply(Surface::ChatCompletions, &mut body, None).await, Outcome::Applied);
        assert_eq!(body["max_tokens"], 65_536);
        assert!(body.get("max_completion_tokens").is_none());
    }

    #[tokio::test]
    async fn responses_use_max_output_tokens() {
        let defaults = defaults(&[("m", window(1_048_576))]);
        let mut body = json!({"model": "m", "input": "hello"});
        assert_eq!(defaults.apply(Surface::Responses, &mut body, None).await, Outcome::Applied);
        assert_eq!(body["max_output_tokens"], 65_536);
    }

    #[tokio::test]
    async fn messages_and_embeddings_are_untouched() {
        let defaults = defaults(&[("m", window(1_048_576))]);
        let mut body = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
        assert_eq!(defaults.apply(Surface::Messages, &mut body, None).await, Outcome::ClientCapped);
        assert!(body.get("max_tokens").is_none());
        let mut body = json!({"model": "m", "input": "hi"});
        assert_eq!(defaults.apply(Surface::Embeddings, &mut body, None).await, Outcome::ClientCapped);
    }

    #[tokio::test]
    async fn the_model_override_and_output_limit_win() {
        let defaults = defaults(&[
            (
                "long",
                ModelInfo {
                    default_max_tokens: Some(262_144),
                    ..window(1_048_576)
                },
            ),
            (
                "short",
                ModelInfo {
                    max_output_tokens: Some(8_192),
                    ..window(131_072)
                },
            ),
        ]);
        let mut body = chat("long", "hello");
        defaults.apply(Surface::ChatCompletions, &mut body, None).await;
        assert_eq!(body["max_tokens"], 262_144);
        let mut body = chat("short", "hello");
        defaults.apply(Surface::ChatCompletions, &mut body, None).await;
        assert_eq!(body["max_tokens"], 8_192);
    }

    #[tokio::test]
    async fn a_prompt_too_large_to_bound_is_left_uncapped_without_a_counter() {
        // 70k bytes + overhead + 65,536 > 131,072, and nothing can count it.
        let defaults = defaults(&[("m", window(131_072))]);
        let mut body = chat("m", &"a".repeat(70_000));
        assert_eq!(defaults.apply(Surface::ChatCompletions, &mut body, None).await, Outcome::Uncounted);
        assert!(body.get("max_tokens").is_none());
    }

    #[tokio::test]
    async fn unknown_models_windowless_models_and_media_are_left_uncapped() {
        let defaults = defaults(&[("windowless", ModelInfo::default()), ("m", window(1_048_576))]);
        let mut body = chat("missing", "hello");
        assert_eq!(defaults.apply(Surface::ChatCompletions, &mut body, None).await, Outcome::NoDefault);
        let mut body = chat("windowless", "hello");
        assert_eq!(
            defaults.apply(Surface::ChatCompletions, &mut body, None).await,
            Outcome::NoContextWindow
        );
        let mut body = json!({"model": "m", "messages": [{"role": "user", "content": [
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
        ]}]});
        assert_eq!(defaults.apply(Surface::ChatCompletions, &mut body, None).await, Outcome::Media);
        assert!(body.get("max_tokens").is_none());
    }

    fn counted(models: &[(&str, ModelInfo)], base_url: String) -> OutputCapDefaults {
        let models = models
            .iter()
            .map(|(alias, info)| (alias.to_string(), Arc::new(info.clone())))
            .collect();
        let exact = ExactCounter::new(
            Arc::new(crate::prompt_cache::TokenizerClient::new(base_url)),
            std::time::Duration::from_secs(1),
        );
        OutputCapDefaults::new(DefaultsConfig::default(), Arc::new(Models(models)), Some(exact))
    }

    async fn tokenizer(total: u64) -> wiremock::MockServer {
        use wiremock::matchers::{method, path};
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(method("POST"))
            .and(path("/v1/render"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
                "virtual_model": "m",
                "tokenizer_version": "v1",
                "template_version": "t1",
                "total": total,
                "prefix_counts": []
            })))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn the_default_shrinks_to_the_room_a_counted_prompt_leaves() {
        let server = tokenizer(100_000).await;
        let defaults = counted(&[("m", window(131_072))], server.uri());
        let mut body = chat("m", &"a".repeat(110_000));
        assert_eq!(defaults.apply(Surface::ChatCompletions, &mut body, None).await, Outcome::Fitted);
        assert_eq!(body["max_tokens"], 131_072 - 100_000 - 256);
    }

    #[tokio::test]
    async fn a_counted_prompt_with_room_gets_the_whole_default() {
        // Many bytes per token: too large to bound by bytes, small once counted.
        let server = tokenizer(20_000).await;
        let defaults = counted(&[("m", window(131_072))], server.uri());
        let mut body = chat("m", &"a".repeat(100_000));
        assert_eq!(defaults.apply(Surface::ChatCompletions, &mut body, None).await, Outcome::Applied);
        assert_eq!(body["max_tokens"], 65_536);
    }

    #[tokio::test]
    async fn a_prompt_that_fills_the_window_is_left_for_the_engine_to_refuse() {
        let server = tokenizer(131_000).await;
        let defaults = counted(&[("m", window(131_072))], server.uri());
        let mut body = chat("m", &"a".repeat(140_000));
        assert_eq!(defaults.apply(Surface::ChatCompletions, &mut body, None).await, Outcome::NoRoom);
        assert!(body.get("max_tokens").is_none());
    }

    #[tokio::test]
    async fn a_spent_budget_leaves_the_prompt_uncounted() {
        let server = tokenizer(100_000).await;
        let defaults = counted(&[("m", window(131_072))], server.uri());
        let budget = ExactCountBudget::new(0);
        let mut body = chat("m", &"a".repeat(110_000));
        assert_eq!(
            defaults.apply(Surface::ChatCompletions, &mut body, Some(&budget)).await,
            Outcome::Uncounted
        );
    }

    #[tokio::test]
    async fn no_global_default_and_no_override_leaves_the_request_alone() {
        let models = HashMap::from([("m".to_string(), Arc::new(window(1_048_576)))]);
        let config = DefaultsConfig {
            max_tokens: None,
            ..DefaultsConfig::default()
        };
        let defaults = OutputCapDefaults::new(config, Arc::new(Models(models)), None);
        let mut body = chat("m", "hello");
        assert_eq!(defaults.apply(Surface::ChatCompletions, &mut body, None).await, Outcome::NoDefault);
        assert!(body.get("max_tokens").is_none());
    }
}
