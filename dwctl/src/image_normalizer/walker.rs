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
//! Two more modes cover `dw-img://...` opaque tokens (what the flex enqueue
//! and file-ingest paths store): [`Mode::TokensOnly`] touches only tokens,
//! and [`Mode::AllAndTokens`] — the edge middleware's mode — touches every
//! kind, so a daemon loopback gets its tokens swapped for freshly-signed
//! URLs (without re-running ingest) in the same pass that normalises a
//! client's URLs and data URIs.
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
    /// Only opaque `dw-img://` tokens: swap tokens for fresh signed URLs.
    TokensOnly,
    /// Everything: HTTP(S) URLs, `data:` URIs AND `dw-img://` tokens. The
    /// edge middleware's mode — a daemon loopback carries the tokens that
    /// flex enqueue / file ingest stored, and signing them here (below the
    /// prompt-cache layer) is what keeps the cache identity of an image the
    /// stable content-addressed token rather than a per-dispatch signed URL.
    AllAndTokens,
}

impl Mode {
    fn applies_to(self, input: &str) -> bool {
        match self {
            Mode::HttpOnly => is_http_url(input),
            Mode::All => is_http_url(input) || crate::image_normalizer::data_uri::looks_like_data_uri(input),
            Mode::TokensOnly => ImageToken::looks_like_token(input),
            Mode::AllAndTokens => Mode::All.applies_to(input) || ImageToken::looks_like_token(input),
        }
    }
}

fn is_http_url(s: &str) -> bool {
    s.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("http://")) || s.get(..8).is_some_and(|p| p.eq_ignore_ascii_case("https://"))
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

/// Whether `body` carries at least one image input that `mode` would act on
/// — the read-only twin of [`substitute_with`], over the same two shapes. Lets
/// a caller skip work (a caller lookup, say) for the common image-free body.
pub fn has_inputs(body: &Value, mode: Mode) -> bool {
    // chat-completions shape: messages[*].content[*].image_url.url
    let chat = body.get("messages").and_then(Value::as_array).is_some_and(|messages| {
        messages.iter().any(|msg| {
            msg.get("content").and_then(Value::as_array).is_some_and(|content| {
                content.iter().any(|item| {
                    item.get("type").and_then(Value::as_str) == Some("image_url")
                        && item
                            .get("image_url")
                            .and_then(|o| o.get("url"))
                            .and_then(Value::as_str)
                            .is_some_and(|url| mode.applies_to(url))
                })
            })
        })
    });
    // responses shape: input[*].content[*].image_url
    let responses = body.get("input").and_then(Value::as_array).is_some_and(|input| {
        input.iter().any(|item| {
            item.get("content").and_then(Value::as_array).is_some_and(|content| {
                content.iter().any(|part| {
                    part.get("type").and_then(Value::as_str) == Some("input_image")
                        && part
                            .get("image_url")
                            .and_then(Value::as_str)
                            .is_some_and(|url| mode.applies_to(url))
                })
            })
        })
    });
    chat || responses
}

/// Every `dw-img://` token in `body`, in document order, over the same two
/// shapes as [`substitute_with`] — so a caller can authorise them all in one
/// query before the walk signs them. Strings that merely look like a token but
/// do not parse are skipped here; the walk itself rejects them as bad input.
pub fn tokens(body: &Value) -> Vec<ImageToken> {
    fn push(url: Option<&str>, out: &mut Vec<ImageToken>) {
        if let Some(url) = url
            && ImageToken::looks_like_token(url)
            && let Ok(token) = url.parse::<ImageToken>()
        {
            out.push(token);
        }
    }
    let mut out = Vec::new();
    // chat-completions shape: messages[*].content[*].image_url.url
    for msg in body.get("messages").and_then(Value::as_array).into_iter().flatten() {
        for item in msg.get("content").and_then(Value::as_array).into_iter().flatten() {
            if item.get("type").and_then(Value::as_str) == Some("image_url") {
                push(item.get("image_url").and_then(|o| o.get("url")).and_then(Value::as_str), &mut out);
            }
        }
    }
    // responses shape: input[*].content[*].image_url
    for item in body.get("input").and_then(Value::as_array).into_iter().flatten() {
        for part in item.get("content").and_then(Value::as_array).into_iter().flatten() {
            if part.get("type").and_then(Value::as_str) == Some("input_image") {
                push(part.get("image_url").and_then(Value::as_str), &mut out);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::convert::Infallible;

    #[test]
    fn tokens_collects_every_token_over_both_shapes_and_nothing_else() {
        let a = ImageToken([1u8; 32]);
        let b = ImageToken([2u8; 32]);
        let body = json!({
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "hi"},
                {"type": "image_url", "image_url": {"url": a.to_dw_img_uri()}},
                {"type": "image_url", "image_url": {"url": "https://x/a.png"}}
            ]}],
            "input": [{"role": "user", "content": [
                {"type": "input_image", "image_url": b.to_dw_img_uri()},
                {"type": "input_image", "image_url": "dw-img://not-hex"}
            ]}]
        });
        assert_eq!(tokens(&body), vec![a, b]);
        assert!(tokens(&json!({"messages": [{"role": "user", "content": "hi"}]})).is_empty());
    }

    #[test]
    fn has_inputs_mirrors_the_walker_over_both_shapes() {
        let chat = json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "hi"},
            {"type": "image_url", "image_url": {"url": "https://x/a.png"}}
        ]}]});
        let responses = json!({"input": [{"role": "user", "content": [
            {"type": "input_image", "image_url": "data:image/png;base64,AAAA"}
        ]}]});
        let text_only = json!({"messages": [{"role": "user", "content": "hi"}]});
        let token_only = json!({"messages": [{"role": "user", "content": [
            {"type": "image_url", "image_url": {"url": "dw-img://0000000000000000000000000000000000000000000000000000000000000000"}}
        ]}]});

        assert!(has_inputs(&chat, Mode::All));
        assert!(has_inputs(&responses, Mode::All));
        assert!(!has_inputs(&text_only, Mode::All));
        // Mode decides: a token is not an `All` input, but is an `AllAndTokens` one.
        assert!(!has_inputs(&token_only, Mode::All));
        assert!(has_inputs(&token_only, Mode::AllAndTokens));
        // A data URI is not an `HttpOnly` input.
        assert!(!has_inputs(&responses, Mode::HttpOnly));
    }

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

    /// The edge middleware's mode: a daemon loopback carries the tokens that
    /// enqueue stored alongside whatever a client may echo back, so every kind
    /// of image input must be handed to the callback in document order.
    #[tokio::test]
    async fn all_and_tokens_mode_substitutes_every_kind() {
        let mut body = json!({
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "image_url", "image_url": { "url": "https://example.com/x.png" } },
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } },
                    { "type": "image_url", "image_url": { "url": "dw-img://0000000000000000000000000000000000000000000000000000000000000001" } }
                ]
            }]
        });

        let count = substitute_with(&mut body, Mode::AllAndTokens, |u| prefix_with("S", u))
            .await
            .unwrap();

        assert_eq!(count, 3);
        let content = &body["messages"][0]["content"];
        assert!(content[0]["image_url"]["url"].as_str().unwrap().starts_with("S:https://"));
        assert!(content[1]["image_url"]["url"].as_str().unwrap().starts_with("S:data:"));
        assert!(content[2]["image_url"]["url"].as_str().unwrap().starts_with("S:dw-img://"));
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

    /// Regression for the byte-slice panic in `is_http_url`: a non-ASCII
    /// image-URL string whose byte index 8 falls inside a multi-byte UTF-8
    /// codepoint must be skipped (return `false`) instead of panicking.
    #[test]
    fn non_ascii_url_does_not_panic() {
        assert!(!Mode::HttpOnly.applies_to("1234567é"));
        assert!(!Mode::All.applies_to("1234567é"));
        assert!(!is_http_url("1234567é"));
    }

    /// A real `http://` URL whose host contains a non-ASCII char within the
    /// first 8 bytes (the original exploit string) is still recognised as an
    /// http URL — the fix must not regress the happy path.
    #[test]
    fn http_url_with_non_ascii_host_is_recognised() {
        assert!(is_http_url("http://é.example/x.png"));
        assert!(Mode::HttpOnly.applies_to("http://é.example/x.png"));
        assert!(Mode::All.applies_to("http://é.example/x.png"));
    }

    /// A non-http(s) URL with non-ASCII content is skipped end-to-end
    /// through the walker, which leaves the value untouched.
    #[tokio::test]
    async fn non_ascii_non_http_url_passes_through_unchanged() {
        let mut body = json!({
            "messages": [{
                "role": "user",
                "content": [
                    { "type": "image_url", "image_url": { "url": "1234567é" } },
                    { "type": "image_url", "image_url": { "url": "ftp://é.example/x.png" } }
                ]
            }]
        });
        let count = substitute_with(&mut body, Mode::All, |u| prefix_with("Z", u)).await.unwrap();
        assert_eq!(count, 0);
        let content = &body["messages"][0]["content"];
        assert_eq!(content[0]["image_url"]["url"], "1234567é");
        assert_eq!(content[1]["image_url"]["url"], "ftp://é.example/x.png");
    }
}
