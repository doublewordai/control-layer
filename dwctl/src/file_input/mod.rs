//! Normalisation of Responses `input_file` content (PDF / document inputs).
//!
//! The OpenAI-compatible Chat Completions `file` content part can only carry
//! inline bytes (`file_data`) or a provider-owned `file_id` - it has no URL
//! form (unlike `image_url`). So an `input_file` submitted to `/v1/responses`
//! with a `file_url` cannot survive the Responses-to-Chat-Completions
//! conversion as a URL: the bytes have to be fetched and inlined first.
//!
//! This module rewrites each `input_file` content part in a Responses request
//! body before it reaches onwards:
//!
//! - `file_url` -> fetch through the hardened [`ImageFetcher`] (the same
//!   SSRF-protected fetcher used for image URLs: DNS pinning, IP deny-list,
//!   redirect re-validation, MIME / size caps) and inline the bytes as a
//!   base64 `file_data` data URI, dropping `file_url`.
//! - `file_data` -> left untouched; onwards already maps it to a Chat
//!   Completions `file` part.
//! - `file_id` (with no `file_data`) -> rejected. dwctl customers never upload
//!   to the upstream provider, so a bare `file_id` always refers to dwctl's own
//!   file store, which is not yet wired to resolve Responses inputs. Returning
//!   a clear error beats forwarding an id the provider can never resolve.
//!
//! The fetch is injected as a callback (mirroring
//! [`crate::image_normalizer::walker::substitute_with`]) so the rewrite logic
//! can be tested without a live fetch and the middleware can supply the real
//! hardened fetcher.
use std::future::Future;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::image_normalizer::NormalizeError;
use crate::image_normalizer::config::FetcherConfig;
use crate::image_normalizer::fetcher::ImageFetcher;

/// Configuration for `input_file` normalisation.
///
/// Default = disabled. Enabling it lets dwctl make outbound fetches to
/// user-supplied `file_url`s (through the hardened fetcher), so it is opt-in,
/// mirroring the image normaliser. When disabled, an `input_file.file_url`
/// flows to onwards untouched and onwards returns a clear error in strict mode
/// rather than silently dropping it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInputConfig {
    /// Master switch for `file_url` fetch-and-inline.
    #[serde(default)]
    pub enabled: bool,

    /// Hardened-fetcher policy. Defaults to a document MIME allow-list.
    #[serde(default = "default_file_fetcher")]
    pub fetcher: FetcherConfig,
}

impl Default for FileInputConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            fetcher: default_file_fetcher(),
        }
    }
}

/// The image fetcher's defaults are sound for documents too (size cap,
/// timeouts, redirect re-validation, IP deny-list); only the accepted MIME
/// types differ.
///
/// Only `application/pdf` is allowed by default: PDF is the dominant document
/// input and the one with broad upstream model support, so we start narrow and
/// let operators widen `allowed_mime` (e.g. for text/csv/docx) when their
/// upstream actually accepts those types. Note the fetched bytes are inlined as
/// base64, inflating the on-wire body by ~33%, so operators enabling this
/// should keep `max_bytes` aligned with their upstream request-size limits.
fn default_file_fetcher() -> FetcherConfig {
    FetcherConfig {
        allowed_mime: vec!["application/pdf".to_string()],
        ..FetcherConfig::default()
    }
}

/// Rewrite every `input_file` content part in a Responses request `body`,
/// fetching+inlining `file_url`s and rejecting unresolvable references.
///
/// `fetch` is handed each `file_url` and must return the resolved
/// `(mime, bytes)`; it carries the SSRF-hardened fetch in production.
/// Returns the number of parts rewritten. Substitutions run sequentially in
/// document order; the first error stops the walk.
pub async fn normalize_input_files<F, Fut>(body: &mut Value, mut fetch: F) -> Result<usize, NormalizeError>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<(String, Bytes), NormalizeError>>,
{
    // Responses shape: input[*].content[*] with `type == "input_file"`.
    let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return Ok(0);
    };
    let mut count = 0usize;
    for item in input {
        let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for part in content {
            if part.get("type").and_then(Value::as_str) != Some("input_file") {
                continue;
            }
            // Already-inline bytes: onwards maps these straight to a file part.
            if part.get("file_data").and_then(Value::as_str).is_some() {
                continue;
            }
            // A URL we can fetch and inline.
            if let Some(url) = part.get("file_url").and_then(Value::as_str).map(str::to_string) {
                let (mime, bytes) = fetch(url).await?;
                let data_uri = format!("data:{};base64,{}", mime, B64.encode(&bytes));
                let obj = part
                    .as_object_mut()
                    .ok_or_else(|| NormalizeError::BadInput("input_file content part is not a JSON object".to_string()))?;
                obj.remove("file_url");
                obj.insert("file_data".to_string(), Value::String(data_uri));
                count += 1;
                continue;
            }
            // A bare dwctl-owned file_id we cannot resolve yet.
            if part.get("file_id").and_then(Value::as_str).is_some() {
                return Err(NormalizeError::BadInput(
                    "input_file.file_id is not supported yet; send the document inline via \
                     file_data or as a fetchable file_url"
                        .to_string(),
                ));
            }
            // Nothing usable on the part at all.
            return Err(NormalizeError::BadInput(
                "input_file must include one of file_data, file_url, or file_id".to_string(),
            ));
        }
    }
    Ok(count)
}

/// Run [`normalize_input_files`] using the hardened [`ImageFetcher`] to fetch
/// each `file_url`. Shared by the realtime `file_input` layer and the
/// warm-path / flex normalisation in `responses_middleware`.
pub async fn normalize_input_files_with_fetcher(body: &mut Value, fetcher: &ImageFetcher) -> Result<usize, NormalizeError> {
    normalize_input_files(body, |url| {
        let fetcher = fetcher.clone();
        async move {
            fetcher
                .fetch(&url)
                .await
                .map(|fetched| (fetched.mime, fetched.bytes))
                .map_err(NormalizeError::from)
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_normalizer::fetcher::ImageFetcher;
    use serde_json::json;

    /// A stub fetch returning fixed bytes; asserts the walker never reaches the
    /// network for the cases that shouldn't fetch.
    fn fake_pdf(_url: String) -> impl Future<Output = Result<(String, Bytes), NormalizeError>> {
        async { Ok(("application/pdf".to_string(), Bytes::from_static(b"%PDF-1.4 fake"))) }
    }

    fn responses_body_with(part: Value) -> Value {
        json!({
            "model": "doc-qa",
            "input": [{
                "role": "user",
                "content": [
                    { "type": "input_text", "text": "summarise" },
                    part
                ]
            }]
        })
    }

    #[tokio::test]
    async fn fetches_file_url_and_inlines_as_base64_file_data() {
        let mut body = responses_body_with(json!({
            "type": "input_file",
            "filename": "doc.pdf",
            "file_url": "https://example.com/doc.pdf"
        }));

        let n = normalize_input_files(&mut body, fake_pdf).await.unwrap();
        assert_eq!(n, 1);

        let part = &body["input"][0]["content"][1];
        assert!(part.get("file_url").is_none(), "file_url should be dropped");
        let data = part["file_data"].as_str().expect("file_data should be set");
        assert_eq!(data, format!("data:application/pdf;base64,{}", B64.encode(b"%PDF-1.4 fake")));
        assert_eq!(part["filename"], "doc.pdf", "filename preserved");
    }

    #[tokio::test]
    async fn file_data_passes_through_untouched_and_does_not_fetch() {
        let mut body = responses_body_with(json!({
            "type": "input_file",
            "filename": "doc.pdf",
            "file_data": "data:application/pdf;base64,QUJD"
        }));
        let n = normalize_input_files(&mut body, |_| async { panic!("must not fetch when file_data is already present") })
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert_eq!(body["input"][0]["content"][1]["file_data"], "data:application/pdf;base64,QUJD");
    }

    #[tokio::test]
    async fn bare_file_id_is_rejected() {
        let mut body = responses_body_with(json!({
            "type": "input_file",
            "file_id": "file-abc123"
        }));
        let err = normalize_input_files(&mut body, fake_pdf).await.unwrap_err();
        assert!(
            matches!(err, NormalizeError::BadInput(ref m) if m.contains("file_id")),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn no_input_file_is_a_noop() {
        let mut body = json!({
            "model": "doc-qa",
            "input": [{ "role": "user", "content": [{ "type": "input_text", "text": "hi" }] }]
        });
        let n = normalize_input_files(&mut body, fake_pdf).await.unwrap();
        assert_eq!(n, 0);
    }

    #[tokio::test]
    async fn link_local_file_url_is_rejected_by_real_ssrf_guard() {
        // Drive the actual hardened fetcher: the IP deny-list must reject the
        // cloud-metadata address before any bytes are inlined.
        let fetcher = ImageFetcher::new(FetcherConfig {
            allowed_mime: vec!["application/pdf".to_string()],
            ..FetcherConfig::default()
        });
        let mut body = responses_body_with(json!({
            "type": "input_file",
            "file_url": "http://169.254.169.254/latest/meta-data/"
        }));
        let err = normalize_input_files(&mut body, |url| {
            let fetcher = fetcher.clone();
            async move { fetcher.fetch(&url).await.map(|f| (f.mime, f.bytes)).map_err(Into::into) }
        })
        .await
        .unwrap_err();
        assert!(matches!(err, NormalizeError::BadInput(_)), "unexpected error: {err}");
    }
}
