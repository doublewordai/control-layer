//! The validation stage the inference middleware calls once per request.
//!
//! It ties the pieces together: extract a [`RequestView`], look the model up,
//! run the stage-1 rules, run the stage-2 exact count only when stage 1 could
//! not decide the context check, record every violation, and render the first
//! enforced one. Shadow-mode violations are recorded and the request proceeds.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::response::Response;
use serde_json::Value;

use super::exact::{ExactCounter, ExactOutcome};
use super::{ModelInfoSource, ModelLookup, RuleId, RuleMode, Surface, ValidationConfig, Violation, envelope, rules, view};

/// Whether a caller may use a model alias on a surface. Validation runs only
/// for callers that may: everyone else gets the normal authentication or
/// access error downstream, so rejection messages never reveal anything about
/// a model the caller cannot use. (A model with no keys is usable by anyone,
/// so requests for it are validated whether or not they carry a key.)
pub trait ModelAccess: Send + Sync {
    fn allows(&self, bearer_token: Option<&str>, alias: &str, surface: Surface) -> bool;
}

/// The routing table onwards authorises requests against: the caller must be
/// admitted by the pool onwards will resolve for this request class, exactly
/// as onwards' own key check does.
impl ModelAccess for onwards::target::Targets {
    fn allows(&self, bearer_token: Option<&str>, alias: &str, surface: Surface) -> bool {
        let Some(pools) = self.targets.get(alias) else {
            return false;
        };
        let class = match surface {
            Surface::Completions => onwards::target::RequestClass::Completions,
            _ => onwards::target::RequestClass::Normal,
        };
        match pools.resolve(class).keys() {
            None => true,
            Some(keys) => bearer_token.is_some_and(|token| onwards::auth::validate_bearer_token(keys, token)),
        }
    }
}

/// Caps how many stage-2 exact counts one unit of work (a batch file upload)
/// may spend. Once spent, remaining near-limit requests pass (fail open).
#[derive(Debug)]
pub struct ExactCountBudget(AtomicUsize);

impl ExactCountBudget {
    pub fn new(max: usize) -> Self {
        Self(AtomicUsize::new(max))
    }

    fn try_take(&self) -> bool {
        self.0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
            .is_ok()
    }
}

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
    /// Gate for [`Self::check`]. `None` validates every caller (tests only).
    access: Option<Arc<dyn ModelAccess>>,
}

impl ValidationStage {
    pub fn new(
        config: ValidationConfig,
        models: Arc<dyn ModelInfoSource>,
        exact: Option<ExactCounter>,
        access: Option<Arc<dyn ModelAccess>>,
    ) -> Self {
        rules::describe_metrics();
        let exact = exact.filter(|_| config.exact_count_enabled);
        Self {
            config: Arc::new(config),
            models,
            exact,
            access,
        }
    }

    /// A fresh exact-count budget for one batch file upload.
    pub fn batch_file_budget(&self) -> ExactCountBudget {
        ExactCountBudget::new(self.config.exact_count_max_per_batch_file)
    }

    /// Validate a parsed inference request body from a caller presenting
    /// `bearer_token`. `Some` is the rejection to return to the client; `None`
    /// means forward the request unchanged. Callers without access to the
    /// model are not validated (see [`ModelAccess`]).
    pub async fn check(&self, surface: Surface, body: &Value, bearer_token: Option<&str>) -> Option<Response> {
        if let Some(access) = &self.access {
            let alias = body.get("model").and_then(Value::as_str)?;
            if !access.allows(bearer_token, alias, surface) {
                return None;
            }
        }
        let violation = self.enforced_violation(surface, body, Source::Request, None).await?;
        Some(envelope::rejection_response(surface, &violation))
    }

    /// Run every rule on a parsed body, record all violations, and return the
    /// first enforced one. Callers render it in their own error shape, and must
    /// already have established that the caller may use the model. `budget`,
    /// when given, caps the exact counts this call may draw on.
    pub async fn enforced_violation(
        &self,
        surface: Surface,
        body: &Value,
        source: Source,
        budget: Option<&ExactCountBudget>,
    ) -> Option<Violation> {
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
            && budget.is_none_or(ExactCountBudget::try_take)
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
        let embedder = ModelInfo {
            model_type: Some(ModelType::Embeddings),
            ..ModelInfo::default()
        };
        let models = FixedModels(HashMap::from([
            ("chat-model".to_string(), Arc::new(info)),
            ("embed-model".to_string(), Arc::new(embedder)),
        ]));
        let config = ValidationConfig {
            enabled: true,
            default_mode,
            ..ValidationConfig::default()
        };
        ValidationStage::new(config, Arc::new(models), None, None)
    }

    fn chat(model: &str, text: &str) -> Value {
        json!({"model": model, "messages": [{"role": "user", "content": text}]})
    }

    #[tokio::test]
    async fn valid_request_passes() {
        assert!(
            stage(RuleMode::Enforce)
                .check(Surface::ChatCompletions, &chat("chat-model", "hi"), None)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn enforced_violation_is_rejected_in_surface_shape() {
        let response = stage(RuleMode::Enforce)
            .check(Surface::ChatCompletions, &chat("embed-model", "hi"), None)
            .await
            .expect("an embeddings model on chat completions is rejected");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()["x-dw-rejected-by"], "ingress-validation");
    }

    #[tokio::test]
    async fn shadow_violation_forwards_the_request() {
        assert!(
            stage(RuleMode::Shadow)
                .check(Surface::ChatCompletions, &chat("embed-model", "hi"), None)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn near_limit_without_exact_counter_passes() {
        // 500 bytes against a 100-token window needs an exact count, and
        // there is no counter: fail open.
        let text = "a".repeat(500);
        assert!(
            stage(RuleMode::Enforce)
                .check(Surface::ChatCompletions, &chat("chat-model", &text), None)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn far_over_limit_without_exact_counter_passes() {
        // Size alone never proves a prompt is over the window: without an
        // exact count the context rule cannot reject.
        let text = "a".repeat(5_000);
        assert!(
            stage(RuleMode::Enforce)
                .check(Surface::ChatCompletions, &chat("chat-model", &text), None)
                .await
                .is_none()
        );
    }

    struct AllowOnly(&'static str);

    impl ModelAccess for AllowOnly {
        fn allows(&self, bearer_token: Option<&str>, _alias: &str, _surface: Surface) -> bool {
            bearer_token == Some(self.0)
        }
    }

    fn gated_stage() -> ValidationStage {
        let config = ValidationConfig {
            enabled: true,
            default_mode: RuleMode::Enforce,
            ..ValidationConfig::default()
        };
        let models = FixedModels(HashMap::new());
        ValidationStage::new(config, Arc::new(models), None, Some(Arc::new(AllowOnly("good-key"))))
    }

    #[tokio::test]
    async fn callers_without_model_access_are_not_validated() {
        let stage = gated_stage();
        // An invalid service tier needs no model metadata to fire.
        let body = json!({"model": "any-model", "service_tier": "turbo", "messages": []});
        // Without access: no validation, so the normal auth/access error
        // downstream answers instead of a rule revealing model details.
        assert!(stage.check(Surface::ChatCompletions, &body, None).await.is_none());
        assert!(stage.check(Surface::ChatCompletions, &body, Some("other-key")).await.is_none());
        // With access: the rule applies.
        assert!(stage.check(Surface::ChatCompletions, &body, Some("good-key")).await.is_some());
    }

    #[test]
    fn exact_count_budget_is_spent_once_per_take() {
        let budget = ExactCountBudget::new(2);
        assert!(budget.try_take());
        assert!(budget.try_take());
        assert!(!budget.try_take(), "an exhausted budget must stay exhausted");
        assert!(!ExactCountBudget::new(0).try_take());
    }
}
