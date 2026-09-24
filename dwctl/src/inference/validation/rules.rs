//! Validation rules and [`evaluate`].
//!
//! Each rule is precision-first: it fires only when the request is *provably*
//! doomed. Missing metadata ([`ModelLookup::NotLoaded`], a `None` model field,
//! `None` capabilities/limits) disables the rules that depend on it rather than
//! guessing. Unknown request fields are never a reason to reject; the rules
//! only inspect the handful of fields [`RequestView`] exposes.
//!
//! `evaluate` returns every firing rule in a fixed order. It is mode-agnostic
//! beyond skipping [`RuleMode::Off`]: callers use [`first_enforced`] to decide
//! whether to reject, and record shadow-mode hits with [`record`].

use axum::http::StatusCode;
use metrics::{counter, describe_counter};
use serde_json::Value;

use super::{Evaluation, ExactCountNeeded, ModelInfo, ModelLookup, RequestView, RuleId, RuleMode, Surface, ValidationConfig, Violation};
use crate::db::models::deployments::ModelType;

/// `service_tier` values that dwctl routes specially (`flex`, `background`) or
/// that OpenAI accepts and dwctl passes through (`auto`, `default`, `priority`,
/// `scale`). Anything else is rejected rather than silently treated as realtime.
const ACCEPTED_SERVICE_TIERS: [&str; 6] = ["auto", "default", "flex", "priority", "scale", "background"];
const ACCEPTED_SERVICE_TIERS_LIST: &str = "auto, default, flex, priority, scale, background";
/// Anthropic Messages additionally defines `standard_only`.
const ACCEPTED_MESSAGES_SERVICE_TIERS: [&str; 7] = ["auto", "standard_only", "default", "flex", "priority", "scale", "background"];
const ACCEPTED_MESSAGES_SERVICE_TIERS_LIST: &str = "auto, standard_only, default, flex, priority, scale, background";

/// Register the rule metric with the recorder. Idempotent; callers may invoke it
/// at startup so rate-based alerts see a zero sample before the first rejection.
pub fn describe_metrics() {
    describe_counter!(
        "dwctl_request_validation_violations_total",
        "Inference requests a validation rule proved would fail, by rule, surface, source (request or batch_file), model alias and mode"
    );
}

/// Evaluate the synchronous (stage-1) rules in order and return every firing
/// violation. `Off` rules are skipped entirely - they contribute neither a
/// violation nor an [`Evaluation::exact_count`] request.
pub fn evaluate(view: &RequestView, model: &ModelLookup, config: &ValidationConfig) -> Evaluation {
    let mut evaluation = Evaluation::default();
    let info = match model {
        ModelLookup::Known(info) => Some(info.as_ref()),
        ModelLookup::Unknown | ModelLookup::NotLoaded => None,
    };

    if enabled(config, RuleId::ModelNotFound)
        && let Some(violation) = model_not_found(view, model)
    {
        evaluation.violations.push(violation);
    }

    if enabled(config, RuleId::ModelTypeMismatch)
        && let Some(info) = info
        && let Some(violation) = model_type_mismatch(view, info)
    {
        evaluation.violations.push(violation);
    }

    if enabled(config, RuleId::InvalidServiceTier)
        && let Some(violation) = invalid_service_tier(view)
    {
        evaluation.violations.push(violation);
    }

    // Parsed once: `MaxTokensExceedsLimit` only considers a well-formed value,
    // and `InvalidMaxTokens` reports the malformed ones.
    let max_tokens = view
        .max_output_tokens
        .as_ref()
        .and_then(|value| parse_max_tokens(value, view.surface));

    if enabled(config, RuleId::InvalidMaxTokens)
        && let Some(violation) = invalid_max_tokens(view)
    {
        evaluation.violations.push(violation);
    }

    if enabled(config, RuleId::MaxTokensExceedsLimit)
        && let (Some(info), Some(max_tokens)) = (info, max_tokens)
        && let Some(violation) = max_tokens_exceeds_limit(view, info, max_tokens)
    {
        evaluation.violations.push(violation);
    }

    if enabled(config, RuleId::UnsupportedModality)
        && let Some(info) = info
    {
        evaluation.violations.extend(unsupported_modality(view, info));
    }

    if enabled(config, RuleId::ContextLengthExceeded)
        && let Some(info) = info
        && let Some(window) = info.context_window
    {
        apply_context_length(view, window, &mut evaluation);
    }

    evaluation
}

/// The stage-2 verdict: `Some` when an exact prompt token count proves the
/// request over the limit. `counted` is engine/BPE tokens, not bytes.
pub fn exact_count_violation(surface: Surface, model: &str, counted: u64, needed: &ExactCountNeeded) -> Option<Violation> {
    if counted <= needed.prompt_token_limit {
        return None;
    }
    Some(Violation {
        rule: RuleId::ContextLengthExceeded,
        status: StatusCode::BAD_REQUEST,
        code: "context_length_exceeded",
        param: Some(surface.prompt_param()),
        message: format!(
            "This model `{model}` has a maximum context length of {} tokens; your request is {counted} tokens. Reduce the length of the input.",
            needed.context_window
        ),
    })
}

/// The first violation whose rule is set to [`RuleMode::Enforce`]. Rules in
/// `Shadow` (or `Off`) are recorded but never returned to the client.
pub fn first_enforced<'a>(violations: &'a [Violation], config: &ValidationConfig) -> Option<&'a Violation> {
    violations.iter().find(|violation| config.mode(violation.rule) == RuleMode::Enforce)
}

/// Emit `dwctl_request_validation_violations_total` for each violation. The
/// caller passes `model_label = "unknown"` for aliases missing from the catalog
/// so the `model` label stays bounded.
pub fn record(violations: &[Violation], surface: Surface, source: &'static str, model_label: &str, config: &ValidationConfig) {
    for violation in violations {
        counter!(
            "dwctl_request_validation_violations_total",
            "rule" => violation.rule.as_str(),
            "surface" => surface.as_str(),
            "source" => source,
            "model" => model_label.to_string(),
            "mode" => config.mode(violation.rule).as_str(),
        )
        .increment(1);
    }
}

fn enabled(config: &ValidationConfig, rule: RuleId) -> bool {
    config.mode(rule) != RuleMode::Off
}

/// (a) The alias is not deployed. `NotLoaded` (metadata unavailable) and a
/// missing alias both fail open.
fn model_not_found(view: &RequestView, model: &ModelLookup) -> Option<Violation> {
    if !matches!(model, ModelLookup::Unknown) {
        return None;
    }
    let alias = view.model.as_deref()?;
    Some(Violation {
        rule: RuleId::ModelNotFound,
        status: StatusCode::NOT_FOUND,
        code: "model_not_found",
        param: Some("model"),
        message: format!("The model `{alias}` does not exist or you do not have access to it."),
    })
}

/// (b) The catalog model type does not match what the surface serves. `None`
/// model type, `None` surface or a surface without a constraint all pass.
fn model_type_mismatch(view: &RequestView, info: &ModelInfo) -> Option<Violation> {
    let actual = info.model_type.clone()?;
    let surface = view.surface?;
    let expected = surface.expected_model_type()?;
    if actual == expected {
        return None;
    }
    let alias = view.model.as_deref().unwrap_or("(unknown)");
    Some(Violation {
        rule: RuleId::ModelTypeMismatch,
        status: StatusCode::BAD_REQUEST,
        code: "model_type_mismatch",
        param: Some("model"),
        message: format!(
            "The model `{alias}` is not usable on the {} endpoint: it is a {} model but this endpoint requires a {} model.",
            surface_label(surface),
            model_type_label(actual),
            model_type_label(expected)
        ),
    })
}

/// (c) `service_tier` is present, non-null, and not one of the accepted values.
fn invalid_service_tier(view: &RequestView) -> Option<Violation> {
    let value = view.service_tier.as_ref()?;
    if value.is_null() {
        return None;
    }
    let (accepted, list): (&[&str], &str) = match view.surface {
        Some(Surface::Messages) => (&ACCEPTED_MESSAGES_SERVICE_TIERS, ACCEPTED_MESSAGES_SERVICE_TIERS_LIST),
        _ => (&ACCEPTED_SERVICE_TIERS, ACCEPTED_SERVICE_TIERS_LIST),
    };
    let message = match value.as_str() {
        Some(tier) if accepted.contains(&tier) => return None,
        Some(tier) => format!("Invalid value '{tier}' for 'service_tier'. Expected one of: {list}."),
        None => format!("Invalid value for 'service_tier'. Expected one of: {list}."),
    };
    Some(Violation {
        rule: RuleId::InvalidServiceTier,
        status: StatusCode::BAD_REQUEST,
        code: "invalid_value",
        param: Some("service_tier"),
        message,
    })
}

/// (d) `max_output_tokens` is present, non-null, and not a positive integer.
/// Integral floats (`256.0`) are accepted as `256`.
fn invalid_max_tokens(view: &RequestView) -> Option<Violation> {
    let value = view.max_output_tokens.as_ref()?;
    if value.is_null() || parse_max_tokens(value, view.surface).is_some() {
        return None;
    }
    let param = view.max_output_tokens_param;
    let field = param.unwrap_or("max_output_tokens");
    Some(Violation {
        rule: RuleId::InvalidMaxTokens,
        status: StatusCode::BAD_REQUEST,
        code: "invalid_value",
        param,
        message: format!("Invalid value for '{field}'. Expected a positive integer."),
    })
}

/// (e) A well-formed max-token value larger than either catalog limit. The
/// output limit is reported first; a value over only the context window still
/// fires.
fn max_tokens_exceeds_limit(view: &RequestView, info: &ModelInfo, max_tokens: u64) -> Option<Violation> {
    let output = info.max_output_tokens.filter(|limit| max_tokens > *limit);
    let (limit, kind) = match output {
        Some(limit) => (limit, "output"),
        None => (info.context_window.filter(|limit| max_tokens > *limit)?, "context"),
    };
    let param = view.max_output_tokens_param;
    let field = param.unwrap_or("max_output_tokens");
    Some(Violation {
        rule: RuleId::MaxTokensExceedsLimit,
        status: StatusCode::BAD_REQUEST,
        code: "max_tokens_exceeds_limit",
        param,
        message: format!("The requested {field} of {max_tokens} exceeds the model's {kind} limit of {limit} tokens."),
    })
}

/// (f) The request carries an input modality the catalog says the model lacks.
/// A `None` capability list means "unknown" and passes.
fn unsupported_modality(view: &RequestView, info: &ModelInfo) -> Vec<Violation> {
    let mut violations = Vec::new();
    if view.has_image_input && info.has_capability("vision") == Some(false) {
        violations.push(modality_violation(view, "image"));
    }
    if view.has_audio_input && info.has_capability("audio") == Some(false) {
        violations.push(modality_violation(view, "audio"));
    }
    violations
}

fn modality_violation(view: &RequestView, modality: &str) -> Violation {
    let alias = view.model.as_deref().unwrap_or("(unknown)");
    Violation {
        rule: RuleId::UnsupportedModality,
        status: StatusCode::BAD_REQUEST,
        code: "unsupported_modality",
        param: Some(prompt_param(view.surface)),
        message: format!("The model `{alias}` does not accept {modality} input."),
    }
}

/// (g) Stage-1 context length. `prompt_token_limit` is the context window
/// itself: engines disagree on whether `prompt + max_tokens` must fit, so
/// subtracting the requested output here would reject requests some engines
/// accept. Byte-level BPE never produces more tokens than bytes, so a prompt
/// that fits in `context_window` bytes is provably under the limit. Anything
/// larger needs an exact count: no byte ratio proves a prompt is over.
fn apply_context_length(view: &RequestView, context_window: u64, evaluation: &mut Evaluation) {
    if view.prompt_text_bytes as u64 <= context_window {
        return;
    }
    evaluation.exact_count = Some(ExactCountNeeded {
        prompt_token_limit: context_window,
        context_window,
    });
}

/// A valid max-tokens value for `surface`, or `None`. Zero, negatives,
/// fractions, strings, bools and `null` are never valid. The Messages and
/// Responses request types are strict `u32` integers, so only integers up to
/// `u32::MAX` pass there. Other surfaces also accept integral floats
/// (`256.0`), bounded by `u32::MAX` so the float-to-int conversion is exact.
fn parse_max_tokens(value: &Value, surface: Option<Surface>) -> Option<u64> {
    let strict = matches!(surface, Some(Surface::Messages | Surface::Responses));
    if let Some(n) = value.as_u64() {
        return (n > 0 && (!strict || n <= u64::from(u32::MAX))).then_some(n);
    }
    if strict || value.is_i64() {
        return None;
    }
    let f = value.as_f64()?;
    (f > 0.0 && f.fract() == 0.0 && f <= f64::from(u32::MAX)).then_some(f as u64)
}

/// `param` for prompt-shape violations; `messages` when the surface is unknown.
fn prompt_param(surface: Option<Surface>) -> &'static str {
    surface.map_or("messages", Surface::prompt_param)
}

fn surface_label(surface: Surface) -> &'static str {
    match surface {
        Surface::ChatCompletions => "chat completions",
        Surface::Completions => "completions",
        Surface::Responses => "responses",
        Surface::Messages => "messages",
        Surface::Embeddings => "embeddings",
    }
}

fn model_type_label(model_type: ModelType) -> &'static str {
    match model_type {
        ModelType::Chat => "chat",
        ModelType::Embeddings => "embeddings",
        ModelType::Reranker => "reranker",
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;

    fn config() -> ValidationConfig {
        ValidationConfig {
            enabled: true,
            ..Default::default()
        }
    }

    fn config_with(rule: RuleId, mode: RuleMode) -> ValidationConfig {
        let mut config = config();
        config.rules.insert(rule, mode);
        config
    }

    fn view(surface: Surface, model: &str) -> RequestView {
        RequestView {
            surface: Some(surface),
            model: Some(model.to_string()),
            ..Default::default()
        }
    }

    fn known(info: ModelInfo) -> ModelLookup {
        ModelLookup::Known(Arc::new(info))
    }

    fn info(model_type: ModelType) -> ModelInfo {
        ModelInfo {
            model_type: Some(model_type),
            ..Default::default()
        }
    }

    fn violations(evaluation: &Evaluation) -> Vec<RuleId> {
        evaluation.violations.iter().map(|violation| violation.rule).collect()
    }

    // --- (a) ModelNotFound -------------------------------------------------

    #[test]
    fn model_not_found_fires_for_unknown_alias() {
        let view = view(Surface::ChatCompletions, "ghost");
        let evaluation = evaluate(&view, &ModelLookup::Unknown, &config());

        assert_eq!(violations(&evaluation), vec![RuleId::ModelNotFound]);
        let violation = &evaluation.violations[0];
        assert_eq!(violation.status, StatusCode::NOT_FOUND);
        assert_eq!(violation.code, "model_not_found");
        assert_eq!(violation.param, Some("model"));
        assert_eq!(
            violation.message,
            "The model `ghost` does not exist or you do not have access to it."
        );
    }

    #[test]
    fn model_not_found_passes_when_alias_missing() {
        let view = RequestView {
            surface: Some(Surface::ChatCompletions),
            ..Default::default()
        };
        let evaluation = evaluate(&view, &ModelLookup::Unknown, &config());
        assert!(evaluation.violations.is_empty());
    }

    #[test]
    fn model_not_found_passes_when_not_loaded() {
        let view = view(Surface::ChatCompletions, "ghost");
        assert!(evaluate(&view, &ModelLookup::NotLoaded, &config()).violations.is_empty());
    }

    #[test]
    fn model_not_found_passes_when_known() {
        let view = view(Surface::ChatCompletions, "ghost");
        let evaluation = evaluate(&view, &known(info(ModelType::Chat)), &config());
        assert!(evaluation.violations.is_empty());
    }

    #[test]
    fn model_not_found_skipped_when_off() {
        let view = view(Surface::ChatCompletions, "ghost");
        let evaluation = evaluate(&view, &ModelLookup::Unknown, &config_with(RuleId::ModelNotFound, RuleMode::Off));
        assert!(evaluation.violations.is_empty());
    }

    // --- (b) ModelTypeMismatch --------------------------------------------

    #[test]
    fn model_type_mismatch_fires_for_wrong_type() {
        let view = view(Surface::ChatCompletions, "embedder");
        let evaluation = evaluate(&view, &known(info(ModelType::Embeddings)), &config());

        assert_eq!(violations(&evaluation), vec![RuleId::ModelTypeMismatch]);
        let violation = &evaluation.violations[0];
        assert_eq!(violation.status, StatusCode::BAD_REQUEST);
        assert_eq!(violation.code, "model_type_mismatch");
        assert_eq!(violation.param, Some("model"));
        assert!(violation.message.contains("embeddings"));
        assert!(violation.message.contains("chat"));
        assert!(violation.message.contains("chat completions"));
    }

    #[test]
    fn model_type_mismatch_passes_when_type_matches() {
        let view = view(Surface::Embeddings, "embedder");
        let evaluation = evaluate(&view, &known(info(ModelType::Embeddings)), &config());
        assert!(evaluation.violations.is_empty());
    }

    #[test]
    fn model_type_mismatch_passes_when_type_unknown() {
        let view = view(Surface::ChatCompletions, "mystery");
        let evaluation = evaluate(&view, &known(ModelInfo::default()), &config());
        assert!(evaluation.violations.is_empty());
    }

    #[test]
    fn model_type_mismatch_passes_when_surface_unknown() {
        let view = RequestView {
            model: Some("embedder".to_string()),
            ..Default::default()
        };
        let evaluation = evaluate(&view, &known(info(ModelType::Embeddings)), &config());
        assert!(evaluation.violations.is_empty());
    }

    #[test]
    fn model_type_mismatch_passes_when_not_loaded() {
        let view = view(Surface::ChatCompletions, "embedder");
        assert!(evaluate(&view, &ModelLookup::NotLoaded, &config()).violations.is_empty());
    }

    // --- (c) InvalidServiceTier -------------------------------------------

    #[test]
    fn invalid_service_tier_accepts_every_known_value() {
        for tier in ACCEPTED_SERVICE_TIERS {
            let view = RequestView {
                service_tier: Some(json!(tier)),
                ..Default::default()
            };
            let evaluation = evaluate(&view, &ModelLookup::NotLoaded, &config());
            assert!(evaluation.violations.is_empty(), "tier {tier} should be accepted");
        }
    }

    #[test]
    fn invalid_service_tier_fires_for_unknown_string() {
        let view = RequestView {
            service_tier: Some(json!("turbo")),
            ..Default::default()
        };
        let evaluation = evaluate(&view, &ModelLookup::NotLoaded, &config());

        assert_eq!(violations(&evaluation), vec![RuleId::InvalidServiceTier]);
        let violation = &evaluation.violations[0];
        assert_eq!(violation.code, "invalid_value");
        assert_eq!(violation.param, Some("service_tier"));
        assert!(violation.message.contains("turbo"));
        assert!(violation.message.contains(ACCEPTED_SERVICE_TIERS_LIST));
    }

    #[test]
    fn invalid_service_tier_fires_for_non_string() {
        let view = RequestView {
            service_tier: Some(json!(7)),
            ..Default::default()
        };
        let evaluation = evaluate(&view, &ModelLookup::NotLoaded, &config());
        assert_eq!(violations(&evaluation), vec![RuleId::InvalidServiceTier]);
    }

    #[test]
    fn invalid_service_tier_passes_when_absent_or_null() {
        let absent = evaluate(&RequestView::default(), &ModelLookup::NotLoaded, &config());
        assert!(absent.violations.is_empty());

        let view = RequestView {
            service_tier: Some(json!(null)),
            ..Default::default()
        };
        let null = evaluate(&view, &ModelLookup::NotLoaded, &config());
        assert!(null.violations.is_empty());
    }

    // --- (d) InvalidMaxTokens ---------------------------------------------

    #[test]
    fn invalid_max_tokens_fires_for_zero_negative_fraction_string_bool() {
        for value in [json!(0), json!(-5), json!(1.5), json!("128"), json!(true)] {
            let view = RequestView {
                max_output_tokens: Some(value.clone()),
                max_output_tokens_param: Some("max_tokens"),
                ..Default::default()
            };
            let evaluation = evaluate(&view, &ModelLookup::NotLoaded, &config());
            assert_eq!(
                violations(&evaluation),
                vec![RuleId::InvalidMaxTokens],
                "value {value} should be invalid"
            );
            let violation = &evaluation.violations[0];
            assert_eq!(violation.code, "invalid_value");
            assert_eq!(violation.param, Some("max_tokens"));
        }
    }

    #[test]
    fn invalid_max_tokens_accepts_positive_integers_and_integral_floats() {
        for value in [json!(1), json!(256), json!(256.0), json!(4096.0)] {
            let view = RequestView {
                max_output_tokens: Some(value.clone()),
                max_output_tokens_param: Some("max_tokens"),
                ..Default::default()
            };
            let evaluation = evaluate(&view, &ModelLookup::NotLoaded, &config());
            assert!(
                evaluation.violations.is_empty(),
                "value {value} should be accepted, got {:?}",
                evaluation.violations
            );
        }
    }

    #[test]
    fn invalid_max_tokens_passes_when_absent_or_null() {
        let absent = evaluate(&RequestView::default(), &ModelLookup::NotLoaded, &config());
        assert!(absent.violations.is_empty());

        let view = RequestView {
            max_output_tokens: Some(json!(null)),
            max_output_tokens_param: Some("max_tokens"),
            ..Default::default()
        };
        let null = evaluate(&view, &ModelLookup::NotLoaded, &config());
        assert!(null.violations.is_empty());
    }

    // --- (e) MaxTokensExceedsLimit ----------------------------------------

    #[test]
    fn max_tokens_exceeds_output_limit() {
        let view = RequestView {
            max_output_tokens: Some(json!(900)),
            max_output_tokens_param: Some("max_tokens"),
            ..Default::default()
        };
        let mut info = info(ModelType::Chat);
        info.max_output_tokens = Some(800);
        let evaluation = evaluate(&view, &known(info), &config());

        assert_eq!(violations(&evaluation), vec![RuleId::MaxTokensExceedsLimit]);
        let violation = &evaluation.violations[0];
        assert_eq!(violation.code, "max_tokens_exceeds_limit");
        assert_eq!(violation.param, Some("max_tokens"));
        assert!(violation.message.contains("900"));
        assert!(violation.message.contains("800"));
    }

    #[test]
    fn max_tokens_exceeds_context_limit_only() {
        let view = RequestView {
            max_output_tokens: Some(json!(900)),
            ..Default::default()
        };
        let mut info = info(ModelType::Chat);
        info.max_output_tokens = Some(1000);
        info.context_window = Some(800);
        let evaluation = evaluate(&view, &known(info), &config());
        assert_eq!(violations(&evaluation), vec![RuleId::MaxTokensExceedsLimit]);
        assert!(evaluation.violations[0].message.contains("context"));
    }

    #[test]
    fn max_tokens_exceeds_passes_at_boundary() {
        let view = RequestView {
            max_output_tokens: Some(json!(800)),
            ..Default::default()
        };
        let mut info = info(ModelType::Chat);
        info.max_output_tokens = Some(800);
        info.context_window = Some(800);
        let evaluation = evaluate(&view, &known(info), &config());
        assert!(evaluation.violations.is_empty());
    }

    #[test]
    fn max_tokens_exceeds_passes_when_limits_unknown() {
        let view = RequestView {
            max_output_tokens: Some(json!(100_000)),
            ..Default::default()
        };
        let evaluation = evaluate(&view, &known(info(ModelType::Chat)), &config());
        assert!(evaluation.violations.is_empty());
    }

    #[test]
    fn max_tokens_exceeds_ignores_malformed_value() {
        let view = RequestView {
            max_output_tokens: Some(json!("lots")),
            ..Default::default()
        };
        let mut info = info(ModelType::Chat);
        info.max_output_tokens = Some(10);
        let evaluation = evaluate(&view, &known(info), &config());
        assert_eq!(violations(&evaluation), vec![RuleId::InvalidMaxTokens]);
    }

    // --- (f) UnsupportedModality ------------------------------------------

    #[test]
    fn unsupported_image_fires_when_vision_false() {
        let view = RequestView {
            has_image_input: true,
            ..Default::default()
        };
        let mut info = info(ModelType::Chat);
        info.capabilities = Some(vec!["reasoning".to_string()]);
        let evaluation = evaluate(&view, &known(info), &config());

        assert_eq!(violations(&evaluation), vec![RuleId::UnsupportedModality]);
        let violation = &evaluation.violations[0];
        assert_eq!(violation.code, "unsupported_modality");
        assert_eq!(violation.param, Some("messages"));
        assert!(violation.message.contains("image"));
    }

    #[test]
    fn unsupported_modality_passes_when_capability_true_or_unknown() {
        let view = RequestView {
            has_image_input: true,
            has_audio_input: true,
            ..Default::default()
        };
        let supported = ModelInfo {
            capabilities: Some(vec!["vision".to_string(), "audio".to_string()]),
            ..Default::default()
        };
        assert!(evaluate(&view, &known(supported), &config()).violations.is_empty());

        // `None` capabilities = unknown = fail open.
        assert!(evaluate(&view, &known(ModelInfo::default()), &config()).violations.is_empty());
    }

    #[test]
    fn unsupported_modality_passes_without_modality_input() {
        let mut info = info(ModelType::Chat);
        info.capabilities = Some(vec![]);
        let view = view(Surface::ChatCompletions, "text-only");
        assert!(evaluate(&view, &known(info), &config()).violations.is_empty());
    }

    #[test]
    fn unsupported_modality_fires_for_both_image_and_audio_in_order() {
        let view = RequestView {
            has_image_input: true,
            has_audio_input: true,
            ..Default::default()
        };
        let mut info = info(ModelType::Chat);
        info.capabilities = Some(vec![]);
        let evaluation = evaluate(&view, &known(info), &config());

        assert_eq!(
            violations(&evaluation),
            vec![RuleId::UnsupportedModality, RuleId::UnsupportedModality]
        );
        assert!(evaluation.violations[0].message.contains("image"));
        assert!(evaluation.violations[1].message.contains("audio"));
    }

    #[test]
    fn unsupported_modality_uses_input_param_for_responses_and_embeddings() {
        for (surface, model_type) in [(Surface::Responses, ModelType::Chat), (Surface::Embeddings, ModelType::Embeddings)] {
            let view = RequestView {
                surface: Some(surface),
                has_image_input: true,
                ..Default::default()
            };
            let mut info = info(model_type);
            info.capabilities = Some(vec![]);
            let evaluation = evaluate(&view, &known(info), &config());
            assert_eq!(evaluation.violations[0].param, Some("input"), "surface {surface:?}");
        }
    }

    #[test]
    fn unsupported_modality_passes_when_not_loaded() {
        let view = RequestView {
            has_image_input: true,
            has_audio_input: true,
            ..Default::default()
        };
        assert!(evaluate(&view, &ModelLookup::NotLoaded, &config()).violations.is_empty());
    }

    // --- (g) ContextLengthExceeded ----------------------------------------

    fn context_window(window: u64) -> ModelInfo {
        ModelInfo {
            model_type: Some(ModelType::Chat),
            context_window: Some(window),
            ..Default::default()
        }
    }

    fn context_view(surface: Surface, bytes: usize) -> RequestView {
        RequestView {
            surface: Some(surface),
            model: Some("long".to_string()),
            prompt_text_bytes: bytes,
            ..Default::default()
        }
    }

    #[test]
    fn context_stage1_passes_when_bytes_fit() {
        // 100-token window: 100 bytes <= 100 is provably fine.
        let evaluation = evaluate(&context_view(Surface::ChatCompletions, 100), &known(context_window(100)), &config());
        assert!(evaluation.violations.is_empty());
        assert!(evaluation.exact_count.is_none());
    }

    #[test]
    fn context_stage1_requests_exact_count_over_byte_budget() {
        // Over the byte budget is never a rejection on its own, however large:
        // only an exact count can prove the prompt is over the window.
        for bytes in [101, 1_201, 10_000_000] {
            let evaluation = evaluate(
                &context_view(Surface::ChatCompletions, bytes),
                &known(context_window(100)),
                &config(),
            );
            assert!(evaluation.violations.is_empty(), "bytes {bytes} should defer");
            assert_eq!(
                evaluation.exact_count,
                Some(ExactCountNeeded {
                    prompt_token_limit: 100,
                    context_window: 100
                }),
                "bytes {bytes}"
            );
        }
    }

    #[test]
    fn context_stage1_passes_when_window_unknown() {
        let evaluation = evaluate(
            &context_view(Surface::ChatCompletions, 1_000_000),
            &known(info(ModelType::Chat)),
            &config(),
        );
        assert!(evaluation.violations.is_empty());
        assert!(evaluation.exact_count.is_none());
    }

    #[test]
    fn context_stage1_passes_when_not_loaded() {
        let evaluation = evaluate(
            &context_view(Surface::ChatCompletions, 1_000_000),
            &ModelLookup::NotLoaded,
            &config(),
        );
        assert!(evaluation.violations.is_empty());
        assert!(evaluation.exact_count.is_none());
    }

    #[test]
    fn context_stage1_skipped_when_off() {
        let evaluation = evaluate(
            &context_view(Surface::ChatCompletions, 99_999),
            &known(context_window(100)),
            &config_with(RuleId::ContextLengthExceeded, RuleMode::Off),
        );
        assert!(evaluation.violations.is_empty());
        assert!(evaluation.exact_count.is_none());
    }

    #[test]
    fn exact_count_violation_fires_only_over_limit() {
        let needed = ExactCountNeeded {
            prompt_token_limit: 100,
            context_window: 100,
        };
        assert!(exact_count_violation(Surface::ChatCompletions, "long", 100, &needed).is_none());

        let violation = exact_count_violation(Surface::ChatCompletions, "long", 101, &needed).expect("over limit");
        assert_eq!(violation.rule, RuleId::ContextLengthExceeded);
        assert_eq!(violation.code, "context_length_exceeded");
        assert_eq!(violation.param, Some("messages"));
        assert!(violation.message.contains("101"));
        assert!(violation.message.contains("100"));

        let responses = exact_count_violation(Surface::Responses, "long", 101, &needed).expect("over limit");
        assert_eq!(responses.param, Some("input"));
    }

    // --- ordering, modes, helpers -----------------------------------------

    #[test]
    fn violations_are_ordered_by_rule() {
        let view = RequestView {
            surface: Some(Surface::ChatCompletions),
            model: Some("ghost".to_string()),
            service_tier: Some(json!("turbo")),
            max_output_tokens: Some(json!("lots")),
            max_output_tokens_param: Some("max_tokens"),
            ..Default::default()
        };
        let evaluation = evaluate(&view, &ModelLookup::Unknown, &config());
        assert_eq!(
            violations(&evaluation),
            vec![RuleId::ModelNotFound, RuleId::InvalidServiceTier, RuleId::InvalidMaxTokens]
        );
    }

    #[test]
    fn first_enforced_ignores_shadow_and_off() {
        let view = RequestView {
            surface: Some(Surface::ChatCompletions),
            model: Some("ghost".to_string()),
            service_tier: Some(json!("turbo")),
            ..Default::default()
        };
        let mut config = config();
        config.rules.insert(RuleId::ModelNotFound, RuleMode::Enforce);
        config.rules.insert(RuleId::InvalidServiceTier, RuleMode::Shadow);
        let evaluation = evaluate(&view, &ModelLookup::Unknown, &config);
        assert_eq!(
            first_enforced(&evaluation.violations, &config).map(|v| v.rule),
            Some(RuleId::ModelNotFound)
        );

        config.rules.insert(RuleId::ModelNotFound, RuleMode::Off);
        config.rules.insert(RuleId::InvalidServiceTier, RuleMode::Enforce);
        let evaluation = evaluate(&view, &ModelLookup::Unknown, &config);
        assert_eq!(
            first_enforced(&evaluation.violations, &config).map(|v| v.rule),
            Some(RuleId::InvalidServiceTier)
        );
    }

    #[test]
    fn first_enforced_none_when_all_shadow_or_off() {
        let view = RequestView {
            service_tier: Some(json!("turbo")),
            ..Default::default()
        };
        let config = config();
        let evaluation = evaluate(&view, &ModelLookup::NotLoaded, &config);
        assert!(!evaluation.violations.is_empty());
        assert!(first_enforced(&evaluation.violations, &config).is_none());
    }

    #[test]
    fn metadata_independent_rules_still_fire_when_not_loaded() {
        // Fail-open only relaxes model-metadata rules; request-shape rules are
        // still enforced.
        let view = RequestView {
            service_tier: Some(json!("turbo")),
            max_output_tokens: Some(json!(0)),
            max_output_tokens_param: Some("max_tokens"),
            ..Default::default()
        };
        let evaluation = evaluate(&view, &ModelLookup::NotLoaded, &config());
        assert_eq!(violations(&evaluation), vec![RuleId::InvalidServiceTier, RuleId::InvalidMaxTokens]);
    }

    #[test]
    fn record_does_not_panic_and_describe_is_idempotent() {
        describe_metrics();
        describe_metrics();
        let violation = Violation {
            rule: RuleId::InvalidServiceTier,
            status: StatusCode::BAD_REQUEST,
            code: "invalid_value",
            param: Some("service_tier"),
            message: "nope".to_string(),
        };
        record(&[violation], Surface::ChatCompletions, "request", "unknown", &config());
    }

    #[test]
    fn parse_max_tokens_boundaries() {
        let chat = Some(Surface::ChatCompletions);
        assert_eq!(parse_max_tokens(&json!(1), chat), Some(1));
        assert_eq!(parse_max_tokens(&json!(u64::MAX), chat), Some(u64::MAX));
        assert_eq!(parse_max_tokens(&json!(1.0), chat), Some(1));
        assert_eq!(parse_max_tokens(&json!(f64::from(u32::MAX)), chat), Some(u64::from(u32::MAX)));
        // Out-of-range floats are rejected rather than saturated.
        assert_eq!(parse_max_tokens(&json!(1.8446744073709552e19), chat), None);
        for invalid in [json!(0), json!(0.0), json!(-1), json!(1.25), json!(null), json!("1")] {
            assert_eq!(parse_max_tokens(&invalid, chat), None, "{invalid}");
        }
    }

    #[test]
    fn parse_max_tokens_is_strict_u32_on_messages_and_responses() {
        for surface in [Surface::Messages, Surface::Responses] {
            assert_eq!(parse_max_tokens(&json!(256), Some(surface)), Some(256));
            assert_eq!(parse_max_tokens(&json!(u32::MAX), Some(surface)), Some(u64::from(u32::MAX)));
            assert_eq!(parse_max_tokens(&json!(u64::from(u32::MAX) + 1), Some(surface)), None);
            assert_eq!(parse_max_tokens(&json!(256.0), Some(surface)), None);
        }
    }

    #[test]
    fn messages_accepts_standard_only_service_tier() {
        let view = |surface| RequestView {
            surface: Some(surface),
            service_tier: Some(json!("standard_only")),
            ..Default::default()
        };
        assert!(
            evaluate(&view(Surface::Messages), &ModelLookup::NotLoaded, &config())
                .violations
                .is_empty()
        );
        assert_eq!(
            violations(&evaluate(&view(Surface::ChatCompletions), &ModelLookup::NotLoaded, &config())),
            vec![RuleId::InvalidServiceTier]
        );
    }
}
