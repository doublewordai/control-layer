//! Outlet `RequestHandler` that enqueues response completion jobs via underway.
//!
//! This handler is added to outlet's `MultiHandler` alongside the existing
//! `PostgresHandler` (which writes to `http_analytics`). It enqueues a
//! `CompleteResponseJob` carrying both the response data and enough request
//! context to synthesize the fusillade row from scratch — `CompleteResponseJob`
//! and `CreateResponseJob` are race-tolerant: whichever wins, the final state
//! is correct.

use outlet::{RequestData, RequestHandler, ResponseData};
use std::sync::Arc;
use underway::Job;
use uuid::Uuid;

use super::jobs::CompleteResponseInput;
use super::store::ONWARDS_RESPONSE_ID_HEADER;
use crate::tasks::TaskState;

/// Outlet handler that enqueues completion jobs for fusillade-tracked responses.
#[derive(Clone)]
pub struct FusilladeOutletHandler {
    job: Arc<Job<CompleteResponseInput, TaskState>>,
}

impl FusilladeOutletHandler {
    pub fn new(job: Arc<Job<CompleteResponseInput, TaskState>>) -> Self {
        Self { job }
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
        // Only the responses middleware sets `x-onwards-response-id`, so its
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

impl RequestHandler for FusilladeOutletHandler {
    async fn handle_request(&self, _data: RequestData) {}

    fn handle_response(&self, request_data: RequestData, response_data: ResponseData) -> impl std::future::Future<Output = ()> + Send {
        let job = self.job.clone();

        async move {
            let Some(ctx) = Self::extract_complete_response_ctx(&request_data) else {
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

            if let Err(e) = job
                .enqueue(&CompleteResponseInput {
                    response_id: ctx.response_id.clone(),
                    status_code,
                    response_body,
                    request_id: ctx.request_id,
                    request_body,
                    model: ctx.model,
                    endpoint: ctx.endpoint,
                    base_url: String::new(),
                    api_key: ctx.api_key,
                })
                .await
            {
                tracing::warn!(error = %e, response_id = %ctx.response_id, "Failed to enqueue complete-response job");
            }
        }
    }

    fn handle_abandoned(&self, request_data: RequestData) -> impl std::future::Future<Output = ()> + Send {
        let job = self.job.clone();

        async move {
            let Some(ctx) = Self::extract_complete_response_ctx(&request_data) else {
                return;
            };

            // 499 Client Closed Request — nginx-popularized status for this
            // scenario. The structured body distinguishes the row from a
            // real upstream 5xx in the responses listing; status 499 also
            // maps to `client_disconnected` in `status_to_error_type` so
            // the body and the derived UI error type agree.
            //
            // We omit `request_body` (set to "") because outlet's
            // AbandonGuard builds the abandon-time RequestData before body
            // capture completes — `request_data.body` is always None on
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

            if let Err(e) = job
                .enqueue(&CompleteResponseInput {
                    response_id: ctx.response_id.clone(),
                    status_code: STATUS_CLIENT_CLOSED,
                    response_body: abandoned_body,
                    request_id: ctx.request_id,
                    request_body: String::new(),
                    model: ctx.model,
                    endpoint: ctx.endpoint,
                    base_url: String::new(),
                    api_key: ctx.api_key,
                })
                .await
            {
                tracing::warn!(error = %e, response_id = %ctx.response_id, "Failed to enqueue complete-response job for abandoned request");
            }
        }
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
        // Responses middleware sets `x-fusillade-request-id` for the ID
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
}
