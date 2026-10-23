//! Bound inference bodies before middleware parses, copies, or logs them, and
//! decode compressed ones so every layer behind this one sees plain JSON.

use std::{
    io::Read,
    sync::{Arc, LazyLock},
};

use axum::{
    Json,
    body::{Body, Bytes, to_bytes},
    extract::State,
    http::{
        HeaderMap, HeaderValue, Method, Request, StatusCode,
        header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE},
    },
    middleware::Next,
    response::{IntoResponse, Response},
};

use tokio::sync::Semaphore;

use super::rejections;

/// The content codings an inference body may arrive in, as advertised on a 415.
const ACCEPTED_ENCODINGS: &str = "gzip, zstd, br";

/// RFC 9659: encoders of the `zstd` content coding must not need a window
/// above 8 MiB, so a frame declaring a larger one is refused rather than
/// allocated.
const ZSTD_WINDOW_LOG_MAX: u32 = 23;

/// Decodes running at once, one per CPU. Decoding runs before authentication,
/// and a few kilobytes of compressed body can expand to the full body limit
/// plus a decoder window of up to 16 MiB. Without a bound, a burst of small
/// anonymous requests could hold hundreds of those at once. Requests over the
/// bound wait for a slot rather than being refused, so legitimate traffic is
/// only ever delayed; uncompressed bodies never take one.
static DECODE_SLOTS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(std::thread::available_parallelism().map_or(1, |cpus| cpus.get()))));

/// Seconds a compressed body waited for a decode slot.
pub(crate) const DECODE_SLOT_WAIT_SECONDS: &str = "dwctl_inference_decode_slot_wait_seconds";

/// Compressed bodies waiting for a decode slot right now.
const DECODES_WAITING: &str = "dwctl_inference_decodes_waiting";

/// Bucket edges for [`DECODE_SLOT_WAIT_SECONDS`]. A free slot is taken at
/// once, so most waits fall in the first bucket; 0.25 is an edge so a
/// p99-above-250ms alert reads an exact bucket.
pub(crate) const DECODE_SLOT_WAIT_BUCKETS: &[f64] = &[0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0];

/// Describes the decode-slot metrics, with the waiting gauge at zero so it is
/// scraped before the first wait.
pub(crate) fn describe_decode_slot_metrics() {
    metrics::describe_histogram!(
        DECODE_SLOT_WAIT_SECONDS,
        metrics::Unit::Seconds,
        "Time a compressed inference body waited for a decode slot"
    );
    metrics::describe_gauge!(DECODES_WAITING, "Compressed inference bodies waiting for a decode slot");
    metrics::gauge!(DECODES_WAITING).set(0.0);
}

/// One decode in [`DECODES_WAITING`] while it lives.
struct Waiting(metrics::Gauge);

impl Waiting {
    fn new() -> Self {
        let gauge = metrics::gauge!(DECODES_WAITING);
        gauge.increment(1.0);
        Self(gauge)
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        self.0.decrement(1.0);
    }
}

/// Install outside all inference middleware, including request logging and
/// daemon dispatch bypasses. File uploads use a separate router and limit.
/// A zero limit retains the configured unlimited behavior.
///
/// A body sent with `Content-Encoding: gzip`, `zstd` or `br` is decoded here,
/// and the limit applies to the decoded size: that is what every inner layer
/// buffers. Any other coding is refused with a 415.
///
/// Before compression was supported the header was ignored, so a plain JSON
/// body under a wrong or meaningless `Content-Encoding` worked. Such a body
/// still does: it is passed on as it is and counted, rather than refused.
pub async fn limit_inference_body(State(max_bytes): State<usize>, request: Request<Body>, next: Next) -> Response {
    if request.method() == Method::POST {
        metrics::counter!("dwctl_inference_request_content_types_total", "content_type" => content_type_class(request.headers()))
            .increment(1);
    }
    let declared = content_encoding(request.headers());
    if max_bytes == 0 && declared == Ok(None) {
        return next.run(request).await;
    }

    let (mut parts, body) = request.into_parts();
    let bytes = match read_inference_body(&parts.headers, body, max_bytes).await {
        Ok(bytes) => bytes,
        Err(crate::errors::Error::PayloadTooLarge { message }) => {
            let response = oversized_body(message, parts.uri.path());
            return rejections::refuse(&parts, response, "request_too_large", None, None);
        }
        Err(_) => {
            let response = (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": {
                    "message": "Failed to read request body",
                    "type": "invalid_request_error",
                    "code": "body_read_failed"
                }})),
            )
                .into_response();
            return rejections::refuse(&parts, response, "body_read_failed", None, None);
        }
    };

    let body = match declared {
        Ok(None) => return next.run(Request::from_parts(parts, Body::from(bytes))).await,
        Ok(Some(encoding)) => {
            let limit = if max_bytes == 0 { usize::MAX } else { max_bytes };
            match decode(&DECODE_SLOTS, encoding, bytes.clone(), limit).await {
                Ok(decoded) => {
                    metrics::counter!("dwctl_inference_bodies_decoded_total", "encoding" => encoding.name()).increment(1);
                    decoded
                }
                Err(DecodeError::TooLarge) => {
                    let message = format!("Decoded request body exceeds the maximum size of {max_bytes} bytes");
                    let response = oversized_body(message, parts.uri.path());
                    return rejections::refuse(&parts, response, "request_too_large", None, None);
                }
                Err(DecodeError::Invalid) if is_json(&bytes) => mislabelled(encoding.name(), bytes),
                Err(DecodeError::Invalid) => {
                    let message = format!("Request body is not valid {} data", encoding.name());
                    let response = invalid_request(StatusCode::BAD_REQUEST, parts.uri.path(), "body_decode_failed", &message);
                    return rejections::refuse(&parts, response, "body_decode_failed", None, None);
                }
            }
        }
        Err(UnsupportedEncoding) if is_json(&bytes) => mislabelled("other", bytes),
        Err(UnsupportedEncoding) => {
            let response = unsupported_encoding(parts.uri.path());
            return rejections::refuse(&parts, response, "unsupported_content_encoding", None, None);
        }
    };
    // Inner layers and the upstream see the body as it now is: plain.
    parts.headers.remove(CONTENT_ENCODING);
    parts.headers.insert(CONTENT_LENGTH, HeaderValue::from(body.len()));
    next.run(Request::from_parts(parts, Body::from(body))).await
}

/// Whether `bytes` is a complete JSON document, as every inference body is.
fn is_json(bytes: &[u8]) -> bool {
    serde_json::from_slice::<serde::de::IgnoredAny>(bytes).is_ok()
}

/// A plain JSON body under a `Content-Encoding` it isn't in: passed on as it is.
fn mislabelled(declared: &'static str, bytes: Bytes) -> Bytes {
    metrics::counter!("dwctl_inference_bodies_mislabelled_total", "declared" => declared).increment(1);
    bytes
}

/// Shared with the authenticated playground proxy, whose model parser runs
/// before URI rewriting reaches the inference router.
pub(crate) async fn read_inference_body(headers: &HeaderMap, body: Body, max_bytes: usize) -> Result<Bytes, crate::errors::Error> {
    let too_large = || crate::errors::Error::PayloadTooLarge {
        message: format!("Request body exceeds the maximum size of {max_bytes} bytes"),
    };
    // Reject known oversized bodies without reading them. Also enforce the
    // actual byte count below: Content-Length may be absent (chunked requests).
    if max_bytes != 0
        && headers
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(too_large());
    }

    to_bytes(body, if max_bytes == 0 { usize::MAX } else { max_bytes })
        .await
        .map_err(|error| {
            if std::error::Error::source(&error).is_some_and(|source| source.is::<http_body_util::LengthLimitError>()) {
                too_large()
            } else {
                // Transport errors can contain user-controlled text. Do not
                // log or echo their details.
                crate::errors::Error::BadRequest {
                    message: "Failed to read request body".to_string(),
                }
            }
        })
}

pub(crate) fn oversized_body(message: String, path: &str) -> Response {
    let body = if path.ends_with("/messages") {
        serde_json::json!({"type": "error", "error": {
            "message": message, "type": "request_too_large"
        }})
    } else {
        serde_json::json!({"error": {
            "message": message, "type": "request_too_large", "code": "request_too_large"
        }})
    };
    (StatusCode::PAYLOAD_TOO_LARGE, Json(body)).into_response()
}

/// An `invalid_request_error` in the error shape of the API `path` belongs to.
fn invalid_request(status: StatusCode, path: &str, code: &str, message: &str) -> Response {
    let body = if path.ends_with("/messages") {
        serde_json::json!({"type": "error", "error": {
            "message": message, "type": "invalid_request_error"
        }})
    } else {
        serde_json::json!({"error": {
            "message": message, "type": "invalid_request_error", "code": code
        }})
    };
    (status, Json(body)).into_response()
}

/// RFC 9110 §15.5.16: a 415 for a content coding names the codings accepted.
fn unsupported_encoding(path: &str) -> Response {
    let message = format!("Unsupported Content-Encoding. Send the body uncompressed or encoded with one of: {ACCEPTED_ENCODINGS}");
    let mut response = invalid_request(StatusCode::UNSUPPORTED_MEDIA_TYPE, path, "unsupported_content_encoding", &message);
    response
        .headers_mut()
        .insert(ACCEPT_ENCODING, HeaderValue::from_static(ACCEPTED_ENCODINGS));
    response
}

/// The declared `Content-Type` of an inference body, as a bounded metric
/// label. The JSON endpoints parse the body whatever it declares (only strict
/// `/responses` checks), and clients such as `curl -d` declare a form body;
/// this counts how many do before anything starts refusing them.
fn content_type_class(headers: &HeaderMap) -> &'static str {
    let Some(value) = headers.get(CONTENT_TYPE) else {
        return "none";
    };
    let mime = value
        .to_str()
        .ok()
        .and_then(|value| value.split(';').next())
        .map(|mime| mime.trim().to_ascii_lowercase())
        .unwrap_or_default();
    match mime.as_str() {
        "application/json" => "json",
        mime if mime.ends_with("+json") => "json",
        "application/x-www-form-urlencoded" => "form",
        mime if mime.starts_with("multipart/") => "multipart",
        mime if mime.starts_with("text/") => "text",
        _ => "other",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Encoding {
    Gzip,
    Zstd,
    Brotli,
}

impl Encoding {
    fn name(self) -> &'static str {
        match self {
            Encoding::Gzip => "gzip",
            Encoding::Zstd => "zstd",
            Encoding::Brotli => "br",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct UnsupportedEncoding;

/// The body's content coding, or `None` when it has none. Layered codings
/// (`gzip, br`) are valid HTTP but no client sends them, so they are refused
/// like an unknown coding rather than decoded in turn.
fn content_encoding(headers: &HeaderMap) -> Result<Option<Encoding>, UnsupportedEncoding> {
    let mut found = None;
    for value in headers.get_all(CONTENT_ENCODING) {
        let value = value.to_str().map_err(|_| UnsupportedEncoding)?;
        for coding in value.split(',').map(str::trim).filter(|coding| !coding.is_empty()) {
            let encoding = match coding.to_ascii_lowercase().as_str() {
                "identity" => continue,
                "gzip" | "x-gzip" => Encoding::Gzip,
                "zstd" => Encoding::Zstd,
                "br" => Encoding::Brotli,
                _ => return Err(UnsupportedEncoding),
            };
            if found.replace(encoding).is_some() {
                return Err(UnsupportedEncoding);
            }
        }
    }
    Ok(found)
}

#[derive(Debug, PartialEq, Eq)]
enum DecodeError {
    TooLarge,
    Invalid,
}

/// Decodes `bytes` once one of `slots` is free, stopping as soon as the output
/// passes `max_bytes`, so a small body that inflates without bound costs at
/// most the limit. Runs on a blocking thread: inflating a full-size body takes
/// tens of milliseconds.
async fn decode(slots: &Arc<Semaphore>, encoding: Encoding, bytes: Bytes, max_bytes: usize) -> Result<Bytes, DecodeError> {
    in_slot(slots, move || decode_blocking(encoding, &bytes, max_bytes))
        .await
        .unwrap_or(Err(DecodeError::Invalid))
}

/// Runs `work` on a blocking thread once one of `slots` is free. The slot
/// moves into the thread with the work: a request dropped mid-decode (the
/// client went away) stops waiting for the result, but the work runs on and
/// must keep counting against the bound until it ends.
///
/// The time to get a slot is recorded in [`DECODE_SLOT_WAIT_SECONDS`], and
/// [`DECODES_WAITING`] counts the decodes still waiting for one: a request
/// dropped while waiting leaves the gauge but records no wait.
async fn in_slot<T: Send + 'static>(slots: &Arc<Semaphore>, work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let requested = tokio::time::Instant::now();
    let slot = {
        let _waiting = Waiting::new();
        slots.clone().acquire_owned().await.ok()?
    };
    metrics::histogram!(DECODE_SLOT_WAIT_SECONDS).record(requested.elapsed().as_secs_f64());
    tokio::task::spawn_blocking(move || {
        let _slot = slot;
        work()
    })
    .await
    .ok()
}

fn decode_blocking(encoding: Encoding, bytes: &[u8], max_bytes: usize) -> Result<Bytes, DecodeError> {
    let reader: Box<dyn Read + '_> = match encoding {
        // HTTP gzip bodies may hold several concatenated members.
        Encoding::Gzip => Box::new(flate2::read::MultiGzDecoder::new(bytes)),
        Encoding::Zstd => {
            // Most compressors record the decoded size in the frame header:
            // answer an oversized one as such rather than failing the window
            // check below with a less helpful error.
            if let Ok(Some(size)) = zstd::zstd_safe::get_frame_content_size(bytes)
                && size > max_bytes as u64
            {
                return Err(DecodeError::TooLarge);
            }
            let mut decoder = zstd::stream::read::Decoder::with_buffer(bytes).map_err(|_| DecodeError::Invalid)?;
            // The decoder allocates the window a frame declares up front, so an
            // unbounded window would let a tiny body claim gigabytes.
            decoder.window_log_max(ZSTD_WINDOW_LOG_MAX).map_err(|_| DecodeError::Invalid)?;
            Box::new(decoder)
        }
        Encoding::Brotli => return decode_brotli(bytes, max_bytes),
    };
    let mut decoded = Vec::new();
    reader
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut decoded)
        .map_err(|_| DecodeError::Invalid)?;
    if decoded.len() > max_bytes {
        return Err(DecodeError::TooLarge);
    }
    Ok(Bytes::from(decoded))
}

/// Brotli as RFC 7932 defines it, with windows up to 16 MiB. `brotli::Decompressor`
/// also accepts the large-window extension, whose windows reach 1 GiB and are
/// allocated before any output, so the output bound alone can't stop them.
/// The strict decoder state refuses that extension; it is only reachable
/// through the streaming API.
fn decode_brotli(bytes: &[u8], max_bytes: usize) -> Result<Bytes, DecodeError> {
    use brotli::{BrotliDecompressStream, BrotliResult, BrotliState, HeapAlloc, HuffmanCode};

    let mut state = BrotliState::new_strict(
        HeapAlloc::<u8>::new(0),
        HeapAlloc::<u32>::new(0),
        HeapAlloc::<HuffmanCode>::new(HuffmanCode::default()),
    );
    let mut buffer = vec![0u8; 64 * 1024];
    let mut decoded = Vec::new();
    let (mut available_in, mut input_offset, mut total_out) = (bytes.len(), 0, 0);
    loop {
        // Never decode more than one byte past the limit, as the other
        // codecs' `take(max_bytes + 1)` does.
        let room = (max_bytes - decoded.len()).saturating_add(1).min(buffer.len());
        let (mut available_out, mut output_offset) = (room, 0);
        let result = BrotliDecompressStream(
            &mut available_in,
            &mut input_offset,
            bytes,
            &mut available_out,
            &mut output_offset,
            &mut buffer[..room],
            &mut total_out,
            &mut state,
        );
        decoded.extend_from_slice(&buffer[..output_offset]);
        if decoded.len() > max_bytes {
            return Err(DecodeError::TooLarge);
        }
        match result {
            BrotliResult::NeedsMoreOutput => continue,
            // Bytes after the end of the stream aren't brotli data either.
            BrotliResult::ResultSuccess if available_in == 0 => return Ok(Bytes::from(decoded)),
            _ => return Err(DecodeError::Invalid),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Bytes, http::StatusCode, middleware, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tower::ServiceExt;

    fn app(limit: usize) -> Router {
        Router::new()
            .route("/chat/completions", post(|body: Bytes| async move { body }))
            .layer(middleware::from_fn_with_state(limit, limit_inference_body))
            .layer(rejections::test_support::layer("m"))
    }

    #[test]
    fn counts_refused_bodies() {
        let (lines, _) = rejections::test_support::rejections(async {
            app(4)
                .oneshot(Request::post("/chat/completions").body(Body::from("12345")).unwrap())
                .await
                .unwrap();
            let failing = Body::from_stream(futures::stream::once(async { Err::<Bytes, _>(std::io::Error::other("reset")) }));
            app(4)
                .oneshot(Request::post("/chat/completions").body(failing).unwrap())
                .await
                .unwrap();
            app(4)
                .oneshot(Request::post("/chat/completions").body(Body::from("1234")).unwrap())
                .await
                .unwrap();
        });
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines
                .iter()
                .any(|l| l.contains("status=\"413\"") && l.contains("code=\"request_too_large\""))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("status=\"400\"") && l.contains("code=\"body_read_failed\""))
        );
    }

    #[tokio::test]
    async fn rejects_oversized_body_before_the_handler() {
        let response = app(4)
            .oneshot(Request::post("/chat/completions").body(Body::from("12345")).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn rejects_content_length_without_polling_the_body() {
        let polls = Arc::new(AtomicUsize::new(0));
        let observed = polls.clone();
        let body = Body::from_stream(futures::stream::poll_fn(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
            std::task::Poll::Ready(None::<Result<Bytes, std::io::Error>>)
        }));
        let response = app(4)
            .oneshot(
                Request::post("/chat/completions")
                    .header("content-length", "10000000")
                    .body(body)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(polls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn stops_streaming_at_the_limit_even_without_an_honest_content_length() {
        for length in [None, Some("1")] {
            let polls = Arc::new(AtomicUsize::new(0));
            let observed = polls.clone();
            let body = Body::from_stream(futures::stream::poll_fn(move |_| {
                let n = observed.fetch_add(1, Ordering::SeqCst);
                assert!(n < 2, "must stop reading as soon as the limit is exceeded");
                std::task::Poll::Ready(Some(Ok::<_, std::io::Error>(Bytes::from_static(b"abc"))))
            }));
            let mut request = Request::post("/chat/completions");
            if let Some(length) = length {
                request = request.header("content-length", length);
            }
            let response = app(4).oneshot(request.body(body).unwrap()).await.unwrap();
            assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
            assert_eq!(polls.load(Ordering::SeqCst), 2);
        }
    }

    #[tokio::test]
    async fn preserves_bodies_at_the_limit_and_when_limits_are_disabled() {
        for (limit, input) in [(4, "1234"), (0, "12345")] {
            let response = app(limit)
                .oneshot(Request::post("/chat/completions").body(Body::from(input)).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(axum::body::to_bytes(response.into_body(), 100).await.unwrap(), input);
        }
    }

    fn compress(encoding: Encoding, data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        match encoding {
            Encoding::Gzip => {
                let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                encoder.write_all(data).unwrap();
                encoder.finish().unwrap()
            }
            Encoding::Zstd => zstd::encode_all(data, 3).unwrap(),
            Encoding::Brotli => {
                let mut out = Vec::new();
                {
                    let mut writer = brotli::CompressorWriter::new(&mut out, 4096, 5, 22);
                    writer.write_all(data).unwrap();
                }
                out
            }
        }
    }

    /// Echoes the body, failing unless the headers describe it as sent onwards.
    fn decoding_app(limit: usize) -> Router {
        Router::new()
            .route(
                "/chat/completions",
                post(|headers: HeaderMap, body: Bytes| async move {
                    assert!(headers.get(CONTENT_ENCODING).is_none());
                    assert_eq!(headers[CONTENT_LENGTH], body.len().to_string());
                    body
                }),
            )
            .layer(middleware::from_fn_with_state(limit, limit_inference_body))
            .layer(rejections::test_support::layer("m"))
    }

    async fn error_of(response: Response) -> serde_json::Value {
        serde_json::from_slice(&axum::body::to_bytes(response.into_body(), 10_000).await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn decodes_each_accepted_encoding_for_the_layers_behind_it() {
        // Compressible, so every encoding is smaller than the limit it decodes to.
        let json = format!(
            r#"{{"model":"m","messages":[{{"role":"user","content":"{}"}}]}}"#,
            "hello ".repeat(50)
        );
        let json = json.as_bytes();
        for (encoding, header) in [
            (Encoding::Gzip, "gzip"),
            (Encoding::Gzip, "x-gzip"),
            (Encoding::Zstd, "zstd"),
            (Encoding::Brotli, "br"),
            (Encoding::Zstd, "ZSTD"),
        ] {
            for limit in [json.len(), 0] {
                let compressed = compress(encoding, json);
                let response = decoding_app(limit)
                    .oneshot(
                        Request::post("/chat/completions")
                            .header(CONTENT_ENCODING, header)
                            .header(CONTENT_LENGTH, compressed.len())
                            .body(Body::from(compressed))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK, "{header} with limit {limit}");
                assert_eq!(axum::body::to_bytes(response.into_body(), 10_000).await.unwrap(), &json[..]);
            }
        }
    }

    #[tokio::test]
    async fn refuses_other_encodings_with_415() {
        for header in ["deflate", "lz4", "compress", "gzip, br", "snappy"] {
            let response = decoding_app(1024)
                .oneshot(
                    Request::post("/chat/completions")
                        .header(CONTENT_ENCODING, header)
                        .body(Body::from(&b"\x04\x22\x4d\x18 compressed bytes"[..]))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE, "{header}");
            assert_eq!(response.headers()[ACCEPT_ENCODING], "gzip, zstd, br");
            assert_eq!(error_of(response).await["error"]["code"], "unsupported_content_encoding");
        }
    }

    #[tokio::test]
    async fn passes_plain_json_under_a_wrong_content_encoding_through() {
        // These worked before compression was supported, when the header was
        // ignored; they must keep working, as plain bodies.
        let json = br#"{"model":"m","messages":[]}"#;
        for header in ["utf-8", "gzip", "zstd", "br", "lz4", "gzip, br"] {
            let response = decoding_app(1024)
                .oneshot(
                    Request::post("/chat/completions")
                        .header(CONTENT_ENCODING, header)
                        .body(Body::from(&json[..]))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{header}");
            assert_eq!(
                axum::body::to_bytes(response.into_body(), 1024).await.unwrap(),
                &json[..],
                "{header}"
            );
        }
    }

    #[tokio::test]
    async fn decodes_wait_for_a_free_slot() {
        let slots = Arc::new(Semaphore::new(1));
        let compressed = Bytes::from(compress(Encoding::Gzip, b"{}"));
        let busy = slots.acquire().await.unwrap();
        let mut waiting = Box::pin(decode(&slots, Encoding::Gzip, compressed, 1024));
        assert!(
            futures::poll!(&mut waiting).is_pending(),
            "a decode must wait while every slot is taken"
        );
        drop(busy);
        assert_eq!(waiting.await.unwrap(), &b"{}"[..]);
        assert_eq!(slots.available_permits(), 1, "the slot is returned after decoding");
    }

    /// Runs `test` on a current-thread runtime with its clock paused, under a
    /// fresh recorder it can render, and returns what that rendered at the end.
    fn metered<F: std::future::Future<Output = ()>>(test: impl FnOnce(metrics_exporter_prometheus::PrometheusHandle) -> F) -> String {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new()
            .set_buckets_for_metric(
                metrics_exporter_prometheus::Matcher::Full(DECODE_SLOT_WAIT_SECONDS.to_string()),
                DECODE_SLOT_WAIT_BUCKETS,
            )
            .unwrap()
            .build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .unwrap()
                .block_on(test(handle.clone()))
        });
        handle.render()
    }

    fn has_line(rendered: &str, line: &str) -> bool {
        rendered.lines().any(|l| l == line)
    }

    #[test]
    fn records_how_long_a_decode_waited_for_its_slot() {
        let rendered = metered(|handle| async move {
            let slots = Arc::new(Semaphore::new(1));
            in_slot(&slots, || ()).await.unwrap();

            let busy = slots.acquire().await.unwrap();
            let mut waiting = Box::pin(in_slot(&slots, || ()));
            assert!(futures::poll!(&mut waiting).is_pending(), "every slot is taken");
            let during = handle.render();
            assert!(has_line(&during, "dwctl_inference_decodes_waiting 1"), "{during}");
            tokio::time::advance(std::time::Duration::from_millis(300)).await;
            drop(busy);
            waiting.await.unwrap();
        });
        let wait = |le: &str, count: u32| format!(r#"dwctl_inference_decode_slot_wait_seconds_bucket{{le="{le}"}} {count}"#);
        assert!(has_line(&rendered, &wait("0.001", 1)), "a free slot is taken at once: {rendered}");
        assert!(has_line(&rendered, &wait("0.25", 1)), "{rendered}");
        assert!(has_line(&rendered, &wait("0.5", 2)), "the held slot's wait is recorded: {rendered}");
        assert!(has_line(&rendered, "dwctl_inference_decodes_waiting 0"), "{rendered}");
    }

    #[test]
    fn a_request_dropped_while_waiting_leaves_the_queue() {
        let rendered = metered(|_| async {
            let slots = Arc::new(Semaphore::new(1));
            let _busy = slots.acquire().await.unwrap();
            let mut waiting = Box::pin(in_slot(&slots, || ()));
            assert!(futures::poll!(&mut waiting).is_pending());
            drop(waiting);
        });
        assert!(has_line(&rendered, "dwctl_inference_decodes_waiting 0"), "{rendered}");
        assert!(
            !rendered.contains("dwctl_inference_decode_slot_wait_seconds_count"),
            "an abandoned wait records no duration: {rendered}"
        );
    }

    #[tokio::test]
    async fn a_dropped_request_keeps_its_slot_until_the_decode_ends() {
        let slots = Arc::new(Semaphore::new(1));
        let (finish, finished) = std::sync::mpsc::channel::<()>();
        let mut running = Box::pin(in_slot(&slots, move || finished.recv().unwrap()));
        assert!(futures::poll!(&mut running).is_pending(), "the work is running");
        drop(running);
        assert_eq!(slots.available_permits(), 0, "the abandoned work still holds its slot");
        finish.send(()).unwrap();
        // Returned once the work ends: this waits for exactly that.
        drop(slots.acquire().await.unwrap());
    }

    #[tokio::test]
    async fn answers_messages_requests_in_the_anthropic_error_shape() {
        let app = Router::new()
            .route("/messages", post(|body: Bytes| async move { body }))
            .layer(middleware::from_fn_with_state(1024usize, limit_inference_body));
        let response = app
            .oneshot(
                Request::post("/messages")
                    .header(CONTENT_ENCODING, "lz4")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let body = error_of(response).await;
        assert_eq!(body["type"], "error");
        assert_eq!(body["error"]["type"], "invalid_request_error");
    }

    #[tokio::test]
    async fn applies_the_limit_to_the_decoded_size() {
        // Megabytes of zeros compress to a few kilobytes: a small upload must
        // not be able to make inner layers buffer past the limit.
        for encoding in [Encoding::Gzip, Encoding::Zstd, Encoding::Brotli] {
            let bomb = compress(encoding, &vec![0u8; 4 << 20]);
            assert!(bomb.len() < 64 << 10);
            let response = decoding_app(1 << 20)
                .oneshot(
                    Request::post("/chat/completions")
                        .header(CONTENT_ENCODING, encoding.name())
                        .body(Body::from(bomb))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE, "{encoding:?}");
            assert_eq!(error_of(response).await["error"]["code"], "request_too_large");
        }
    }

    #[test]
    fn refuses_zstd_windows_above_the_http_limit() {
        use std::io::Write;
        // A streaming compressor that doesn't know the input size declares its
        // full window, however small the body: the decoder must not allocate
        // more than RFC 9659's 8 MiB for it.
        let framed = |window_log| {
            let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
            encoder.window_log(window_log).unwrap();
            encoder.write_all(b"{}").unwrap();
            encoder.finish().unwrap()
        };
        assert_eq!(decode_blocking(Encoding::Zstd, &framed(23), 1024).unwrap(), &b"{}"[..]);
        assert_eq!(decode_blocking(Encoding::Zstd, &framed(24), 1024), Err(DecodeError::Invalid));
    }

    #[test]
    fn refuses_brotli_large_window_streams() {
        use brotli::enc::BrotliEncoderParams;
        let params = BrotliEncoderParams {
            large_window: true,
            lgwin: 30,
            ..Default::default()
        };
        let mut large = Vec::new();
        brotli::BrotliCompress(&mut &b"{}"[..], &mut large, &params).unwrap();
        // A valid large-window stream, as the permissive decoder shows...
        let mut permissive = Vec::new();
        brotli::Decompressor::new(&large[..], 4096).read_to_end(&mut permissive).unwrap();
        assert_eq!(permissive, b"{}");
        // ...which the strict one refuses before allocating its window.
        assert_eq!(decode_blocking(Encoding::Brotli, &large, 1024), Err(DecodeError::Invalid));
        let standard = compress(Encoding::Brotli, b"{}");
        assert_eq!(decode_blocking(Encoding::Brotli, &standard, 1024).unwrap(), &b"{}"[..]);
    }

    #[test]
    fn decodes_up_to_the_limit_and_not_a_byte_further() {
        let body = vec![b'x'; 200_000];
        for encoding in [Encoding::Gzip, Encoding::Zstd, Encoding::Brotli] {
            let compressed = compress(encoding, &body);
            assert_eq!(decode_blocking(encoding, &compressed, body.len()).unwrap().len(), body.len());
            assert_eq!(
                decode_blocking(encoding, &compressed, body.len() - 1),
                Err(DecodeError::TooLarge),
                "{encoding:?}"
            );
        }
    }

    #[test]
    fn decodes_every_concatenated_frame() {
        for encoding in [Encoding::Gzip, Encoding::Zstd] {
            let mut frames = compress(encoding, b"{\"a\":");
            frames.extend(compress(encoding, b"1}"));
            assert_eq!(decode_blocking(encoding, &frames, 1024).unwrap(), &b"{\"a\":1}"[..], "{encoding:?}");
        }
        let mut trailing = compress(Encoding::Brotli, b"{}");
        trailing.extend_from_slice(b"junk");
        assert_eq!(decode_blocking(Encoding::Brotli, &trailing, 1024), Err(DecodeError::Invalid));
    }

    #[tokio::test]
    async fn rejects_bodies_that_are_not_valid_for_their_encoding() {
        for encoding in [Encoding::Gzip, Encoding::Zstd, Encoding::Brotli] {
            let response = decoding_app(1024)
                .oneshot(
                    Request::post("/chat/completions")
                        .header(CONTENT_ENCODING, encoding.name())
                        .body(Body::from("neither compressed nor JSON"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{encoding:?}");
            assert_eq!(error_of(response).await["error"]["code"], "body_decode_failed");
        }
    }

    #[test]
    fn counts_refused_encodings() {
        let (lines, _) = rejections::test_support::rejections(async {
            decoding_app(1024)
                .oneshot(
                    Request::post("/chat/completions")
                        .header(CONTENT_ENCODING, "lz4")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            decoding_app(1024)
                .oneshot(
                    Request::post("/chat/completions")
                        .header(CONTENT_ENCODING, "gzip")
                        .body(Body::from("not gzip"))
                        .unwrap(),
                )
                .await
                .unwrap();
        });
        assert!(
            lines
                .iter()
                .any(|l| l.contains("status=\"415\"") && l.contains("code=\"unsupported_content_encoding\"")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("status=\"400\"") && l.contains("code=\"body_decode_failed\"")),
            "{lines:?}"
        );
    }

    #[test]
    fn reads_the_content_encoding_header() {
        let headers = |values: &[&str]| {
            let mut headers = HeaderMap::new();
            for value in values {
                headers.append(CONTENT_ENCODING, value.parse().unwrap());
            }
            headers
        };
        assert_eq!(content_encoding(&headers(&[])), Ok(None));
        assert_eq!(content_encoding(&headers(&["identity"])), Ok(None));
        assert_eq!(content_encoding(&headers(&[" GZip "])), Ok(Some(Encoding::Gzip)));
        assert_eq!(content_encoding(&headers(&["identity, br"])), Ok(Some(Encoding::Brotli)));
        assert_eq!(content_encoding(&headers(&["gzip", "gzip"])), Err(UnsupportedEncoding));
        assert_eq!(content_encoding(&headers(&["deflate"])), Err(UnsupportedEncoding));
    }

    #[test]
    fn classifies_declared_content_types() {
        let class = |value: Option<&str>| {
            let mut headers = HeaderMap::new();
            if let Some(value) = value {
                headers.insert(CONTENT_TYPE, value.parse().unwrap());
            }
            content_type_class(&headers)
        };
        assert_eq!(class(None), "none");
        assert_eq!(class(Some("application/json")), "json");
        assert_eq!(class(Some("Application/JSON; charset=utf-8")), "json");
        assert_eq!(class(Some("application/vnd.api+json")), "json");
        assert_eq!(class(Some("application/x-www-form-urlencoded")), "form");
        assert_eq!(class(Some("multipart/form-data; boundary=x")), "multipart");
        assert_eq!(class(Some("text/plain")), "text");
        assert_eq!(class(Some("application/octet-stream")), "other");
    }

    #[tokio::test]
    async fn distinguishes_transport_errors_without_exposing_their_contents() {
        let body = Body::from_stream(futures::stream::once(async {
            Err::<Bytes, _>(std::io::Error::other("sensitive transport detail"))
        }));
        let response = app(4)
            .oneshot(Request::post("/chat/completions").body(body).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), 1000).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "body_read_failed");
        assert!(!body.to_string().contains("sensitive transport detail"));
    }
}
