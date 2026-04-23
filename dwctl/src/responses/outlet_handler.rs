//! Outlet `RequestHandler` that enqueues response completion jobs via underway.
//!
//! This handler is added to outlet's `MultiHandler` alongside the existing
//! `PostgresHandler` (which writes to `http_analytics`). It enqueues a
//! `CompleteResponseJob` which updates the fusillade row with the response
//! body/status. Using underway ensures the completion retries if the
//! `CreateResponseJob` hasn't created the row yet.

use outlet::{RequestData, RequestHandler, ResponseData};
use std::sync::Arc;
use underway::Job;

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
        request
            .headers
            .get(ONWARDS_RESPONSE_ID_HEADER)
            .and_then(|values| values.first())
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .map(|s| s.to_string())
    }
}

impl RequestHandler for FusilladeOutletHandler {
    async fn handle_request(&self, _data: RequestData) {}

    fn handle_response(&self, request_data: RequestData, response_data: ResponseData) -> impl std::future::Future<Output = ()> + Send {
        let job = self.job.clone();

        async move {
            // Skip batch requests — the daemon handles its own completion.
            // Batch requests have both x-fusillade-request-id and x-fusillade-batch-id.
            // Realtime requests only have x-fusillade-request-id (set by the
            // responses middleware for ID override) and should NOT be skipped.
            if request_data.headers.contains_key("x-fusillade-batch-id") {
                return;
            }

            // Check for the onwards response ID header (set by responses middleware)
            let response_id = match Self::extract_response_id(&request_data) {
                Some(id) => id,
                None => return,
            };

            let status_code = response_data.status.as_u16();
            let response_body = response_data
                .body
                .as_ref()
                .and_then(|b| std::str::from_utf8(b).ok())
                .unwrap_or("")
                .to_string();

            if let Err(e) = job
                .enqueue(&CompleteResponseInput {
                    response_id: response_id.clone(),
                    status_code,
                    response_body,
                })
                .await
            {
                tracing::warn!(error = %e, response_id = %response_id, "Failed to enqueue complete-response job");
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
}
