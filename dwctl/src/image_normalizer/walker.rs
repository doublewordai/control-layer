//! Walks a parsed request body and applies a substitution callback to each
//! image input field that needs normalising.
//!
//! Handles both supported endpoint shapes:
//!
//! - **chat-completions**: `messages[*].content[*]` where the item is an
//!   object with `type == "image_url"` and a nested `image_url.url` string.
//! - **responses**: `input[*].content[*]` where the item is an object with
//!   `type == "input_image"` and a bare `image_url` string (not nested
//!   under a `.url` field).
//!
//! Two operating modes:
//!
//! - [`Mode::HttpOnly`] — substitute values starting with `http://` or
//!   `https://` only. `data:` URIs and other schemes pass through.
//! - [`Mode::All`] — additionally substitute `data:` URIs (the opt-in
//!   "image privacy" mode).
//!
//! The walker also has a third operating mode, [`Mode::TokensOnly`], used
//! at dispatch time: it only touches values that already look like
//! `dw-img://...` opaque tokens — swapping them for freshly-signed URLs
//! without re-running ingest.
use serde_json::Value;
use std::future::Future;

use super::token::ImageToken;

/// Which inputs the walker should hand to the substitution callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Only HTTP(S) URLs. Used by default for users who haven't opted into
    /// full normalisation.
    HttpOnly,
    /// HTTP(S) URLs and `data:` URIs. Used when the calling user has the
    /// per-account opt-in enabled.
    All,
    /// Only opaque `dw-img://` tokens. Used at dispatch time to swap
    /// tokens for fresh signed URLs.
    TokensOnly,
}

impl Mode {
    fn applies_to(self, input: &str) -> bool {
        match self {
            Mode::HttpOnly => is_http_url(input),
            Mode::All => is_http_url(input) || crate::image_normalizer::data_uri::looks_like_data_uri(input),
            Mode::TokensOnly => ImageToken::looks_like_token(input),
        }
    }
}

fn is_http_url(s: &str) -> bool {
    let lower = &s[..s.len().min(8)].to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// Walks `body` and, for each image input matching `mode`, calls
/// `substitute(value)` — replacing the JSON string in-place with the
/// returned string.
///
/// The callback is async to allow it to perform fetches / signing /
/// store lookups. Substitutions are performed sequentially in document
/// order; if a callback returns `Err`, the walker stops and the partial
/// state of the body is unspecified.
pub async fn substitute_with<F, Fut, E>(body: &mut Value, mode: Mode, mut substitute: F) -> Result<usize, E>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<String, E>>,
{
    let mut count = 0usize;

    // chat-completions shape: messages[*].content[*].image_url.url
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for msg in messages {
            count += visit_content_array_chat_completions(msg, mode, &mut substitute).await?;
        }
    }

    // responses shape: input[*].content[*].image_url
    if let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) {
        for item in input {
            count += visit_content_array_responses(item, mode, &mut substitute).await?;
        }
    }

    Ok(count)
}

async fn visit_content_array_chat_completions<F, Fut, E>(msg: &mut Value, mode: Mode, substitute: &mut F) -> Result<usize, E>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<String, E>>,
{
    let Some(content) = msg.get_mut("content").and_then(Value::as_array_mut) else {
        return Ok(0);
    };
    let mut count = 0usize;
    for item in content {
        // The chat-completions shape: { "type": "image_url", "image_url": { "url": "..." } }
        let is_image_url_item = item.get("type").and_then(Value::as_str) == Some("image_url");
        if !is_image_url_item {
            continue;
        }
        let Some(image_url_obj) = item.get_mut("image_url") else {
            continue;
        };
        let Some(url_value) = image_url_obj.get_mut("url") else {
            continue;
        };
        let Some(url_str) = url_value.as_str() else {
            continue;
        };
        if !mode.applies_to(url_str) {
            continue;
        }
        let replacement = substitute(url_str.to_string()).await?;
        *url_value = Value::String(replacement);
        count += 1;
    }
    Ok(count)
}

async fn visit_content_array_responses<F, Fut, E>(item: &mut Value, mode: Mode, substitute: &mut F) -> Result<usize, E>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<String, E>>,
{
    let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) else {
        return Ok(0);
    };
    let mut count = 0usize;
    for part in content {
        // The responses shape: { "type": "input_image", "image_url": "..." }
        let is_input_image = part.get("type").and_then(Value::as_str) == Some("input_image");
        if !is_input_image {
            continue;
        }
        let Some(image_url_value) = part.get_mut("image_url") else {
            continue;
        };
        let Some(url_str) = image_url_value.as_str() else {
            continue;
        };
        if !mode.applies_to(url_str) {
            continue;
        }
        let replacement = substitute(url_str.to_string()).await?;
        *image_url_value = Value::String(replacement);
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::convert::Infallible;

    /// Substitution callback that just prefixes the input — easy to assert
    /// against and never errors.
    async fn prefix_with(prefix: &'static str, url: String) -> Result<String, Infallible> {
        Ok(format!("{prefix}:{url}"))
    }

    #[tokio::test]
    async fn http_only_substitutes_http_in_chat_completions_shape() {
        let mut body = json!({
            "model": "vision",
            "messages": [
                {
                    "role": "user",
                    "content": [
                        { "type": "text", "text": "describe" },
                        { "type": "image_url", "image_url": { "url": "https://example.com/a.png" } },
                        { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } }
                    ]
                }
            ]
        });

        let count = substitute_with(&mut body, Mode::HttpOnly, |u| prefix_with("X", u)).await.unwrap();

        assert_eq!(count, 1);
        let content = &body["messages"][0]["content"];
        assert_eq!(content[1]["image_url"]["url"], "X:https://example.com/a.png");
        assert_eq!(content[2]["image_url"]["url"], "data:image/png;base64,AAAA"); // untouched
    }

    #[tokio::test]
    async fn all_mode_substitutes_data_uris_too() {
        let mut body = json!({
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "image_url", "image_url": { "url": "https://example.com/a.png" } },
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } }
                ]
            }]
        });

        let count = substitute_with(&mut body, Mode::All, |u| prefix_with("Y", u)).await.unwrap();

        assert_eq!(count, 2);
        let content = &body["messages"][0]["content"];
        assert_eq!(content[0]["image_url"]["url"], "Y:https://example.com/a.png");
        assert_eq!(content[1]["image_url"]["url"], "Y:data:image/png;base64,AAAA");
    }

    #[tokio::test]
    async fn substitutes_responses_input_image_shape() {
        let mut body = json!({
            "model": "vision",
            "input": [
                {
                    "role": "user",
                    "content": [
                        { "type": "input_text", "text": "what is this" },
                        { "type": "input_image", "image_url": "https://example.com/b.png" }
                    ]
                }
            ]
        });

        let count = substitute_with(&mut body, Mode::HttpOnly, |u| prefix_with("R", u)).await.unwrap();

        assert_eq!(count, 1);
        assert_eq!(body["input"][0]["content"][1]["image_url"], "R:https://example.com/b.png");
    }

    #[tokio::test]
    async fn skips_non_image_content_items() {
        let mut body = json!({
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "text", "text": "no image here" },
                    { "type": "input_audio", "input_audio": { "data": "AAA", "format": "wav" } }
                ]
            }]
        });

        let count = substitute_with(&mut body, Mode::All, |u| prefix_with("Z", u)).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn skips_string_content_field() {
        // Plain old "content": "hello" — not an array. Walker must not crash.
        let mut body = json!({
            "messages": [{ "role": "user", "content": "hello world" }]
        });
        let count = substitute_with(&mut body, Mode::HttpOnly, |u| prefix_with("Z", u)).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn tokens_only_swaps_dw_img_uris() {
        let mut body = json!({
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "image_url", "image_url": { "url": "https://example.com/x.png" } },
                    { "type": "image_url", "image_url": { "url": "dw-img://0000000000000000000000000000000000000000000000000000000000000001" } }
                ]
            }]
        });

        let count = substitute_with(&mut body, Mode::TokensOnly, |u| prefix_with("S", u)).await.unwrap();

        assert_eq!(count, 1);
        let content = &body["messages"][0]["content"];
        // http url untouched in TokensOnly mode
        assert_eq!(content[0]["image_url"]["url"], "https://example.com/x.png");
        assert!(content[1]["image_url"]["url"].as_str().unwrap().starts_with("S:dw-img://"));
    }

    #[tokio::test]
    async fn case_insensitive_http_scheme() {
        let mut body = json!({
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "image_url", "image_url": { "url": "HTTP://example.com/a.png" } }
                ]
            }]
        });
        let count = substitute_with(&mut body, Mode::HttpOnly, |u| prefix_with("X", u)).await.unwrap();
        assert_eq!(count, 1);
    }
}
