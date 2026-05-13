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

    /// Extract the raw fusillade batch UUID from `x-fusillade-batch-id`.
    fn extract_batch_id(request: &RequestData) -> Option<Uuid> {
        Self::header_str(request, "x-fusillade-batch-id").and_then(|s| Uuid::parse_str(s).ok())
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
}

impl RequestHandler for FusilladeOutletHandler {
    async fn handle_request(&self, _data: RequestData) {}

    fn handle_response(&self, request_data: RequestData, response_data: ResponseData) -> impl std::future::Future<Output = ()> + Send {
        let job = self.job.clone();

        async move {
            // Only the responses middleware sets `x-onwards-response-id`, so
            // its absence means this is either a daemon-driven fusillade batch
            // request (the daemon handles its own completion) or some other
            // non-responses traffic — nothing for us to do.
            let response_id = match Self::extract_response_id(&request_data) {
                Some(id) => id,
                None => return,
            };

            // We also need the raw request UUID for the create-if-missing path.
            // The responses middleware always sets both headers together; if it's
            // missing here something upstream is broken — bail out.
            let request_id = match Self::extract_request_id(&request_data) {
                Some(id) => id,
                None => {
                    tracing::warn!(response_id = %response_id, "Missing x-fusillade-request-id header on response — skipping enqueue");
                    return;
                }
            };

            // Same story for the batch_id — middleware always sets it alongside
            // request_id. If it's missing, complete-response would synthesize a
            // row with a fresh batch_id that doesn't match what create-response
            // used, breaking the analytics join.
            let batch_id = match Self::extract_batch_id(&request_data) {
                Some(id) => id,
                None => {
                    tracing::warn!(response_id = %response_id, "Missing x-fusillade-batch-id header on response — skipping enqueue");
                    return;
                }
            };

            let status_code = response_data.status.as_u16();
            let response_body = response_data
                .body
                .as_ref()
                .and_then(|b| std::str::from_utf8(b).ok())
                .unwrap_or("")
                .to_string();

            // Context used by complete-response if it has to synthesize the row
            // (i.e., raced ahead of create-response). The middleware sets these
            // headers explicitly so we don't have to parse the body or guess
            // path nesting.
            let request_body = request_data
                .body
                .as_ref()
                .and_then(|b| std::str::from_utf8(b).ok())
                .unwrap_or("")
                .to_string();
            let model = Self::header_str(&request_data, "x-onwards-model").unwrap_or("unknown").to_string();
            let endpoint = Self::header_str(&request_data, "x-onwards-endpoint").unwrap_or("").to_string();
            let api_key = Self::extract_api_key(&request_data);

            if endpoint.is_empty() {
                // The responses middleware always sets x-onwards-endpoint when
                // it intercepts. If we get here without it, complete-response
                // would synthesize a row with an empty endpoint that the
                // /responses lookup queries can't find. Log loudly.
                tracing::warn!(
                    response_id = %response_id,
                    uri = %request_data.uri,
                    "Missing x-onwards-endpoint header on captured request — complete-response will fail"
                );
            }

            if let Err(e) = job
                .enqueue(&CompleteResponseInput {
                    response_id: response_id.clone(),
                    status_code,
                    response_body,
                    batch_id,
                    request_id,
                    request_body,
                    model,
                    endpoint,
                    base_url: String::new(),
                    api_key,
                })
                .await
            {
                tracing::warn!(error = %e, response_id = %response_id, "Failed to enqueue complete-response job");
            }
        }
    }

    fn handle_abandoned(&self, request_data: RequestData) -> impl std::future::Future<Output = ()> + Send {
        let job = self.job.clone();

        async move {
            // Same header-presence gating as handle_response — without
            // `x-onwards-response-id` this wasn't a responses-middleware
            // request, so nothing for us to do.
            let response_id = match Self::extract_response_id(&request_data) {
                Some(id) => id,
                None => return,
            };
            let request_id = match Self::extract_request_id(&request_data) {
                Some(id) => id,
                None => {
                    tracing::warn!(response_id = %response_id, "Missing x-fusillade-request-id header on abandoned request — skipping enqueue");
                    return;
                }
            };
            let batch_id = match Self::extract_batch_id(&request_data) {
                Some(id) => id,
                None => {
                    tracing::warn!(response_id = %response_id, "Missing x-fusillade-batch-id header on abandoned request — skipping enqueue");
                    return;
                }
            };

            // We don't have a body captured here — RequestData::body is None
            // because outlet's AbandonGuard builds a minimal record before
            // body capture completes. That's fine for create-if-missing:
            // the row's body column ends up empty, which is correct since
            // no upstream call ever happened. The endpoint/model headers
            // still need to be present so the synthesized row is queryable.
            let model = Self::header_str(&request_data, "x-onwards-model").unwrap_or("unknown").to_string();
            let endpoint = Self::header_str(&request_data, "x-onwards-endpoint").unwrap_or("").to_string();
            let api_key = Self::extract_api_key(&request_data);

            if endpoint.is_empty() {
                tracing::warn!(
                    response_id = %response_id,
                    uri = %request_data.uri,
                    "Missing x-onwards-endpoint header on abandoned request — complete-response synthesize will fail"
                );
            }

            // 499 Client Closed Request — nginx-popularized status for
            // exactly this scenario. The structured body marks the row
            // distinguishable from a real upstream 5xx in the responses
            // listing without inventing a new schema.
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
                    response_id: response_id.clone(),
                    status_code: STATUS_CLIENT_CLOSED,
                    response_body: abandoned_body,
                    batch_id,
                    request_id,
                    request_body: String::new(),
                    model,
                    endpoint,
                    base_url: String::new(),
                    api_key,
                })
                .await
            {
                tracing::warn!(error = %e, response_id = %response_id, "Failed to enqueue complete-response job for abandoned request");
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
    fn test_extract_response_id_not_skipped_for_realtime_with_fusillade_header() {
        // Realtime requests have x-fusillade-request-id (for ID override) but
        // NOT x-fusillade-batch-id — they should still be processed.
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
}
