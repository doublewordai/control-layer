//! The validation stage the inference middleware calls once per request.
//!
//! It ties the pieces together: extract a [`RequestView`], look the model up,
//! run the stage-1 rules, run the stage-2 exact count only when stage 1 could
//! not decide the context check, record every violation, and render the first
//! enforced one. Shadow-mode violations are recorded and the request proceeds.

use std::sync::Arc;

use axum::response::Response;
use serde_json::Value;

use super::exact::{ExactCounter, ExactOutcome};
use super::{ModelInfoSource, ModelLookup, RuleId, RuleMode, Surface, ValidationConfig, Violation, envelope, rules, view};

/// Where a validated request came from, recorded as the `source` metric label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A realtime or flex request at the inference endpoints.
    Request,
    /// A line of an uploaded batch input file.
    BatchFile,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::BatchFile => "batch_file",
        }
    }
}

/// Everything the middleware needs to validate a request. Cheap to clone.
#[derive(Clone)]
pub struct ValidationStage {
    config: Arc<ValidationConfig>,
    models: Arc<dyn ModelInfoSource>,
    /// `None` disables stage 2: near-limit requests pass.
    exact: Option<ExactCounter>,
}

impl ValidationStage {
    pub fn new(config: ValidationConfig, models: Arc<dyn ModelInfoSource>, exact: Option<ExactCounter>) -> Self {
        rules::describe_metrics();
        let exact = exact.filter(|_| config.exact_count_enabled);
        Self {
            config: Arc::new(config),
            models,
            exact,
        }
    }

    /// Validate a parsed inference request body. `Some` is the rejection to
    /// return to the client; `None` means forward the request unchanged.
    pub async fn check(&self, surface: Surface, body: &Value) -> Option<Response> {
        let violation = self.enforced_violation(surface, body, Source::Request).await?;
        Some(envelope::rejection_response(surface, &violation))
    }

    /// Run every rule on a parsed body, record all violations, and return the
    /// first enforced one. Callers render it in their own error shape.
    pub async fn enforced_violation(&self, surface: Surface, body: &Value, source: Source) -> Option<Violation> {
        let view = view::extract(surface, body);
        let lookup = match view.model.as_deref() {
            Some(alias) => self.models.lookup(alias),
            // No string model: onwards owns that error.
            None => ModelLookup::NotLoaded,
        };
        let mut evaluation = rules::evaluate(&view, &lookup, &self.config);

        // An enforced stage-1 rejection already decides the request; don't pay
        // for a tokenizer round trip to find a second reason.
        let decided = rules::first_enforced(&evaluation.violations, &self.config).is_some();
        if let (Some(needed), Some(exact), Some(alias), false) = (&evaluation.exact_count, &self.exact, view.model.as_deref(), decided)
            && self.config.mode(RuleId::ContextLengthExceeded) != RuleMode::Off
            && let ExactOutcome::Counted(tokens) = exact.prompt_tokens(alias, surface, body).await
            && let Some(violation) = rules::exact_count_violation(surface, alias, tokens, needed)
        {
            evaluation.violations.push(violation);
        }

        if evaluation.violations.is_empty() {
            return None;
        }
        // Unknown aliases are client-controlled strings: keep the label bounded.
        let model_label = match (&lookup, view.model.as_deref()) {
            (ModelLookup::Known(_), Some(alias)) => alias,
            _ => "unknown",
        };
        rules::record(&evaluation.violations, surface, source.as_str(), model_label, &self.config);

        let violation = rules::first_enforced(&evaluation.violations, &self.config)?;
        tracing::info!(
            rule = violation.rule.as_str(),
            surface = surface.as_str(),
            source = source.as_str(),
            model = model_label,
            "Rejected inference request at ingress validation"
        );
        Some(violation.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use axum::http::StatusCode;
    use serde_json::json;

    use super::*;
    use crate::db::models::deployments::ModelType;
    use crate::inference::validation::ModelInfo;

    struct FixedModels(HashMap<String, Arc<ModelInfo>>);

    impl ModelInfoSource for FixedModels {
        fn lookup(&self, alias: &str) -> ModelLookup {
            self.0.get(alias).cloned().map_or(ModelLookup::Unknown, ModelLookup::Known)
        }
    }

    fn stage(default_mode: RuleMode) -> ValidationStage {
        let info = ModelInfo {
            model_type: Some(ModelType::Chat),
            context_window: Some(100),
            max_output_tokens: None,
            capabilities: Some(vec!["reasoning".to_string()]),
        };
        let models = FixedModels(HashMap::from([("chat-model".to_string(), Arc::new(info))]));
        let config = ValidationConfig {
            enabled: true,
            default_mode,
            ..ValidationConfig::default()
        };
        ValidationStage::new(config, Arc::new(models), None)
    }

    fn chat(model: &str, text: &str) -> Value {
        json!({"model": model, "messages": [{"role": "user", "content": text}]})
    }

    #[tokio::test]
    async fn valid_request_passes() {
        assert!(
            stage(RuleMode::Enforce)
                .check(Surface::ChatCompletions, &chat("chat-model", "hi"))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn enforced_violation_is_rejected_in_surface_shape() {
        let response = stage(RuleMode::Enforce)
            .check(Surface::ChatCompletions, &chat("missing-model", "hi"))
            .await
            .expect("unknown model is rejected");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(response.headers()["x-dw-rejected-by"], "ingress-validation");
    }

    #[tokio::test]
    async fn shadow_violation_forwards_the_request() {
        assert!(
            stage(RuleMode::Shadow)
                .check(Surface::ChatCompletions, &chat("missing-model", "hi"))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn near_limit_without_exact_counter_passes() {
        // 500 bytes against a 100-token window is in the exact-count band
        // (100 < 500 / 12 is false), and there is no counter: fail open.
        let text = "a".repeat(500);
        assert!(
            stage(RuleMode::Enforce)
                .check(Surface::ChatCompletions, &chat("chat-model", &text))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn far_over_limit_is_rejected_without_exact_counter() {
        let text = "a".repeat(5_000);
        let response = stage(RuleMode::Enforce)
            .check(Surface::ChatCompletions, &chat("chat-model", &text))
            .await
            .expect("far over the window is rejected at stage 1");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
