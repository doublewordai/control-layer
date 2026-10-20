//! Keeps credentials out of the request log.
//!
//! `outlet_postgres::PostgresHandler` stores every request header in
//! `http_requests.headers` and every response header in `http_responses.headers`.
//! It builds those columns straight from the captured header map, not from the
//! serializer dwctl supplies, so the only place to remove a header before it is
//! written is a `RequestHandler` that wraps the Postgres handler and edits the
//! captured data on the way through. That is what [`CredentialHeaderScrubber`] is.
//!
//! It wraps ONLY the Postgres handler. The analytics handler in the same chain
//! still needs the bearer token to attribute the request to an API key, and it
//! receives its own clone of the captured data, so it is unaffected.
//!
//! The serializer (`parse_ai_request`) also copies the header map into
//! `ParsedAIRequest.headers`, which lands in `http_requests.body`. The Postgres
//! handler runs the serializer on the data this wrapper hands it, so that copy is
//! scrubbed too.

use std::collections::HashMap;

use bytes::Bytes;
use outlet::{RequestData, RequestHandler, ResponseData};

/// Request headers that carry a credential. Matched case-insensitively.
///
/// `authorization` and `x-api-key` are the headers the `/ai` proxy authenticates
/// with (`x-api-key` on the Anthropic-shaped endpoints). `proxy-authorization`
/// and `cookie` are not read by the proxy but a client may still send them, and a
/// session cookie is as good as a key.
const REQUEST_HEADER_DENYLIST: [&str; 4] = ["authorization", "proxy-authorization", "x-api-key", "cookie"];

/// Request header prefixes that carry a credential. An SSO proxy in front of
/// dwctl forwards the upstream identity as `x-auth-request-*`, which can include
/// the access token.
const REQUEST_HEADER_PREFIX_DENYLIST: [&str; 1] = ["x-auth-request-"];

/// Response headers that carry a credential.
const RESPONSE_HEADER_DENYLIST: [&str; 1] = ["set-cookie"];

fn is_denied_request_header(name: &str) -> bool {
    REQUEST_HEADER_DENYLIST.iter().any(|denied| name.eq_ignore_ascii_case(denied))
        || REQUEST_HEADER_PREFIX_DENYLIST
            .iter()
            .any(|prefix| name.len() >= prefix.len() && name[..prefix.len()].eq_ignore_ascii_case(prefix))
}

fn is_denied_response_header(name: &str) -> bool {
    RESPONSE_HEADER_DENYLIST.iter().any(|denied| name.eq_ignore_ascii_case(denied))
}

/// Remove credential-bearing request headers in place.
pub fn scrub_request_headers(headers: &mut HashMap<String, Vec<Bytes>>) {
    headers.retain(|name, _| !is_denied_request_header(name));
}

/// Remove credential-bearing response headers in place.
pub fn scrub_response_headers(headers: &mut HashMap<String, Vec<Bytes>>) {
    headers.retain(|name, _| !is_denied_response_header(name));
}

/// A `RequestHandler` that strips credential headers from the captured request
/// and response before the handler it wraps persists them. See the module docs.
#[derive(Clone)]
pub struct CredentialHeaderScrubber<H> {
    inner: H,
}

impl<H> CredentialHeaderScrubber<H> {
    pub fn new(inner: H) -> Self {
        Self { inner }
    }
}

impl<H: RequestHandler> RequestHandler for CredentialHeaderScrubber<H> {
    fn handle_request(&self, mut data: RequestData) -> impl std::future::Future<Output = ()> + Send {
        scrub_request_headers(&mut data.headers);
        self.inner.handle_request(data)
    }

    fn handle_response(
        &self,
        mut request_data: RequestData,
        mut response_data: ResponseData,
    ) -> impl std::future::Future<Output = ()> + Send {
        scrub_request_headers(&mut request_data.headers);
        scrub_response_headers(&mut response_data.headers);
        self.inner.handle_response(request_data, response_data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{Method, StatusCode};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime};

    /// Records exactly what the wrapped handler would have been given.
    #[derive(Clone, Default)]
    struct Recorder {
        requests: Arc<Mutex<Vec<RequestData>>>,
        responses: Arc<Mutex<Vec<(RequestData, ResponseData)>>>,
    }

    impl RequestHandler for Recorder {
        async fn handle_request(&self, data: RequestData) {
            self.requests.lock().unwrap().push(data);
        }

        async fn handle_response(&self, request_data: RequestData, response_data: ResponseData) {
            self.responses.lock().unwrap().push((request_data, response_data));
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> HashMap<String, Vec<Bytes>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), vec![Bytes::copy_from_slice(v.as_bytes())]))
            .collect()
    }

    fn request(headers: HashMap<String, Vec<Bytes>>) -> RequestData {
        RequestData {
            correlation_id: 1,
            timestamp: SystemTime::now(),
            method: Method::POST,
            uri: "/ai/v1/chat/completions".parse().unwrap(),
            headers,
            body: Some(Bytes::from_static(b"{}")),
            trace_id: None,
            span_id: None,
        }
    }

    fn response(headers: HashMap<String, Vec<Bytes>>) -> ResponseData {
        ResponseData {
            correlation_id: 1,
            timestamp: SystemTime::now(),
            status: StatusCode::OK,
            headers,
            body: None,
            duration_to_first_byte: Duration::ZERO,
            duration: Duration::ZERO,
            extensions: Default::default(),
        }
    }

    const CREDENTIAL_REQUEST_HEADERS: [(&str, &str); 6] = [
        ("authorization", "Bearer sk-secret"),
        ("proxy-authorization", "Basic c2VjcmV0"),
        ("x-api-key", "sk-secret"),
        ("cookie", "session=secret"),
        ("x-auth-request-access-token", "secret-token"),
        ("x-auth-request-email", "someone@example.com"),
    ];

    const KEPT_REQUEST_HEADERS: [(&str, &str); 5] = [
        ("content-type", "application/json"),
        ("user-agent", "test-client/1.0"),
        ("traceparent", "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"),
        ("x-fusillade-request-id", "req-1"),
        ("x-fusillade-batch-zdr", "1"),
    ];

    #[tokio::test]
    async fn request_credential_headers_do_not_reach_the_wrapped_handler() {
        let recorder = Recorder::default();
        let scrubber = CredentialHeaderScrubber::new(recorder.clone());

        let mut all = CREDENTIAL_REQUEST_HEADERS.to_vec();
        all.extend_from_slice(&KEPT_REQUEST_HEADERS);
        scrubber.handle_request(request(headers(&all))).await;

        let seen = recorder.requests.lock().unwrap();
        let seen = &seen[0].headers;
        for (name, _) in CREDENTIAL_REQUEST_HEADERS {
            assert!(!seen.contains_key(name), "{name} reached the wrapped handler: {seen:?}");
        }
        for (name, value) in KEPT_REQUEST_HEADERS {
            assert_eq!(seen.get(name).map(|v| v[0].as_ref()), Some(value.as_bytes()), "{name} was dropped");
        }
    }

    #[tokio::test]
    async fn header_names_match_regardless_of_case() {
        let recorder = Recorder::default();
        let scrubber = CredentialHeaderScrubber::new(recorder.clone());

        scrubber
            .handle_request(request(headers(&[
                ("Authorization", "Bearer sk-secret"),
                ("X-API-KEY", "sk-secret"),
                ("X-Auth-Request-Access-Token", "secret-token"),
                ("Content-Type", "application/json"),
            ])))
            .await;

        let seen = recorder.requests.lock().unwrap();
        let names: Vec<&String> = seen[0].headers.keys().collect();
        assert_eq!(names, vec!["Content-Type"], "{names:?}");
    }

    #[tokio::test]
    async fn response_path_scrubs_both_request_and_response_headers() {
        let recorder = Recorder::default();
        let scrubber = CredentialHeaderScrubber::new(recorder.clone());

        scrubber
            .handle_response(
                request(headers(&[
                    ("authorization", "Bearer sk-secret"),
                    ("content-type", "application/json"),
                ])),
                response(headers(&[("set-cookie", "session=secret"), ("content-type", "application/json")])),
            )
            .await;

        let seen = recorder.responses.lock().unwrap();
        let (req, resp) = &seen[0];
        assert_eq!(req.headers.keys().collect::<Vec<_>>(), vec!["content-type"]);
        assert_eq!(resp.headers.keys().collect::<Vec<_>>(), vec!["content-type"]);
    }

    #[tokio::test]
    async fn everything_else_passes_through_untouched() {
        let recorder = Recorder::default();
        let scrubber = CredentialHeaderScrubber::new(recorder.clone());

        let mut data = request(headers(&[("authorization", "Bearer sk-secret")]));
        data.correlation_id = 42;
        data.trace_id = Some("0af7651916cd43dd8448eb211c80319c".to_string());
        data.body = Some(Bytes::from_static(b"{\"model\":\"m\"}"));
        scrubber.handle_request(data).await;

        let seen = recorder.requests.lock().unwrap();
        assert_eq!(seen[0].correlation_id, 42);
        assert_eq!(seen[0].trace_id.as_deref(), Some("0af7651916cd43dd8448eb211c80319c"));
        assert_eq!(seen[0].body.as_deref(), Some(&b"{\"model\":\"m\"}"[..]));
    }
}
