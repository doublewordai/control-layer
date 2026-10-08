//! Engine-agnostic classification of upstream errors caused by a request's
//! media inputs (an image, video or audio URL the inference engine could not
//! fetch or decode).
//!
//! Such a failure is a property of the request, not of the provider: retrying
//! it, failing over to another provider, or counting it as overload is wrong,
//! and left alone it retries until a batch daemon's attempt cap while feeding
//! every adaptive-concurrency limiter in the path a false overload signal.
//!
//! Engines phrase the failure differently and change the phrasing between
//! versions — Python `requests` (`404 Client Error: Not Found for url: …`),
//! `aiohttp` (`404, message='Not Found', url='…'`), vLLM (`Failed to fetch
//! media from URL: HTTP 404 error`, or older `An exception occurred while
//! loading IMAGE data at index 0: …`), SGLang (`Could not decode image: …`,
//! `Timed out while downloading media URL: …`) — so this does not match any
//! engine's exact wording. It looks for three independent signals in the
//! message and combines them:
//!
//! 1. a media context word (`image`, `video`, `audio`, `media`, `multimodal`);
//! 2. a fetch/decode verb (`fetch`, `download`, `load`, `retriev`, `decode`,
//!    `for url`, `url=`) or a decode-failure phrase;
//! 3. an HTTP 4xx status token standing on its own.
//!
//! A fetch failure needs all of (1), (2) and a 4xx from (3); a decode failure
//! needs (1) and a decode phrase. `408` and `429` are transient by definition
//! and never classified. Anything else — capacity, template, context-length,
//! OOM, model-not-found — is left to the normal path.
//!
//! This is a fast path. Some gateways (NVIDIA Dynamo) send the client only a
//! public message such as `internal server error during processing` and keep
//! the engine diagnostic server-side; for those the authoritative detection is
//! the caller's own check that the request's media objects still exist.

/// What went wrong with the media input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaFailureKind {
    /// The engine fetched the media URL and got an HTTP client error.
    Fetch,
    /// The engine fetched bytes it could not decode as the declared media.
    Decode,
    /// The engine gave up waiting for the media URL. Transient: not terminal.
    Timeout,
}

impl MediaFailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaFailureKind::Fetch => "fetch",
            MediaFailureKind::Decode => "decode",
            MediaFailureKind::Timeout => "timeout",
        }
    }
}

/// A classified media-input failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaFailure {
    pub kind: MediaFailureKind,
    /// The HTTP status the engine received from the media URL, when the
    /// message carried one.
    pub upstream_status: Option<u16>,
}

impl MediaFailure {
    /// True when retrying the same request can never succeed: a 4xx from the
    /// media URL, or bytes that do not decode. A timeout is not terminal.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.kind,
            MediaFailureKind::Fetch | MediaFailureKind::Decode
        )
    }
}

const MEDIA_WORDS: &[&str] = &["image", "video", "audio", "media", "multimodal", "mm data"];
const FETCH_WORDS: &[&str] = &[
    "fetch", "download", "load", "retriev", "for url", "url=", "url:", "url '",
];
const DECODE_PHRASES: &[&str] = &[
    "could not decode",
    "cannot decode",
    "failed to decode",
    "unable to decode",
    "cannot identify image",
    "unidentifiedimageerror",
    "not a valid image",
    "invalid image",
    "image file is truncated",
    "decodeerror",
];
const TIMEOUT_PHRASES: &[&str] = &["timed out", "timeout", "time out"];

/// Classify an upstream error message. `None` means "not a media-input
/// failure as far as the text shows"; callers must treat that as ordinary.
pub fn classify_media_failure(message: &str) -> Option<MediaFailure> {
    let text = message.to_ascii_lowercase();
    if !MEDIA_WORDS.iter().any(|w| text.contains(w)) {
        return None;
    }
    if DECODE_PHRASES.iter().any(|p| text.contains(p)) {
        return Some(MediaFailure {
            kind: MediaFailureKind::Decode,
            upstream_status: None,
        });
    }
    let fetch_context = FETCH_WORDS.iter().any(|w| text.contains(w));
    if !fetch_context {
        return None;
    }
    let status = standalone_4xx(&text);
    // A request timeout or rate limit from the media host is transient by
    // definition: not ours to classify, whatever else the message says.
    if matches!(status, Some(408 | 429)) {
        return None;
    }
    if TIMEOUT_PHRASES.iter().any(|p| text.contains(p)) {
        return Some(MediaFailure {
            kind: MediaFailureKind::Timeout,
            upstream_status: None,
        });
    }
    Some(MediaFailure {
        kind: MediaFailureKind::Fetch,
        upstream_status: Some(status?),
    })
}

/// The first three-digit number in `400..=499` that stands on its own: not
/// part of a longer digit run (`4040`) and not embedded in a token of letters
/// and digits (`f481`, `8a404c13` — hashes and content-addressed paths).
fn standalone_4xx(text: &str) -> Option<u16> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let bounded = i - start == 3
                && (start == 0 || !bytes[start - 1].is_ascii_alphanumeric())
                && (i == bytes.len() || !bytes[i].is_ascii_alphanumeric());
            if bounded
                && let Ok(n) = text[start..i].parse::<u16>()
                && (400..=499).contains(&n)
            {
                return Some(n);
            }
        } else {
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fetch(status: u16) -> Option<MediaFailure> {
        Some(MediaFailure {
            kind: MediaFailureKind::Fetch,
            upstream_status: Some(status),
        })
    }

    #[test]
    fn python_requests_http_error_as_forwarded_by_vllm_and_sglang() {
        // Both engines surface `requests.raise_for_status()` text; the URL path
        // is our content-addressed `images/` prefix.
        let m = "HTTPError: 404 Client Error: Not Found for url: https://acct.r2.cloudflarestorage.com/bucket/images/ab/cd/abcd?X-Amz-Signature=deadbeef";
        assert_eq!(classify_media_failure(m), fetch(404));
        let m = "403 Client Error: Forbidden for url: https://cdn.example.com/photo.jpg (image)";
        assert_eq!(classify_media_failure(m), fetch(403));
    }

    #[test]
    fn older_vllm_wraps_the_requests_error_in_a_value_error() {
        let m = "ValueError: An exception occurred while loading IMAGE data at index 0: Error while loading data https://acct.r2.cloudflarestorage.com/b/images/f4/81/f481?x-id=GetObject: 404 Client Error: Not Found for url: https://acct.r2.cloudflarestorage.com/b/images/f4/81/f481?x-id=GetObject";
        assert_eq!(classify_media_failure(m), fetch(404));
    }

    #[test]
    fn current_vllm_media_connector_omits_the_url() {
        assert_eq!(
            classify_media_failure("Failed to fetch media from URL: HTTP 404 error"),
            fetch(404)
        );
    }

    #[test]
    fn aiohttp_client_response_error() {
        let m = "aiohttp.ClientResponseError: 404, message='Not Found', url='https://acct.r2.cloudflarestorage.com/b/images/ab/cd/abcd'";
        assert_eq!(classify_media_failure(m), fetch(404));
    }

    #[test]
    fn dynamo_multimodal_router_dimension_probe() {
        let m = "mm-routing: failed to fetch image dims; image dim fetch expected 206 Partial Content, got HTTP 404 Not Found";
        assert_eq!(classify_media_failure(m), fetch(404));
    }

    #[test]
    fn decode_failures_are_terminal_without_a_status() {
        for m in [
            "Could not decode image: cannot identify image file <_io.BytesIO object at 0x7f>",
            "PIL.UnidentifiedImageError: cannot identify image file",
            "Could not decode video: moov atom not found",
        ] {
            let f = classify_media_failure(m).expect(m);
            assert_eq!(f.kind, MediaFailureKind::Decode, "{m}");
            assert!(f.is_terminal());
        }
    }

    #[test]
    fn media_timeouts_are_classified_but_not_terminal() {
        let f = classify_media_failure(
            "Timed out while downloading media URL: https://cdn.example.com/a.png",
        )
        .unwrap();
        assert_eq!(f.kind, MediaFailureKind::Timeout);
        assert!(!f.is_terminal());
    }

    #[test]
    fn transient_4xx_from_the_media_host_is_not_classified() {
        assert_eq!(
            classify_media_failure("Failed to fetch media from URL: HTTP 429 error"),
            None
        );
        assert_eq!(
            classify_media_failure(
                "408 Client Error: Request Timeout for url: https://cdn.example.com/image.png"
            ),
            None
        );
    }

    #[test]
    fn inference_failures_are_left_alone() {
        for m in [
            "internal server error during processing",
            "No workers available for model",
            "This model's maximum context length is 16384 tokens. However, you requested 20000 tokens",
            "Failed to apply prompt template: System message must be at the beginning",
            "CUDA out of memory. Tried to allocate 2.00 GiB",
            "Error code: 404 - {'error': {'message': 'The model `gpt-image-1` does not exist', 'type': 'invalid_request_error'}}",
            "Backend unknown",
            "",
        ] {
            assert_eq!(classify_media_failure(m), None, "{m}");
        }
    }

    #[test]
    fn a_4xx_only_counts_when_it_stands_alone() {
        // `4040` and hashes containing 404 are not statuses.
        assert_eq!(standalone_4xx("fetch image from url gave 4040 bytes"), None);
        assert_eq!(standalone_4xx("hash 8a404c13 for url image"), None);
        assert_eq!(
            standalone_4xx("got http 404 not found for image"),
            Some(404)
        );
    }
}
