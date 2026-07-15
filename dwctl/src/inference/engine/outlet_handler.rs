//! Outlet `RequestHandler` that sends response completion records into the
//! in-process `RequestsWriter` channel.
//!
//! This handler is added to outlet's `MultiHandler` alongside the existing
//! `PostgresHandler` (which writes to `http_analytics`). After outlet captures
//! the response body, this handler builds a `RawCompletedRequest` and pushes
//! it onto the writer's mpsc channel; the batched writer task then persists
//! the row in fusillade.
//!
//! Channel-full backpressure flows back to the outlet handler via
//! `Sender::send().await`, matching `AnalyticsHandler`'s shape. We prefer
//! slowing outlet to dropping rows, since the `requests` table is the only
//! place the responses listing reads from.

use chrono::{DateTime, Utc};
use outlet::{RequestData, RequestHandler, ResponseData};
use sqlx::PgPool;
use uuid::Uuid;

use super::writer::{RawCompletedRequest, RequestsWriterSender};
use crate::inference::store::{ONWARDS_RESPONSE_ID_HEADER, lookup_created_by};

/// Outlet handler that forwards completion records to the in-process writer.
///
/// Resolves `created_by` from the api_key here (one indexed lookup against
/// dwctl_pool) before sending so the writer can flush without touching the
/// dwctl pool inside its bulk transaction. Records without a resolvable
/// attribution are dropped (matches the underway era, where the
/// create-response job returned early on missing/invalid api_key).
#[derive(Clone)]
pub struct FusilladeOutletHandler {
    sender: RequestsWriterSender,
    dwctl_pool: PgPool,
}

impl FusilladeOutletHandler {
    pub fn new(sender: RequestsWriterSender, dwctl_pool: PgPool) -> Self {
        Self { sender, dwctl_pool }
    }

    /// Extract the onwards response ID from request headers, if present.
    fn extract_response_id(request: &RequestData) -> Option<String> {
        Self::header_str(request, ONWARDS_RESPONSE_ID_HEADER).map(String::from)
    }

    /// Extract the raw fusillade request UUID from `x-fusillade-request-id`.
    fn extract_request_id(request: &RequestData) -> Option<Uuid> {
        Self::header_str(request, "x-fusillade-request-id").and_then(|s| Uuid::parse_str(s).ok())
    }

    /// Extract the bearer token from the Authorization header.
    fn extract_api_key(request: &RequestData) -> Option<String> {
        Self::header_str(request, "authorization")
            .and_then(|s| s.strip_prefix("Bearer "))
            .map(String::from)
    }

    fn header_str<'a>(request: &'a RequestData, name: &str) -> Option<&'a str> {
        request
            .headers
            .get(name)
            .and_then(|values| values.first())
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
    }

    /// Extract the identifiers + attribution context every `CompleteResponseJob`
    /// needs. Returns `None` (after logging) when a required header is absent
    /// or unparseable, so both [`Self::handle_response`] and
    /// [`Self::handle_abandoned`] short-circuit consistently. Centralising
    /// this here keeps the two handler paths from drifting on which headers
    /// are gating vs. just warned-about.
    fn extract_complete_response_ctx(request: &RequestData) -> Option<CompleteResponseCtx> {
        // Only the inference middleware sets `x-onwards-response-id`, so its
        // absence means this is either a daemon-driven fusillade batch request
        // (daemon handles its own completion) or unrelated traffic. Silent
        // no-op — no warning, no work to do.
        let response_id = Self::extract_response_id(request)?;

        let request_id = match Self::extract_request_id(request) {
            Some(id) => id,
            None => {
                tracing::warn!(response_id = %response_id, "Missing x-fusillade-request-id header — skipping enqueue");
                return None;
            }
        };

        let model = Self::header_str(request, "x-onwards-model").unwrap_or("unknown").to_string();
        let endpoint = Self::header_str(request, "x-onwards-endpoint").unwrap_or("").to_string();
        let api_key = Self::extract_api_key(request);

        if endpoint.is_empty() {
            // Empty endpoint isn't fatal here — `complete_response_idempotent`
            // will refuse to synthesize a row without it. We continue (rather
            // than returning None) so the UPDATE path still succeeds if the
            // row already exists.
            tracing::warn!(
                response_id = %response_id,
                uri = %request.uri,
                "Missing x-onwards-endpoint header — complete-response synthesize will fail if create-response hasn't run"
            );
        }

        Some(CompleteResponseCtx {
            response_id,
            request_id,
            model,
            endpoint,
            api_key,
        })
    }
}

/// Per-request identifiers + attribution extracted from the captured headers,
/// shared by [`FusilladeOutletHandler::handle_response`] and
/// [`FusilladeOutletHandler::handle_abandoned`].
#[derive(Debug, PartialEq, Eq)]
struct CompleteResponseCtx {
    response_id: String,
    request_id: Uuid,
    model: String,
    endpoint: String,
    api_key: Option<String>,
}

impl FusilladeOutletHandler {
    /// Resolve `created_by` from the api_key, drop records that can't be
    /// attributed. The fusillade row's XOR check requires non-empty
    /// `created_by` for batchless rows; the underway-era code skipped the
    /// same case at the job level, so this is just where the skip moved to.
    async fn resolve_attribution(&self, ctx: &CompleteResponseCtx) -> Option<(String, String)> {
        let api_key = match ctx.api_key.as_deref() {
            Some(key) if !key.is_empty() => key.to_string(),
            _ => {
                tracing::debug!(
                    response_id = %ctx.response_id,
                    "Skipping response writer send - no api_key on request"
                );
                metrics::counter!("dwctl_requests_writer_dropped_total", "reason" => "missing_api_key").increment(1);
                return None;
            }
        };
        let created_by = lookup_created_by(&self.dwctl_pool, Some(&api_key)).await;
        match created_by {
            Some(uid) if !uid.is_empty() => Some((api_key, uid)),
            _ => {
                tracing::debug!(
                    response_id = %ctx.response_id,
                    "Skipping response writer send - api_key did not resolve to a user"
                );
                metrics::counter!("dwctl_requests_writer_dropped_total", "reason" => "unknown_api_key").increment(1);
                None
            }
        }
    }
}

impl RequestHandler for FusilladeOutletHandler {
    async fn handle_request(&self, _data: RequestData) {}

    fn handle_response(&self, request_data: RequestData, response_data: ResponseData) -> impl std::future::Future<Output = ()> + Send {
        let sender = self.sender.clone();
        let handler = self.clone();

        async move {
            let Some(ctx) = Self::extract_complete_response_ctx(&request_data) else {
                return;
            };
            let Some((api_key, created_by)) = handler.resolve_attribution(&ctx).await else {
                return;
            };

            let status_code = response_data.status.as_u16();
            let response_body = response_data
                .body
                .as_ref()
                .and_then(|b| std::str::from_utf8(b).ok())
                .unwrap_or("")
                .to_string();
            let request_body = request_data
                .body
                .as_ref()
                .and_then(|b| std::str::from_utf8(b).ok())
                .unwrap_or("")
                .to_string();

            // Realtime ZDR (marker set by the inference middleware): non-persistence.
            // Suppress both bodies before the RequestsWriter stores them, so plaintext
            // never lands in `requests.response_body` / `request_templates.body` —
            // mirroring how `ZdrBodyScrubber` guards the analytics inserts. This handler
            // is not wrapped by the scrubber (that would drop its `handle_abandoned`
            // override), so the guard lives inline here.
            let (response_body, request_body) = if request_is_zdr(&request_data) {
                (String::new(), String::new())
            } else {
                (response_body, request_body)
            };

            // Real request timing measured by outlet: arrival (request
            // timestamp) → arrival + total duration. Carried through so the
            // synthesized fusillade row records the true latency instead of
            // started_at == completed_at == NOW() (which read as duration 0).
            let started_at: DateTime<Utc> = request_data.timestamp.into();
            let completed_at = started_at + response_duration(&response_data);

            if let Err(e) = sender
                .send(RawCompletedRequest {
                    request_id: ctx.request_id,
                    status_code,
                    response_body,
                    request_body,
                    model: ctx.model,
                    endpoint: ctx.endpoint,
                    api_key,
                    created_by,
                    started_at,
                    completed_at,
                })
                .await
            {
                metrics::counter!("dwctl_requests_writer_sends_total", "result" => "err").increment(1);
                tracing::warn!(
                    error = %e,
                    response_id = %ctx.response_id,
                    "Failed to send completed-response record to writer (channel closed)"
                );
            } else {
                metrics::counter!("dwctl_requests_writer_sends_total", "result" => "ok").increment(1);
            }
        }
    }

    fn handle_abandoned(&self, request_data: RequestData) -> impl std::future::Future<Output = ()> + Send {
        let sender = self.sender.clone();
        let handler = self.clone();

        async move {
            let Some(ctx) = Self::extract_complete_response_ctx(&request_data) else {
                return;
            };
            let Some((api_key, created_by)) = handler.resolve_attribution(&ctx).await else {
                return;
            };

            // 499 Client Closed Request - nginx-popularized status for this
            // scenario. The structured body distinguishes the row from a
            // real upstream 5xx in the responses listing; status 499 also
            // maps to `client_disconnected` in `status_to_error_type` so
            // the body and the derived UI error type agree.
            //
            // We omit `request_body` (set to "") because outlet's
            // AbandonGuard builds the abandon-time RequestData before body
            // capture completes - `request_data.body` is always None on
            // this path. That's fine for create-if-missing: an empty body
            // is correct since no upstream call ever happened.
            const STATUS_CLIENT_CLOSED: u16 = 499;
            let abandoned_body = serde_json::json!({
                "error": {
                    "type": "client_disconnected",
                    "message": "client cancelled request before upstream response",
                    "code": STATUS_CLIENT_CLOSED,
                }
            })
            .to_string();

            // Client cancelled before any upstream response, so there is no
            // measured duration. Record arrival for both endpoints (duration 0
            // is truthful here — no round-trip completed).
            let started_at: DateTime<Utc> = request_data.timestamp.into();

            if let Err(e) = sender
                .send(RawCompletedRequest {
                    request_id: ctx.request_id,
                    status_code: STATUS_CLIENT_CLOSED,
                    response_body: abandoned_body,
                    request_body: String::new(),
                    model: ctx.model,
                    endpoint: ctx.endpoint,
                    api_key,
                    created_by,
                    started_at,
                    completed_at: started_at,
                })
                .await
            {
                metrics::counter!("dwctl_requests_writer_sends_total", "result" => "err").increment(1);
                tracing::warn!(
                    error = %e,
                    response_id = %ctx.response_id,
                    "Failed to send abandoned-response record to writer (channel closed)"
                );
            } else {
                metrics::counter!("dwctl_requests_writer_sends_total", "result" => "ok").increment(1);
            }
        }
    }
}

/// TRANSITIONAL (dwctl ZDR): true when a captured request carried the ZDR marker
/// header. dwctl's dispatch processor tags the request's `batch_metadata` at
/// decrypt time and fusillade forwards it as
/// [`ZDR_MARKER_HEADER`](crate::inference::zdr::ZDR_MARKER_HEADER). By the time
/// outlet captures the loopback dispatch the body is already-decrypted plaintext,
/// so the header is the only signal that it must not be logged. Remove once
/// response reassembly moves into dwctl.
/// Outlet's total request duration as a `chrono::Duration`. Falls back to zero
/// if the (always-positive) `std::time::Duration` somehow overflows the chrono
/// range, so a completion timestamp is always produced.
fn response_duration(response: &ResponseData) -> chrono::Duration {
    chrono::Duration::from_std(response.duration).unwrap_or_else(|_| chrono::Duration::zero())
}

fn request_is_zdr(request: &RequestData) -> bool {
    request
        .headers
        .get(crate::inference::zdr::ZDR_MARKER_HEADER)
        .and_then(|values| values.first())
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        == Some("1")
}

/// dwctl ZDR: a `RequestHandler` that blanks the request and response bodies of
/// ZDR requests before the handler it wraps persists them, so plaintext ZDR
/// bodies never reach the analytics DB (`http_requests` / `http_responses`).
///
/// Where it runs: it is one link in the handler chain that the `outlet` crate's
/// `RequestLoggerLayer` drives as traffic passes through the `/ai` proxy (it is
/// dwctl code, not part of `outlet` itself). It wraps
/// `outlet_postgres::PostgresHandler`, the component that does the actual
/// inserts. That handler is a generic logger that knows nothing about ZDR, so
/// rather than teach it we blank the body in the wrapper and then delegate - it
/// never sees the plaintext.
///
/// This is NOT the fusillade-side encryptor. `ZdrResponseEncryptor` (in
/// `crate::inference::zdr`) encrypts the body fusillade writes into its OWN db
/// (`requests.response_body`), and exists because fusillade reassembles and
/// persists the response itself; this scrubber only guards outlet's separate
/// analytics tables. They share nothing but the goal of keeping a ZDR body out
/// of a db.
///
/// Why a plaintext ZDR body reaches outlet at all:
///   - realtime: the body is plaintext throughout (realtime ZDR is
///     non-persistence, not encryption). The inference middleware stamps the
///     [`request_is_zdr`] marker on the request.
///   - flex: the daemon must decrypt the stored ciphertext to call the provider,
///     and that decrypted dispatch is re-sent through the `/ai` loopback, which
///     outlet captures. Fusillade forwards the marker from `batch_metadata`.
///
/// The marker is present on both the request and response callbacks, so both of
/// PostgresHandler's inserts (`http_requests`, `http_responses`) are covered.
#[derive(Clone)]
pub struct ZdrBodyScrubber<H> {
    inner: H,
}

impl<H> ZdrBodyScrubber<H> {
    pub fn new(inner: H) -> Self {
        Self { inner }
    }
}

impl<H: RequestHandler> RequestHandler for ZdrBodyScrubber<H> {
    fn handle_request(&self, mut data: RequestData) -> impl std::future::Future<Output = ()> + Send {
        if request_is_zdr(&data) {
            data.body = None;
        }
        self.inner.handle_request(data)
    }

    fn handle_response(
        &self,
        mut request_data: RequestData,
        mut response_data: ResponseData,
    ) -> impl std::future::Future<Output = ()> + Send {
        if request_is_zdr(&request_data) {
            request_data.body = None;
            response_data.body = None;
        }
        self.inner.handle_response(request_data, response_data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::collections::HashMap;
    use std::time::SystemTime;

    fn make_request_data(headers: HashMap<String, Vec<Bytes>>) -> RequestData {
        RequestData {
            correlation_id: 1,
            timestamp: SystemTime::now(),
            method: axum::http::Method::POST,
            uri: "/v1/responses".parse().unwrap(),
            headers,
            body: None,
            trace_id: None,
            span_id: None,
        }
    }

    #[test]
    fn test_extract_response_id_present() {
        let mut headers = HashMap::new();
        headers.insert(
            ONWARDS_RESPONSE_ID_HEADER.to_string(),
            vec![Bytes::from("resp_12345678-1234-1234-1234-123456789abc")],
        );
        let request = make_request_data(headers);
        let id = FusilladeOutletHandler::extract_response_id(&request);
        assert_eq!(id, Some("resp_12345678-1234-1234-1234-123456789abc".to_string()));
    }

    #[test]
    fn test_extract_response_id_absent() {
        let request = make_request_data(HashMap::new());
        let id = FusilladeOutletHandler::extract_response_id(&request);
        assert!(id.is_none());
    }

    #[test]
    fn test_extract_response_id_present_with_fusillade_request_header() {
        // Inference middleware sets `x-fusillade-request-id` for the ID
        // override and `x-onwards-response-id` for outlet routing. Both
        // headers together must produce a valid response id.
        let mut headers = HashMap::new();
        headers.insert("x-fusillade-request-id".to_string(), vec![Bytes::from("some-id")]);
        headers.insert(
            ONWARDS_RESPONSE_ID_HEADER.to_string(),
            vec![Bytes::from("resp_12345678-1234-1234-1234-123456789abc")],
        );
        let request = make_request_data(headers);
        let id = FusilladeOutletHandler::extract_response_id(&request);
        assert_eq!(id, Some("resp_12345678-1234-1234-1234-123456789abc".to_string()));
    }

    #[test]
    fn test_extract_request_id_present() {
        let mut headers = HashMap::new();
        let uuid_str = "12345678-1234-1234-1234-123456789abc";
        headers.insert("x-fusillade-request-id".to_string(), vec![Bytes::from(uuid_str)]);
        let request = make_request_data(headers);
        let id = FusilladeOutletHandler::extract_request_id(&request);
        assert_eq!(id, Some(Uuid::parse_str(uuid_str).unwrap()));
    }

    #[test]
    fn test_extract_request_id_absent() {
        let request = make_request_data(HashMap::new());
        assert!(FusilladeOutletHandler::extract_request_id(&request).is_none());
    }

    #[test]
    fn test_extract_request_id_invalid_uuid() {
        let mut headers = HashMap::new();
        headers.insert("x-fusillade-request-id".to_string(), vec![Bytes::from("not-a-uuid")]);
        let request = make_request_data(headers);
        assert!(FusilladeOutletHandler::extract_request_id(&request).is_none());
    }

    #[test]
    fn test_extract_api_key_strips_bearer_prefix() {
        let mut headers = HashMap::new();
        headers.insert("authorization".to_string(), vec![Bytes::from("Bearer sk-test-123")]);
        let request = make_request_data(headers);
        assert_eq!(FusilladeOutletHandler::extract_api_key(&request), Some("sk-test-123".to_string()));
    }

    #[test]
    fn test_extract_api_key_without_bearer_prefix_is_none() {
        let mut headers = HashMap::new();
        headers.insert("authorization".to_string(), vec![Bytes::from("sk-test-123")]);
        let request = make_request_data(headers);
        assert!(FusilladeOutletHandler::extract_api_key(&request).is_none());
    }

    #[test]
    fn test_extract_api_key_absent() {
        let request = make_request_data(HashMap::new());
        assert!(FusilladeOutletHandler::extract_api_key(&request).is_none());
    }

    fn full_headers() -> HashMap<String, Vec<Bytes>> {
        let mut headers = HashMap::new();
        headers.insert(
            ONWARDS_RESPONSE_ID_HEADER.to_string(),
            vec![Bytes::from("resp_12345678-1234-1234-1234-123456789abc")],
        );
        headers.insert(
            "x-fusillade-request-id".to_string(),
            vec![Bytes::from("12345678-1234-1234-1234-123456789abc")],
        );
        headers.insert("x-onwards-model".to_string(), vec![Bytes::from("Qwen/Qwen3.5-9B")]);
        headers.insert("x-onwards-endpoint".to_string(), vec![Bytes::from("http://127.0.0.1:3001/ai")]);
        headers.insert("authorization".to_string(), vec![Bytes::from("Bearer sk-test")]);
        headers
    }

    #[test]
    fn test_extract_complete_response_ctx_all_headers_present() {
        let request = make_request_data(full_headers());
        let ctx = FusilladeOutletHandler::extract_complete_response_ctx(&request).expect("should extract");
        assert_eq!(ctx.response_id, "resp_12345678-1234-1234-1234-123456789abc");
        assert_eq!(ctx.request_id, Uuid::parse_str("12345678-1234-1234-1234-123456789abc").unwrap());
        assert_eq!(ctx.model, "Qwen/Qwen3.5-9B");
        assert_eq!(ctx.endpoint, "http://127.0.0.1:3001/ai");
        assert_eq!(ctx.api_key, Some("sk-test".to_string()));
    }

    #[test]
    fn test_extract_complete_response_ctx_missing_response_id_returns_none() {
        // x-onwards-response-id absence is the silent-no-op gate: this is
        // either daemon traffic or unrelated, and the handler must return
        // without enqueueing anything.
        let mut headers = full_headers();
        headers.remove(ONWARDS_RESPONSE_ID_HEADER);
        let request = make_request_data(headers);
        assert!(FusilladeOutletHandler::extract_complete_response_ctx(&request).is_none());
    }

    #[test]
    fn test_extract_complete_response_ctx_missing_request_id_returns_none() {
        let mut headers = full_headers();
        headers.remove("x-fusillade-request-id");
        let request = make_request_data(headers);
        assert!(FusilladeOutletHandler::extract_complete_response_ctx(&request).is_none());
    }

    #[test]
    fn test_extract_complete_response_ctx_invalid_request_id_returns_none() {
        let mut headers = full_headers();
        headers.insert("x-fusillade-request-id".to_string(), vec![Bytes::from("not-a-uuid")]);
        let request = make_request_data(headers);
        assert!(FusilladeOutletHandler::extract_complete_response_ctx(&request).is_none());
    }

    #[test]
    fn test_extract_complete_response_ctx_missing_endpoint_warns_but_returns_some() {
        // Empty endpoint is non-fatal — extraction returns Some so the
        // UPDATE path can still succeed if the row already exists.
        // complete_response_idempotent refuses to synthesize on empty
        // endpoint; that's the failure mode, not silent skip here.
        let mut headers = full_headers();
        headers.remove("x-onwards-endpoint");
        let request = make_request_data(headers);
        let ctx = FusilladeOutletHandler::extract_complete_response_ctx(&request).expect("should still extract");
        assert_eq!(ctx.endpoint, "");
    }

    #[test]
    fn test_extract_complete_response_ctx_missing_model_defaults_to_unknown() {
        let mut headers = full_headers();
        headers.remove("x-onwards-model");
        let request = make_request_data(headers);
        let ctx = FusilladeOutletHandler::extract_complete_response_ctx(&request).expect("should extract");
        assert_eq!(ctx.model, "unknown");
    }

    #[test]
    fn test_extract_complete_response_ctx_missing_api_key_is_none() {
        let mut headers = full_headers();
        headers.remove("authorization");
        let request = make_request_data(headers);
        let ctx = FusilladeOutletHandler::extract_complete_response_ctx(&request).expect("should extract");
        assert_eq!(ctx.api_key, None);
    }

    // TRANSITIONAL (dwctl ZDR): the scrubber must blank bodies for ZDR-marked
    // requests before the inner analytics handler sees them, and leave ordinary
    // requests untouched. Remove with `ZdrBodyScrubber`.
    #[derive(Clone, Default)]
    struct BodyRecorder {
        req: std::sync::Arc<std::sync::Mutex<Option<Option<Bytes>>>>,
        resp: std::sync::Arc<std::sync::Mutex<Option<Option<Bytes>>>>,
    }

    impl RequestHandler for BodyRecorder {
        fn handle_request(&self, data: RequestData) -> impl std::future::Future<Output = ()> + Send {
            *self.req.lock().unwrap() = Some(data.body.clone());
            async {}
        }

        fn handle_response(&self, request_data: RequestData, response_data: ResponseData) -> impl std::future::Future<Output = ()> + Send {
            *self.req.lock().unwrap() = Some(request_data.body.clone());
            *self.resp.lock().unwrap() = Some(response_data.body.clone());
            async {}
        }
    }

    fn body_request(zdr: bool) -> RequestData {
        let mut headers = HashMap::new();
        if zdr {
            headers.insert(crate::inference::zdr::ZDR_MARKER_HEADER.to_string(), vec![Bytes::from_static(b"1")]);
        }
        let mut data = make_request_data(headers);
        data.body = Some(Bytes::from_static(br#"{"secret":"prompt"}"#));
        data
    }

    fn body_response() -> ResponseData {
        ResponseData {
            correlation_id: 1,
            timestamp: SystemTime::now(),
            status: axum::http::StatusCode::OK,
            headers: HashMap::new(),
            body: Some(Bytes::from_static(br#"{"secret":"reply"}"#)),
            duration_to_first_byte: std::time::Duration::from_millis(1),
            duration: std::time::Duration::from_millis(2),
        }
    }

    #[tokio::test]
    async fn zdr_scrubber_blanks_marked_request_body() {
        let rec = BodyRecorder::default();
        ZdrBodyScrubber::new(rec.clone()).handle_request(body_request(true)).await;
        assert_eq!(*rec.req.lock().unwrap(), Some(None), "ZDR request body must be blanked");
    }

    #[tokio::test]
    async fn zdr_scrubber_blanks_marked_response_bodies() {
        let rec = BodyRecorder::default();
        ZdrBodyScrubber::new(rec.clone())
            .handle_response(body_request(true), body_response())
            .await;
        assert_eq!(
            *rec.req.lock().unwrap(),
            Some(None),
            "ZDR request body must be blanked on response log"
        );
        assert_eq!(*rec.resp.lock().unwrap(), Some(None), "ZDR response body must be blanked");
    }

    #[tokio::test]
    async fn zdr_scrubber_passes_through_unmarked() {
        let rec = BodyRecorder::default();
        ZdrBodyScrubber::new(rec.clone())
            .handle_response(body_request(false), body_response())
            .await;
        assert!(
            rec.req.lock().unwrap().clone().unwrap().is_some(),
            "non-ZDR request body must pass through"
        );
        assert!(
            rec.resp.lock().unwrap().clone().unwrap().is_some(),
            "non-ZDR response body must pass through"
        );
    }
}
