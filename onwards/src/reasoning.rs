//! Canonical OpenAI reasoning controls and provider-specific request translation.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

const ALLOWED_EFFORTS: &str = "none, minimal, low, medium, high, xhigh, max";
const MAX_TARGET_DEPTH: usize = 8;
const MAX_VALUE_BYTES: usize = 8 * 1024;
const MAX_VALUE_DEPTH: usize = 8;

/// Canonical reasoning effort values accepted by OpenAI-compatible endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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

/// Canonical reasoning controls normalized from the original client request.
#[derive(Debug, Clone, PartialEq)]
pub struct CanonicalReasoningRequest {
    effort: ReasoningEffort,
    effort_param: &'static str,
    output_limit: OutputLimit,
}

impl CanonicalReasoningRequest {
    pub const fn effort(&self) -> ReasoningEffort {
        self.effort
    }

    pub const fn effort_param(&self) -> &'static str {
        self.effort_param
    }

    pub const fn output_limit_param(&self) -> &'static str {
        self.output_limit.param()
    }

    pub fn output_limit_value(&self) -> Option<&Value> {
        self.output_limit.value()
    }
}

#[derive(Debug, Clone, PartialEq)]
enum OutputLimit {
    Missing { param: &'static str },
    Present { param: &'static str, value: Value },
}

impl OutputLimit {
    const fn param(&self) -> &'static str {
        match self {
            Self::Missing { param } | Self::Present { param, .. } => param,
        }
    }

    fn value(&self) -> Option<&Value> {
        match self {
            Self::Missing { .. } => None,
            Self::Present { value, .. } => Some(value),
        }
    }
}

impl ReasoningEffort {
    pub const ALL: [Self; 7] = [
        Self::None,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
        Self::Max,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl fmt::Display for ReasoningEffort {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ReasoningEffort {
    type Err = ReasoningError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|effort| effort.as_str() == value)
            .ok_or_else(|| ReasoningError::invalid_effort(value, "reasoning_effort"))
    }
}

/// Maps canonical reasoning efforts to values at one provider request path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningWrite {
    pub target_path: String,
    pub values: BTreeMap<ReasoningEffort, Value>,
}

/// Maps every canonical reasoning effort to provider request writes or an explicit rejection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningTranslation {
    pub unsupported_efforts: BTreeSet<ReasoningEffort>,
    pub writes: Vec<ReasoningWrite>,
}

/// Provider translations for each supported OpenAI request surface.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReasoningTranslationConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_completions: Option<ReasoningTranslation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responses: Option<ReasoningTranslation>,
}

/// A parameter-specific error safe to return in an OpenAI error envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasoningError {
    message: String,
    param: Option<String>,
    code: &'static str,
    status_code: u16,
}

impl ReasoningError {
    fn new(message: impl Into<String>, param: Option<&str>, code: &'static str) -> Self {
        Self {
            message: message.into(),
            param: param.map(str::to_string),
            code,
            status_code: 400,
        }
    }

    fn unprocessable(message: impl Into<String>, param: Option<&str>, code: &'static str) -> Self {
        Self {
            message: message.into(),
            param: param.map(str::to_string),
            code,
            status_code: 422,
        }
    }

    fn invalid_effort(value: &str, param: &str) -> Self {
        Self::new(
            format!("Invalid value '{value}' for '{param}'. Expected one of: {ALLOWED_EFFORTS}."),
            Some(param),
            "invalid_value",
        )
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn param(&self) -> Option<&str> {
        self.param.as_deref()
    }

    pub fn code(&self) -> &'static str {
        self.code
    }

    pub const fn status_code(&self) -> u16 {
        self.status_code
    }
}

impl fmt::Display for ReasoningError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ReasoningError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApiSurface {
    ChatCompletions,
    Responses,
    Completions,
    Other,
}

impl ApiSurface {
    fn from_path(path: &str) -> Self {
        let path = path.trim_end_matches('/');
        if path.ends_with("/chat/completions") {
            Self::ChatCompletions
        } else if path.ends_with("/responses") {
            Self::Responses
        } else if path.ends_with("/completions") {
            Self::Completions
        } else {
            Self::Other
        }
    }

    const fn canonical_path(self) -> Option<&'static str> {
        match self {
            Self::ChatCompletions => Some("/reasoning_effort"),
            Self::Responses => Some("/reasoning/effort"),
            Self::Completions | Self::Other => None,
        }
    }

    const fn parameter_name(self) -> Option<&'static str> {
        match self {
            Self::ChatCompletions => Some("reasoning_effort"),
            Self::Responses => Some("reasoning.effort"),
            Self::Completions | Self::Other => None,
        }
    }
}

pub(crate) fn uses_reasoning_contract(path: &str) -> bool {
    !matches!(ApiSurface::from_path(path), ApiSurface::Other)
}

impl ReasoningTranslationConfig {
    /// Validate all target paths and mapped values before accepting configuration.
    pub fn validate(&self) -> Result<(), ReasoningError> {
        if self.chat_completions.is_none() && self.responses.is_none() {
            return Err(invalid_config(
                "at least one API surface must be configured",
            ));
        }

        for translation in [self.chat_completions.as_ref(), self.responses.as_ref()]
            .into_iter()
            .flatten()
        {
            validate_translation(translation)?;
        }
        Ok(())
    }

    /// Apply the configured provider mapping to a canonical request body.
    pub fn apply(&self, path: &str, body: &mut Value) -> Result<(), ReasoningError> {
        let Some(request) = validate_canonical_reasoning(path, body)? else {
            return Ok(());
        };
        self.apply_request(path, &request, body)
    }

    /// Apply a mapping using reasoning controls normalized from the original request.
    pub(crate) fn apply_request(
        &self,
        path: &str,
        request: &CanonicalReasoningRequest,
        body: &mut Value,
    ) -> Result<(), ReasoningError> {
        self.validate_request(path, request)?;
        let surface = ApiSurface::from_path(path);
        let effort = request.effort();
        let translation = self.translation_for(surface);
        let Some(translation) = translation else {
            return Ok(());
        };
        let canonical_path = surface
            .canonical_path()
            .expect("translated reasoning surfaces have canonical paths");
        if !translation
            .writes
            .iter()
            .any(|write| write.target_path == canonical_path)
        {
            remove_json_pointer(body, canonical_path);
        }
        for write in &translation.writes {
            let mapped_value = write
                .values
                .get(&effort)
                .cloned()
                .expect("validated effort has a provider mapping");
            set_json_pointer(body, &write.target_path, mapped_value)?;
        }
        Ok(())
    }

    /// Validate provider support and any absolute reasoning budget before routing.
    pub fn validate_request(
        &self,
        path: &str,
        request: &CanonicalReasoningRequest,
    ) -> Result<(), ReasoningError> {
        self.validate()?;
        let surface = ApiSurface::from_path(path);
        let Some(translation) = self.translation_for(surface) else {
            return Ok(());
        };
        self.validate_effort(path, request.effort())?;

        let Some(budget_write) = translation
            .writes
            .iter()
            .find(|write| write.target_path == "/thinking_token_budget")
        else {
            return Ok(());
        };
        let budget = budget_write
            .values
            .get(&request.effort())
            .and_then(Value::as_u64)
            .expect("validated budget mappings contain non-negative integers");
        let output_param = request.output_limit_param();
        let Some(output_value) = request.output_limit_value() else {
            return Err(ReasoningError::unprocessable(
                format!(
                    "{} '{}' maps to an {budget}-token reasoning budget for this model, but {output_param} is not set. Set {output_param} above {budget} or select a lower {}.",
                    request.effort_param(),
                    request.effort(),
                    request.effort_param(),
                ),
                Some(output_param),
                "reasoning_budget_requires_max_tokens",
            ));
        };
        let output_limit = output_value.as_u64().ok_or_else(|| {
            ReasoningError::new(
                format!("Invalid type for '{output_param}'. Expected a non-negative integer."),
                Some(output_param),
                "invalid_type",
            )
        })?;
        if budget >= output_limit {
            return Err(ReasoningError::new(
                format!(
                    "{} '{}' maps to an {budget}-token reasoning budget for this model, but {output_param} is {output_limit}. Increase {output_param} above {budget} or select a lower {}.",
                    request.effort_param(),
                    request.effort(),
                    request.effort_param(),
                ),
                Some(output_param),
                "reasoning_budget_exceeds_max_tokens",
            ));
        }
        Ok(())
    }

    /// Ensure a provider can represent an effort before load balancing begins.
    pub fn validate_effort(
        &self,
        path: &str,
        effort: ReasoningEffort,
    ) -> Result<(), ReasoningError> {
        self.validate()?;
        let surface = ApiSurface::from_path(path);
        let Some(translation) = self.translation_for(surface) else {
            return Ok(());
        };
        let supported_values = &translation
            .writes
            .first()
            .expect("validated translations contain at least one write")
            .values;
        if supported_values.contains_key(&effort) {
            return Ok(());
        }
        let supported = supported_values
            .keys()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        Err(ReasoningError::new(
            format!(
                "Reasoning effort '{effort}' is not supported by this provider. Supported values: {supported}."
            ),
            surface.parameter_name(),
            "unsupported_value",
        ))
    }

    fn translation_for(&self, surface: ApiSurface) -> Option<&ReasoningTranslation> {
        match surface {
            ApiSurface::ChatCompletions => self.chat_completions.as_ref(),
            ApiSurface::Responses => self.responses.as_ref(),
            ApiSurface::Completions | ApiSurface::Other => None,
        }
    }
}

/// Validate and return the canonical effort supplied for the request surface.
pub fn validate_canonical_reasoning(
    path: &str,
    body: &Value,
) -> Result<Option<CanonicalReasoningRequest>, ReasoningError> {
    let surface = ApiSurface::from_path(path);
    if surface == ApiSurface::Completions {
        let unsupported = [
            ("/reasoning_effort", "reasoning_effort"),
            ("/reasoning", "reasoning"),
            ("/thinking", "thinking"),
            ("/thinking_token_budget", "thinking_token_budget"),
            ("/chat_template_kwargs", "chat_template_kwargs"),
        ]
        .into_iter()
        .find(|(pointer, _)| body.pointer(pointer).is_some());
        if let Some((_, param)) = unsupported {
            return Err(ReasoningError::new(
                format!(
                    "Parameter '{param}' is not supported by /v1/completions. Use /v1/chat/completions or /v1/responses."
                ),
                Some(param),
                "unsupported_parameter",
            ));
        }
        return Ok(None);
    }

    let unsupported = match surface {
        ApiSurface::ChatCompletions => [
            (
                "/chat_template_kwargs/thinking",
                "chat_template_kwargs.thinking",
            ),
            (
                "/chat_template_kwargs/enable_thinking",
                "chat_template_kwargs.enable_thinking",
            ),
            ("/chat_template_kwargs", "chat_template_kwargs"),
            ("/thinking", "thinking"),
            ("/thinking_token_budget", "thinking_token_budget"),
            ("/reasoning", "reasoning"),
        ]
        .into_iter()
        .find(|(pointer, _)| body.pointer(pointer).is_some()),
        ApiSurface::Responses => [
            (
                "/chat_template_kwargs/thinking",
                "chat_template_kwargs.thinking",
            ),
            (
                "/chat_template_kwargs/enable_thinking",
                "chat_template_kwargs.enable_thinking",
            ),
            ("/chat_template_kwargs", "chat_template_kwargs"),
            ("/thinking", "thinking"),
            ("/thinking_token_budget", "thinking_token_budget"),
            ("/reasoning_effort", "reasoning_effort"),
        ]
        .into_iter()
        .find(|(pointer, _)| body.pointer(pointer).is_some()),
        ApiSurface::Completions | ApiSurface::Other => None,
    };
    if let Some((_, param)) = unsupported {
        let canonical = surface
            .parameter_name()
            .expect("reasoning API surfaces have canonical parameters");
        return Err(ReasoningError::new(
            format!("Unsupported parameter '{param}'; use '{canonical}'."),
            Some(param),
            "unsupported_parameter",
        ));
    }

    if surface == ApiSurface::Responses
        && let Some(reasoning) = body.get("reasoning").filter(|value| !value.is_null())
        && !reasoning.is_object()
    {
        return Err(ReasoningError::new(
            "Invalid type for 'reasoning'. Expected an object.",
            Some("reasoning"),
            "invalid_type",
        ));
    }

    let Some(canonical_path) = surface.canonical_path() else {
        return Ok(None);
    };
    let Some(value) = body.pointer(canonical_path) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let param = surface
        .parameter_name()
        .expect("canonical reasoning surfaces have parameter names");
    let value = value.as_str().ok_or_else(|| {
        ReasoningError::new(
            format!("Invalid type for '{param}'. Expected a string."),
            Some(param),
            "invalid_type",
        )
    })?;
    let effort = value
        .parse::<ReasoningEffort>()
        .map_err(|_| ReasoningError::invalid_effort(value, param))?;
    Ok(Some(CanonicalReasoningRequest {
        effort,
        effort_param: param,
        output_limit: normalize_output_limit(surface, body),
    }))
}

fn normalize_output_limit(surface: ApiSurface, body: &Value) -> OutputLimit {
    let preferred_param = match surface {
        ApiSurface::ChatCompletions => "max_completion_tokens",
        ApiSurface::Responses => "max_output_tokens",
        ApiSurface::Completions | ApiSurface::Other => {
            unreachable!("output limits are normalized only for reasoning API surfaces")
        }
    };
    let candidates = match surface {
        ApiSurface::ChatCompletions => [
            ("/max_completion_tokens", "max_completion_tokens"),
            ("/max_tokens", "max_tokens"),
        ]
        .as_slice(),
        ApiSurface::Responses => [("/max_output_tokens", "max_output_tokens")].as_slice(),
        ApiSurface::Completions | ApiSurface::Other => unreachable!(),
    };
    candidates
        .iter()
        .find_map(|(pointer, param)| {
            body.pointer(pointer)
                .filter(|value| !value.is_null())
                .map(|value| OutputLimit::Present {
                    param,
                    value: value.clone(),
                })
        })
        .unwrap_or(OutputLimit::Missing {
            param: preferred_param,
        })
}

fn validate_translation(translation: &ReasoningTranslation) -> Result<(), ReasoningError> {
    if translation.writes.is_empty() {
        return Err(invalid_config("writes must contain at least one write"));
    }

    let mut target_paths: BTreeSet<&str> = BTreeSet::new();
    let mut supported_efforts: Option<BTreeSet<ReasoningEffort>> = None;
    let mut has_reasoning_effort = false;
    let mut has_thinking_token_budget = false;

    for write in &translation.writes {
        if target_paths.contains(write.target_path.as_str()) {
            return Err(invalid_config("target paths must be unique"));
        }
        if target_paths
            .iter()
            .any(|target_path| target_paths_overlap(target_path, &write.target_path))
        {
            return Err(invalid_config("target paths must not overlap"));
        }
        target_paths.insert(write.target_path.as_str());
        validate_write(write)?;
        has_reasoning_effort |= write.target_path == "/reasoning_effort";
        has_thinking_token_budget |= write.target_path == "/thinking_token_budget";

        let write_efforts = write.values.keys().copied().collect::<BTreeSet<_>>();
        if write_efforts.is_empty() {
            return Err(invalid_config(
                "values must contain at least one effort mapping",
            ));
        }
        if let Some(expected) = supported_efforts.as_ref()
            && expected != &write_efforts
        {
            return Err(invalid_config(
                "every write must map exactly the same reasoning efforts",
            ));
        }
        supported_efforts.get_or_insert(write_efforts);
    }

    if has_thinking_token_budget && !has_reasoning_effort {
        return Err(invalid_config(
            "thinking_token_budget requires a reasoning_effort write",
        ));
    }

    let supported_efforts = supported_efforts.expect("validated writes contain effort mappings");
    if !supported_efforts.is_disjoint(&translation.unsupported_efforts) {
        return Err(invalid_config(
            "mapped and unsupported reasoning efforts must not overlap",
        ));
    }
    let accounted_efforts = supported_efforts
        .union(&translation.unsupported_efforts)
        .copied()
        .collect::<BTreeSet<_>>();
    if accounted_efforts != ReasoningEffort::ALL.into_iter().collect() {
        return Err(invalid_config(
            "mapped and unsupported reasoning efforts must account for every OpenAI reasoning effort",
        ));
    }

    Ok(())
}

fn target_paths_overlap(left: &str, right: &str) -> bool {
    right
        .strip_prefix(left)
        .is_some_and(|suffix| suffix.starts_with('/'))
        || left
            .strip_prefix(right)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn validate_write(write: &ReasoningWrite) -> Result<(), ReasoningError> {
    let segments = pointer_segments(&write.target_path)?;
    let Some(root) = segments.first().map(String::as_str) else {
        return Err(invalid_config("target_path must not be empty"));
    };
    let allowed_path = match root {
        "reasoning_effort" => segments.len() == 1,
        "thinking_token_budget" => segments.len() == 1,
        "reasoning" | "thinking" => true,
        "chat_template_kwargs" => {
            segments.len() == 2 && matches!(segments[1].as_str(), "thinking" | "enable_thinking")
        }
        _ => false,
    };
    if !allowed_path {
        return Err(invalid_config(
            "target_path must address a reasoning-related request field",
        ));
    }
    for value in write.values.values() {
        if write.target_path == "/thinking_token_budget" && value.as_u64().is_none() {
            return Err(invalid_config(
                "thinking_token_budget values must be non-negative integers",
            ));
        }
        let bytes = serde_json::to_vec(value)
            .map_err(|_| invalid_config("mapped values must be valid JSON"))?;
        if bytes.len() > MAX_VALUE_BYTES {
            return Err(invalid_config("mapped values must not exceed 8192 bytes"));
        }
        if json_depth(value) > MAX_VALUE_DEPTH {
            return Err(invalid_config("mapped values must not exceed 8 levels"));
        }
    }
    Ok(())
}

fn pointer_segments(path: &str) -> Result<Vec<String>, ReasoningError> {
    if !path.starts_with('/') || path.len() == 1 {
        return Err(invalid_config(
            "target_path must be an absolute JSON pointer",
        ));
    }
    let mut segments = Vec::new();
    for segment in path[1..].split('/') {
        if segment.is_empty()
            || !segment.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
            })
        {
            return Err(invalid_config(
                "target_path segments may only contain letters, numbers, underscores, and hyphens",
            ));
        }
        segments.push(segment.to_string());
    }
    if segments.len() > MAX_TARGET_DEPTH {
        return Err(invalid_config(
            "target_path must not exceed 8 path segments",
        ));
    }
    Ok(segments)
}

fn set_json_pointer(body: &mut Value, path: &str, value: Value) -> Result<(), ReasoningError> {
    let segments = pointer_segments(path)?;
    let mut current = body;
    for segment in &segments[..segments.len() - 1] {
        let object = current
            .as_object_mut()
            .ok_or_else(|| invalid_config("target_path crosses a non-object request field"))?;
        current = object
            .entry(segment.clone())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    let object = current
        .as_object_mut()
        .ok_or_else(|| invalid_config("target_path parent must be a JSON object"))?;
    object.insert(
        segments
            .last()
            .expect("validated JSON pointers contain a segment")
            .clone(),
        value,
    );
    Ok(())
}

fn remove_json_pointer(body: &mut Value, path: &str) {
    let Ok(segments) = pointer_segments(path) else {
        return;
    };
    remove_json_pointer_segments(body, &segments);
}

fn remove_json_pointer_segments(current: &mut Value, segments: &[String]) -> bool {
    let Some(object) = current.as_object_mut() else {
        return false;
    };
    if segments.len() == 1 {
        return object.remove(&segments[0]).is_some() && object.is_empty();
    }

    let child_is_empty = object
        .get_mut(&segments[0])
        .is_some_and(|child| remove_json_pointer_segments(child, &segments[1..]));
    if child_is_empty {
        object.remove(&segments[0]);
        return object.is_empty();
    }
    false
}

fn json_depth(value: &Value) -> usize {
    match value {
        Value::Array(values) => 1 + values.iter().map(json_depth).max().unwrap_or(0),
        Value::Object(values) => 1 + values.values().map(json_depth).max().unwrap_or(0),
        _ => 1,
    }
}

fn invalid_config(message: &str) -> ReasoningError {
    ReasoningError::new(
        format!("Invalid reasoning translation config: {message}."),
        Some("reasoning_translation"),
        "invalid_translation_config",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sglang_config() -> ReasoningTranslationConfig {
        serde_json::from_value(json!({
            "chat_completions": {
                "unsupported_efforts": ["minimal", "xhigh", "max"],
                "writes": [{
                    "target_path": "/chat_template_kwargs/thinking",
                    "values": {
                        "none": false,
                        "low": true,
                        "medium": true,
                        "high": true
                    }
                }]
            }
        }))
        .unwrap()
    }

    fn token_budget_config() -> ReasoningTranslationConfig {
        serde_json::from_value(json!({
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
        .unwrap()
    }

    fn responses_token_budget_config() -> ReasoningTranslationConfig {
        serde_json::from_value(json!({
            "responses": {
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
        .unwrap()
    }

    #[test]
    fn accepts_explicit_multi_write_config_covering_all_openai_efforts() {
        let config: ReasoningTranslationConfig = serde_json::from_value(json!({
            "chat_completions": {
                "unsupported_efforts": [],
                "writes": [
                    {
                        "target_path": "/reasoning_effort",
                        "values": {
                            "none": "none",
                            "minimal": "low",
                            "low": "low",
                            "medium": "medium",
                            "high": "high",
                            "xhigh": "high",
                            "max": "high"
                        }
                    },
                    {
                        "target_path": "/thinking_token_budget",
                        "values": {
                            "none": 0,
                            "minimal": 512,
                            "low": 1024,
                            "medium": 4096,
                            "high": 8192,
                            "xhigh": 12288,
                            "max": 16384
                        }
                    }
                ]
            }
        }))
        .unwrap();

        config.validate().unwrap();
    }

    #[test]
    fn translates_chat_reasoning_effort_to_sglang_boolean() {
        let config = sglang_config();
        let mut body = json!({
            "model": "kimi-k2.5",
            "messages": [{"role": "user", "content": "Hello"}],
            "reasoning_effort": "none"
        });

        config.apply("/chat/completions", &mut body).unwrap();

        assert_eq!(
            body.pointer("/chat_template_kwargs/thinking"),
            Some(&json!(false))
        );
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn omitted_reasoning_effort_does_not_change_body() {
        let config = sglang_config();
        let mut body = json!({"model": "kimi-k2.5", "messages": []});
        let original = body.clone();

        config.apply("/chat/completions", &mut body).unwrap();

        assert_eq!(body, original);
    }

    #[test]
    fn supports_object_shaped_provider_values() {
        let config: ReasoningTranslationConfig = serde_json::from_value(json!({
            "chat_completions": {
                "unsupported_efforts": ["minimal", "low", "medium", "xhigh", "max"],
                "writes": [{
                    "target_path": "/thinking",
                    "values": {
                        "none": {"type": "disabled"},
                        "high": {"type": "enabled"}
                    }
                }]
            }
        }))
        .unwrap();
        let mut body = json!({"model": "model", "messages": [], "reasoning_effort": "high"});

        config.apply("/v1/chat/completions", &mut body).unwrap();

        assert_eq!(body["thinking"], json!({"type": "enabled"}));
    }

    #[test]
    fn responses_translation_removes_empty_canonical_parent() {
        let config: ReasoningTranslationConfig = serde_json::from_value(json!({
            "responses": {
                "unsupported_efforts": ["none", "minimal", "medium", "high", "xhigh", "max"],
                "writes": [{
                    "target_path": "/thinking",
                    "values": {"low": {"type": "enabled"}}
                }]
            }
        }))
        .unwrap();
        let mut body = json!({
            "model": "model",
            "input": "Hello",
            "reasoning": {"effort": "low"}
        });

        config.apply("/v1/responses", &mut body).unwrap();

        assert!(body.get("reasoning").is_none());
        assert_eq!(body["thinking"], json!({"type": "enabled"}));
    }

    #[test]
    fn responses_translation_preserves_canonical_reasoning_siblings() {
        let config: ReasoningTranslationConfig = serde_json::from_value(json!({
            "responses": {
                "unsupported_efforts": ["none", "minimal", "medium", "high", "xhigh", "max"],
                "writes": [{
                    "target_path": "/thinking",
                    "values": {"low": true}
                }]
            }
        }))
        .unwrap();
        let mut body = json!({
            "model": "model",
            "input": "Hello",
            "reasoning": {"effort": "low", "summary": "auto"}
        });

        config.apply("/v1/responses", &mut body).unwrap();

        assert_eq!(body["reasoning"], json!({"summary": "auto"}));
        assert_eq!(body["thinking"], json!(true));
    }

    #[test]
    fn rejects_effort_missing_from_provider_map() {
        let config = sglang_config();
        let mut body = json!({"model": "model", "messages": [], "reasoning_effort": "xhigh"});

        let error = config.apply("/chat/completions", &mut body).unwrap_err();

        assert_eq!(error.param(), Some("reasoning_effort"));
        assert_eq!(error.code(), "unsupported_value");
        assert!(error.message().contains("xhigh"));
    }

    #[test]
    fn rejects_unknown_canonical_effort() {
        let body = json!({"model": "model", "messages": [], "reasoning_effort": "ultra"});

        let error = validate_canonical_reasoning("/chat/completions", &body).unwrap_err();

        assert_eq!(error.param(), Some("reasoning_effort"));
        assert_eq!(error.code(), "invalid_value");
    }

    #[test]
    fn rejects_non_object_responses_reasoning() {
        for reasoning in [json!("high"), json!(1), json!(true), json!([])] {
            let body = json!({
                "model": "model",
                "input": "Hello",
                "reasoning": reasoning
            });

            let error = validate_canonical_reasoning("/responses", &body).unwrap_err();

            assert_eq!(error.status_code(), 400);
            assert_eq!(error.param(), Some("reasoning"));
            assert_eq!(error.code(), "invalid_type");
            assert_eq!(
                error.message(),
                "Invalid type for 'reasoning'. Expected an object."
            );
        }
    }

    #[test]
    fn accepts_responses_reasoning_without_an_effort() {
        for body in [
            json!({"model": "model", "input": "Hello"}),
            json!({"model": "model", "input": "Hello", "reasoning": null}),
            json!({
                "model": "model",
                "input": "Hello",
                "reasoning": {"summary": "auto"}
            }),
        ] {
            assert_eq!(
                validate_canonical_reasoning("/responses", &body).unwrap(),
                None
            );
        }
    }

    #[test]
    fn chat_prefers_max_completion_tokens_over_legacy_max_tokens() {
        let body = json!({
            "model": "model",
            "messages": [],
            "reasoning_effort": "high",
            "max_completion_tokens": 9000,
            "max_tokens": 4000
        });

        let request = validate_canonical_reasoning("/chat/completions", &body)
            .unwrap()
            .unwrap();

        assert_eq!(request.output_limit_param(), "max_completion_tokens");
        assert_eq!(request.output_limit_value(), Some(&json!(9000)));
    }

    #[test]
    fn missing_chat_limit_returns_parameter_specific_422_for_budget_mapping() {
        let config = token_budget_config();
        let mut body = json!({
            "model": "model",
            "messages": [],
            "reasoning_effort": "high"
        });

        let error = config.apply("/chat/completions", &mut body).unwrap_err();

        assert_eq!(error.status_code(), 422);
        assert_eq!(error.param(), Some("max_completion_tokens"));
        assert_eq!(error.code(), "reasoning_budget_requires_max_tokens");
        assert_eq!(
            error.message(),
            "reasoning_effort 'high' maps to an 8192-token reasoning budget for this model, but max_completion_tokens is not set. Set max_completion_tokens above 8192 or select a lower reasoning_effort."
        );
    }

    #[test]
    fn null_max_completion_tokens_falls_back_to_legacy_max_tokens() {
        let config = token_budget_config();
        let mut body = json!({
            "model": "model",
            "messages": [],
            "reasoning_effort": "high",
            "max_completion_tokens": null,
            "max_tokens": 9000
        });

        config.apply("/chat/completions", &mut body).unwrap();

        assert_eq!(body["reasoning_effort"], json!("high"));
        assert_eq!(body["thinking_token_budget"], json!(8192));
    }

    #[test]
    fn reasoning_budget_equal_to_output_limit_returns_400() {
        let config = token_budget_config();
        let mut body = json!({
            "model": "model",
            "messages": [],
            "reasoning_effort": "high",
            "max_completion_tokens": 8192
        });

        let error = config.apply("/chat/completions", &mut body).unwrap_err();

        assert_eq!(error.status_code(), 400);
        assert_eq!(error.param(), Some("max_completion_tokens"));
        assert_eq!(error.code(), "reasoning_budget_exceeds_max_tokens");
        assert_eq!(
            error.message(),
            "reasoning_effort 'high' maps to an 8192-token reasoning budget for this model, but max_completion_tokens is 8192. Increase max_completion_tokens above 8192 or select a lower reasoning_effort."
        );
    }

    #[test]
    fn malformed_output_limit_is_rejected_only_for_budget_mappings() {
        let config = token_budget_config();
        let mut budget_body = json!({
            "model": "model",
            "messages": [],
            "reasoning_effort": "high",
            "max_completion_tokens": "many"
        });

        let error = config
            .apply("/chat/completions", &mut budget_body)
            .unwrap_err();
        assert_eq!(error.status_code(), 400);
        assert_eq!(error.param(), Some("max_completion_tokens"));
        assert_eq!(error.code(), "invalid_type");

        let mut binary_body = json!({
            "model": "model",
            "messages": [],
            "reasoning_effort": "none",
            "max_completion_tokens": "many"
        });
        sglang_config()
            .apply("/chat/completions", &mut binary_body)
            .unwrap();
    }

    #[test]
    fn responses_budget_errors_name_original_openai_parameters() {
        let config = responses_token_budget_config();
        let mut body = json!({
            "model": "model",
            "input": "Hello",
            "reasoning": {"effort": "high"},
            "max_output_tokens": 4096
        });

        let error = config.apply("/responses", &mut body).unwrap_err();

        assert_eq!(error.status_code(), 400);
        assert_eq!(error.param(), Some("max_output_tokens"));
        assert_eq!(error.code(), "reasoning_budget_exceeds_max_tokens");
        assert_eq!(
            error.message(),
            "reasoning.effort 'high' maps to an 8192-token reasoning budget for this model, but max_output_tokens is 4096. Increase max_output_tokens above 8192 or select a lower reasoning.effort."
        );
    }

    #[test]
    fn rejects_reasoning_on_legacy_completions() {
        let body = json!({"model": "model", "prompt": "Hello", "reasoning_effort": "low"});

        let error = validate_canonical_reasoning("/completions", &body).unwrap_err();

        assert_eq!(error.param(), Some("reasoning_effort"));
        assert_eq!(error.code(), "unsupported_parameter");
    }

    #[test]
    fn rejects_provider_reasoning_controls_on_legacy_completions() {
        let body = json!({"model": "model", "prompt": "Hello", "thinking": false});

        let error = validate_canonical_reasoning("/completions", &body).unwrap_err();

        assert_eq!(error.param(), Some("thinking"));
        assert_eq!(error.code(), "unsupported_parameter");
    }

    #[test]
    fn rejects_client_supplied_thinking_token_budget_on_all_openai_surfaces() {
        for (path, body) in [
            (
                "/chat/completions",
                json!({"model": "model", "messages": [], "thinking_token_budget": 1024}),
            ),
            (
                "/responses",
                json!({"model": "model", "input": "Hello", "thinking_token_budget": 1024}),
            ),
            (
                "/completions",
                json!({"model": "model", "prompt": "Hello", "thinking_token_budget": 1024}),
            ),
        ] {
            let error = validate_canonical_reasoning(path, &body).unwrap_err();

            assert_eq!(error.param(), Some("thinking_token_budget"));
            assert_eq!(error.code(), "unsupported_parameter");
        }
    }

    #[test]
    fn rejects_provider_native_chat_thinking_controls() {
        let body = json!({
            "model": "model",
            "messages": [],
            "chat_template_kwargs": {"thinking": false}
        });

        let error = validate_canonical_reasoning("/chat/completions", &body).unwrap_err();

        assert_eq!(error.param(), Some("chat_template_kwargs.thinking"));
        assert_eq!(error.code(), "unsupported_parameter");
        assert!(error.message().contains("reasoning_effort"));
    }

    #[test]
    fn rejects_chat_reasoning_provider_extension() {
        let body = json!({
            "model": "model",
            "messages": [],
            "reasoning": {"effort": "low"}
        });

        let error = validate_canonical_reasoning("/chat/completions", &body).unwrap_err();

        assert_eq!(error.param(), Some("reasoning"));
        assert_eq!(error.code(), "unsupported_parameter");
    }

    #[test]
    fn rejects_translation_paths_outside_reasoning_fields() {
        let config: ReasoningTranslationConfig = serde_json::from_value(json!({
            "chat_completions": {
                "unsupported_efforts": ["minimal", "low", "medium", "high", "xhigh", "max"],
                "writes": [{
                    "target_path": "/messages/0/content",
                    "values": {"none": "replaced"}
                }]
            }
        }))
        .unwrap();

        let error = config.validate().unwrap_err();

        assert_eq!(error.code(), "invalid_translation_config");
        assert!(error.message().contains("target_path"));
    }

    #[test]
    fn rejects_non_reasoning_chat_template_kwargs_paths() {
        let config: ReasoningTranslationConfig = serde_json::from_value(json!({
            "chat_completions": {
                "unsupported_efforts": ["minimal", "low", "medium", "high", "xhigh", "max"],
                "writes": [{
                    "target_path": "/chat_template_kwargs/template",
                    "values": {"none": "replacement"}
                }]
            }
        }))
        .unwrap();

        let error = config.validate().unwrap_err();

        assert_eq!(error.code(), "invalid_translation_config");
        assert!(error.message().contains("target_path"));
    }

    #[test]
    fn rejects_excessively_deep_target_paths() {
        let config: ReasoningTranslationConfig = serde_json::from_value(json!({
            "chat_completions": {
                "unsupported_efforts": ["minimal", "low", "medium", "high", "xhigh", "max"],
                "writes": [{
                    "target_path": "/thinking/a/b/c/d/e/f/g/h",
                    "values": {"none": false}
                }]
            }
        }))
        .unwrap();

        let error = config.validate().unwrap_err();

        assert_eq!(error.code(), "invalid_translation_config");
        assert!(error.message().contains("target_path"));
    }

    #[test]
    fn rejects_multi_write_effort_set_mismatches() {
        let config: ReasoningTranslationConfig = serde_json::from_value(json!({
            "chat_completions": {
                "unsupported_efforts": ["minimal", "medium", "xhigh", "max"],
                "writes": [
                    {
                        "target_path": "/reasoning_effort",
                        "values": {"none": "none", "low": "low", "high": "high"}
                    },
                    {
                        "target_path": "/thinking_token_budget",
                        "values": {"none": 0, "low": 1024}
                    }
                ]
            }
        }))
        .unwrap();

        let error = config.validate().unwrap_err();

        assert!(
            error
                .message()
                .contains("exactly the same reasoning efforts")
        );
    }

    #[test]
    fn rejects_duplicate_write_target_paths() {
        let config: ReasoningTranslationConfig = serde_json::from_value(json!({
            "chat_completions": {
                "unsupported_efforts": ["minimal", "low", "medium", "high", "xhigh", "max"],
                "writes": [
                    {"target_path": "/thinking", "values": {"none": false}},
                    {"target_path": "/thinking", "values": {"none": true}}
                ]
            }
        }))
        .unwrap();

        let error = config.validate().unwrap_err();

        assert!(error.message().contains("unique"));
    }

    #[test]
    fn rejects_overlapping_write_target_paths_in_either_order() {
        for target_paths in [
            ["/thinking", "/thinking/mode"],
            ["/thinking/mode", "/thinking"],
        ] {
            let config: ReasoningTranslationConfig = serde_json::from_value(json!({
                "chat_completions": {
                    "unsupported_efforts": ["minimal", "low", "medium", "high", "xhigh", "max"],
                    "writes": [
                        {"target_path": target_paths[0], "values": {"none": false}},
                        {"target_path": target_paths[1], "values": {"none": false}}
                    ]
                }
            }))
            .unwrap();

            let error = config.validate().unwrap_err();

            assert_eq!(error.code(), "invalid_translation_config");
            assert!(error.message().contains("overlap"));
        }
    }

    #[test]
    fn rejects_overlapping_mapped_and_unsupported_efforts() {
        let config: ReasoningTranslationConfig = serde_json::from_value(json!({
            "chat_completions": {
                "unsupported_efforts": ["none", "minimal", "low", "medium", "high", "xhigh", "max"],
                "writes": [{"target_path": "/thinking", "values": {"none": false}}]
            }
        }))
        .unwrap();

        let error = config.validate().unwrap_err();

        assert!(error.message().contains("must not overlap"));
    }

    #[test]
    fn rejects_incomplete_openai_effort_accounting() {
        let config: ReasoningTranslationConfig = serde_json::from_value(json!({
            "chat_completions": {
                "unsupported_efforts": ["minimal", "low", "medium", "high", "xhigh"],
                "writes": [{"target_path": "/thinking", "values": {"none": false}}]
            }
        }))
        .unwrap();

        let error = config.validate().unwrap_err();

        assert!(error.message().contains("every OpenAI reasoning effort"));
    }

    #[test]
    fn rejects_non_integer_thinking_token_budgets() {
        let config: ReasoningTranslationConfig = serde_json::from_value(json!({
            "chat_completions": {
                "unsupported_efforts": ["minimal", "low", "medium", "high", "xhigh", "max"],
                "writes": [
                    {"target_path": "/reasoning_effort", "values": {"none": "none"}},
                    {"target_path": "/thinking_token_budget", "values": {"none": 1.5}}
                ]
            }
        }))
        .unwrap();

        let error = config.validate().unwrap_err();

        assert!(error.message().contains("non-negative integers"));
    }

    #[test]
    fn rejects_budget_mapping_without_reasoning_effort_activation() {
        let config: ReasoningTranslationConfig = serde_json::from_value(json!({
            "chat_completions": {
                "unsupported_efforts": ["minimal", "low", "medium", "high", "xhigh", "max"],
                "writes": [{"target_path": "/thinking_token_budget", "values": {"none": 0}}]
            }
        }))
        .unwrap();

        let error = config.validate().unwrap_err();

        assert!(
            error
                .message()
                .contains("requires a reasoning_effort write")
        );
    }
}
