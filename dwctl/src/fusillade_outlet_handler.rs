//! Outlet `RequestHandler` that writes captured response bodies back to
//! fusillade's `requests` table.
//!
//! This handler is added to outlet's `MultiHandler` alongside the existing
//! `PostgresHandler` (which writes to `http_analytics`). It completes the
//! response lifecycle that the responses middleware started.

use outlet::{RequestData, RequestHandler, ResponseData};
use sqlx::PgPool;

use crate::response_store::{self, ONWARDS_RESPONSE_ID_HEADER};

/// Outlet handler that updates fusillade request rows with captured response data.
#[derive(Clone)]
pub struct FusilladeOutletHandler {
    pool: PgPool,
}

impl FusilladeOutletHandler {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
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
    fn handle_request(&self, _data: RequestData) -> impl std::future::Future<Output = ()> + Send {
        async {}
    }

    fn handle_response(
        &self,
        request_data: RequestData,
        response_data: ResponseData,
    ) -> impl std::future::Future<Output = ()> + Send {
        let pool = self.pool.clone();

        async move {
            // Skip batch requests — the daemon handles its own completion
            if request_data.headers.contains_key("x-fusillade-request-id") {
                return;
            }

            // Check for the onwards response ID header (set by responses middleware)
            let response_id = match Self::extract_response_id(&request_data) {
                Some(id) => id,
                None => return, // Not a tracked response
            };

            let status_code = response_data.status.as_u16();
            let body_str = response_data
                .body
                .as_ref()
                .and_then(|b| std::str::from_utf8(b).ok())
                .unwrap_or("");

            if status_code >= 200 && status_code < 300 {
                if let Err(e) =
                    response_store::complete_response(&pool, &response_id, body_str, status_code)
                        .await
                {
                    tracing::warn!(
                        error = %e,
                        response_id = %response_id,
                        "Failed to complete response in fusillade"
                    );
                } else {
                    tracing::debug!(
                        response_id = %response_id,
                        status_code = status_code,
                        body_size = body_str.len(),
                        "Response completed in fusillade"
                    );
                }
            } else {
                if let Err(e) = response_store::fail_response(
                    &pool,
                    &response_id,
                    &format!("Upstream returned {status_code}: {body_str}"),
                )
                .await
                {
                    tracing::warn!(
                        error = %e,
                        response_id = %response_id,
                        "Failed to mark response as failed in fusillade"
                    );
                } else {
                    tracing::debug!(
                        response_id = %response_id,
                        status_code = status_code,
                        "Response marked as failed in fusillade"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::collections::HashMap;
    use std::time::{Duration, SystemTime};

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
        assert_eq!(
            id,
            Some("resp_12345678-1234-1234-1234-123456789abc".to_string())
        );
    }

    #[test]
    fn test_extract_response_id_absent() {
        let request = make_request_data(HashMap::new());
        let id = FusilladeOutletHandler::extract_response_id(&request);
        assert!(id.is_none());
    }

    #[test]
    fn test_extract_response_id_skips_fusillade_header() {
        // When X-Fusillade-Request-Id is present, the handler should skip
        // (tested in handle_response, not extract — but verify extraction still works)
        let mut headers = HashMap::new();
        headers.insert(
            "x-fusillade-request-id".to_string(),
            vec![Bytes::from("some-batch-id")],
        );
        // No onwards header → extraction returns None
        let request = make_request_data(headers);
        let id = FusilladeOutletHandler::extract_response_id(&request);
        assert!(id.is_none());
    }
}
