//! Legacy Completions API schemas
//!
//! These schemas match the OpenAI Completions API specification (legacy text completions).
//! See: https://platform.openai.com/docs/api-reference/completions
//!
//! In strict mode, completions requests are validated against the typed schema,
//! forwarded to the upstream `/v1/completions` endpoint, and the response is
//! sanitized (unknown fields stripped, model field rewritten).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::chat_completions::{StopSequence, Usage};
use super::utils::ensure_field;

pub(crate) fn generated_completion_id() -> String {
    format!("cmpl-{}", Uuid::new_v4())
}

fn normalize_completion_response_choice_value(value: &mut Value, fallback_index: usize) {
    let Some(object) = value.as_object_mut() else {
        return;
    };

    ensure_field(object, "index", || Value::from(fallback_index));
    ensure_field(object, "logprobs", || Value::Null);
    ensure_field(object, "finish_reason", || Value::Null);
}

fn normalize_completion_chunk_choice_value(value: &mut Value, fallback_index: usize) {
    let Some(object) = value.as_object_mut() else {
        return;
    };

    ensure_field(object, "text", || Value::String(String::new()));
    ensure_field(object, "index", || Value::from(fallback_index));
    ensure_field(object, "logprobs", || Value::Null);
    ensure_field(object, "finish_reason", || Value::Null);
}

/// Backfill omitted non-critical legacy completion response fields during
/// strict sanitization. Kept out of serde defaults so only provider payloads
/// are relaxed.
pub(crate) fn normalize_completion_response_value(value: &mut Value, fallback_model: &str) {
    let Some(object) = value.as_object_mut() else {
        return;
    };

    if !object.contains_key("choices") {
        return;
    }

    ensure_field(object, "id", || Value::String(generated_completion_id()));
    ensure_field(object, "object", || {
        Value::String("text_completion".to_string())
    });
    ensure_field(object, "created", || Value::from(0));
    ensure_field(object, "model", || {
        Value::String(fallback_model.to_string())
    });
    ensure_field(object, "usage", || Value::Null);
    ensure_field(object, "system_fingerprint", || Value::Null);

    if let Some(choices) = object.get_mut("choices").and_then(Value::as_array_mut) {
        for (index, choice) in choices.iter_mut().enumerate() {
            normalize_completion_response_choice_value(choice, index);
        }
    }
}

/// Backfill omitted non-critical legacy completion chunk fields during strict
/// streaming sanitization.
pub(crate) fn normalize_completion_chunk_value(
    value: &mut Value,
    fallback_model: &str,
    fallback_id: &str,
) {
    let Some(object) = value.as_object_mut() else {
        return;
    };

    if !object.contains_key("choices") {
        return;
    }

    ensure_field(object, "id", || Value::String(fallback_id.to_string()));
    ensure_field(object, "object", || {
        Value::String("text_completion".to_string())
    });
    ensure_field(object, "created", || Value::from(0));
    ensure_field(object, "model", || {
        Value::String(fallback_model.to_string())
    });
    ensure_field(object, "usage", || Value::Null);

    if let Some(choices) = object.get_mut("choices").and_then(Value::as_array_mut) {
        for (index, choice) in choices.iter_mut().enumerate() {
            normalize_completion_chunk_choice_value(choice, index);
        }
    }
}

/// Prompt input — matches the OpenAI spec `oneOf`: string | string[] | integer[] | integer[][]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CompletionPrompt {
    Single(String),
    Multiple(Vec<String>),
    /// Pre-tokenized prompt as a flat array of token IDs
    Tokens(Vec<u32>),
    /// Pre-tokenized prompt as an array of token ID arrays (batch)
    TokenArrays(Vec<Vec<u32>>),
}

/// Request body for POST /v1/completions
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionRequest {
    /// The model to use for completion
    pub model: String,

    /// The prompt to generate completions for (optional; defaults to `<|endoftext|>` server-side)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<CompletionPrompt>,

    /// Text to append after the completion (fill-in-the-middle)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suffix: Option<String>,

    /// Maximum tokens to generate
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,

    /// Sampling temperature (0–2)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,

    /// Nucleus sampling parameter
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,

    /// Number of completions to generate
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,

    /// Whether to stream the response
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,

    /// Include log probabilities (0–5)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<u32>,

    /// Echo the prompt in the response
    #[serde(skip_serializing_if = "Option::is_none")]
    pub echo: Option<bool>,

    /// Stop sequences
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopSequence>,

    /// Presence penalty (−2.0 to 2.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,

    /// Frequency penalty (−2.0 to 2.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,

    /// Generate best_of completions server-side and return the best
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_of: Option<u32>,

    /// Logit bias for tokens
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logit_bias: Option<serde_json::Value>,

    /// User identifier for abuse tracking
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,

    /// Random seed for deterministic sampling
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,

    /// Captured only to return a useful compatibility error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<serde_json::Value>,

    /// Captured only to return a useful compatibility error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<serde_json::Value>,

    /// Captured only to return a useful compatibility error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<serde_json::Value>,

    /// Captured only to return a useful compatibility error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_token_budget: Option<serde_json::Value>,

    /// Captured only to return a useful compatibility error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<serde_json::Value>,
}

/// Response from POST /v1/completions (non-streaming)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<CompletionChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_fingerprint: Option<String>,
}

/// A single completion choice
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionChoice {
    pub text: String,
    pub index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

/// Streaming chunk from POST /v1/completions with stream=true
///
/// When `stream_options.include_usage` is set, the final chunk includes a `usage` object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionChunk {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<CompletionChunkChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<super::chat_completions::Usage>,
}

/// A single choice within a streaming completion chunk
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionChunkChoice {
    pub text: String,
    pub index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deserialize_string_prompt() {
        let json = r#"{"model": "gpt-3.5-turbo-instruct", "prompt": "Say hello"}"#;
        let req: CompletionRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(req.prompt, Some(CompletionPrompt::Single(_))));
    }

    #[test]
    fn test_deserialize_array_of_strings_prompt() {
        let json = r#"{"model": "gpt-3.5-turbo-instruct", "prompt": ["Hello", "World"]}"#;
        let req: CompletionRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(req.prompt, Some(CompletionPrompt::Multiple(_))));
    }

    #[test]
    fn test_deserialize_token_array_prompt() {
        let json = r#"{"model": "gpt-3.5-turbo-instruct", "prompt": [1, 2, 3]}"#;
        let req: CompletionRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(req.prompt, Some(CompletionPrompt::Tokens(_))));
    }

    #[test]
    fn test_deserialize_token_array_of_arrays_prompt() {
        let json = r#"{"model": "gpt-3.5-turbo-instruct", "prompt": [[1, 2], [3, 4]]}"#;
        let req: CompletionRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(req.prompt, Some(CompletionPrompt::TokenArrays(_))));
    }

    #[test]
    fn test_reject_mixed_token_array_prompt() {
        // Mixed arrays (e.g. integers and strings) don't match any variant
        let json = r#"{"model": "gpt-3.5-turbo-instruct", "prompt": [1, "hello"]}"#;
        assert!(serde_json::from_str::<CompletionRequest>(json).is_err());
    }

    #[test]
    fn test_reject_float_token_array_prompt() {
        // Floats are not integers per the spec
        let json = r#"{"model": "gpt-3.5-turbo-instruct", "prompt": [1.5, 2.5]}"#;
        assert!(serde_json::from_str::<CompletionRequest>(json).is_err());
    }

    #[test]
    fn test_deserialize_with_all_fields() {
        let json = r#"{
            "model": "gpt-3.5-turbo-instruct",
            "prompt": "Complete this",
            "suffix": "end",
            "max_tokens": 100,
            "temperature": 0.7,
            "top_p": 0.9,
            "n": 1,
            "stream": false,
            "logprobs": 3,
            "echo": true,
            "stop": "\n",
            "presence_penalty": 0.1,
            "frequency_penalty": 0.2,
            "best_of": 3,
            "user": "user-123",
            "seed": 42
        }"#;
        let req: CompletionRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.model, "gpt-3.5-turbo-instruct");
        assert_eq!(req.max_tokens, Some(100));
        assert_eq!(req.temperature, Some(0.7));
        assert_eq!(req.logprobs, Some(3));
        assert_eq!(req.echo, Some(true));
        assert_eq!(req.best_of, Some(3));
        assert_eq!(req.seed, Some(42));
    }
}
