//! Rebuilding an [`outlet`] request/response pair from a stored fusillade exchange.
//!
//! The recompute engine deliberately reuses the live path's serializer rather than parsing
//! payloads itself (see the module docs). That serializer takes the `RequestData` /
//! `ResponseData` the proxy saw at the time, so a replay has to reconstruct enough of that
//! shape for parsing to dispatch the same way it did originally.
//!
//! "Enough" is a short list, but every item on it is load-bearing:
//!
//! - the **request path**, because `/v1/messages` and `/v1/responses` are detected by path
//!   and have wholly different response shapes from chat completions;
//! - the **request body**, because the Anthropic and Responses arms read its `stream` flag;
//! - whether the request was **streamed**, because a streaming response is a sequence of
//!   SSE frames and the usage lives in the terminal one;
//! - the **response body**, which is the thing actually being re-read.
//!
//! A *blocking* body is largely self-describing — `AiResponse` is an untagged enum, so an
//! Anthropic message still deserialises correctly even if the endpoint is wrong. **Streaming
//! is not**: Anthropic's `message_start` / `message_delta` lifecycle is nothing like a
//! chat-completion chunk stream, and the wrong parser finds no usage at all. A recompute run
//! against a mis-recorded endpoint would then propose billing real requests at zero. That is
//! why [`StoredExchange::endpoint`] is a required field rather than an inferred one.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use axum::http::{Method, StatusCode, Uri};
use bytes::Bytes;
use outlet::{RequestData, ResponseData};
use outlet_postgres::SerializationError;
use serde_json::Value;

/// Why a recompute could not produce an answer for one request.
///
/// These are per-request outcomes, not run failures: a run reports them per item and
/// carries on, because one unparseable payload must not abort a 20k-request batch.
#[derive(Debug, thiserror::Error)]
pub enum RecomputeError {
    /// The stored response could not be parsed into a known AI response shape.
    #[error("stored response could not be parsed: {0}")]
    Parse(SerializationError),
    /// The request never completed, so there is no response to re-read. Nothing to
    /// recompute and nothing wrong — a pending or failed request was never billed.
    #[error("request has no stored response body")]
    NoResponseBody,
    /// The stored endpoint is not a URI we can rebuild, so parsing would dispatch on the
    /// wrong shape. Refused rather than guessed.
    #[error("stored endpoint {0:?} is not a valid request path")]
    BadEndpoint(String),
    /// The stored HTTP status is not a valid status code, so this row is corrupt. Refused
    /// rather than coerced: everything downstream gates on 2xx, so any substituted value
    /// is a claim about whether the request succeeded that the data does not support.
    #[error("stored status code {0} is not a valid HTTP status")]
    BadStatus(u16),
}

/// One request as fusillade stored it, in the minimal form a replay needs.
#[derive(Debug, Clone)]
pub struct StoredExchange {
    /// The endpoint the request was made against, e.g. `/v1/chat/completions`,
    /// `/v1/messages`. Required: response parsing dispatches on this path, and guessing it
    /// would silently mis-parse a whole ingress. For batch requests this is
    /// `batches.endpoint`.
    pub endpoint: String,
    /// The original request body (fusillade's `request_templates.body`).
    pub request_body: Option<Vec<u8>>,
    /// The stored response body (fusillade's `requests.response_body`). `None` for a
    /// request that never completed.
    pub response_body: Option<Vec<u8>>,
    /// The upstream HTTP status, so an error body is classified as one rather than being
    /// read for usage.
    pub status_code: u16,
    /// Whether the response was streamed. Defaults from the request body's `stream` flag
    /// via [`StoredExchange::streamed_from_body`], but batch dispatch can set streaming
    /// independently of the stored template, so callers that know better should say so.
    pub streamed: bool,
}

impl StoredExchange {
    /// Read the `stream` flag out of a stored request body. The fallback when the caller
    /// has no better source; `false` when the body is absent or unparseable.
    pub fn streamed_from_body(request_body: Option<&[u8]>) -> bool {
        request_body
            .and_then(|b| serde_json::from_slice::<Value>(b).ok())
            .and_then(|v| v.get("stream").and_then(Value::as_bool))
            .unwrap_or(false)
    }

    /// Rebuild the `(RequestData, ResponseData)` pair the live serializer expects.
    pub fn to_outlet_pair(&self) -> Result<(RequestData, ResponseData), RecomputeError> {
        let Some(response_body) = &self.response_body else {
            return Err(RecomputeError::NoResponseBody);
        };

        let uri: Uri = self
            .endpoint
            .parse()
            .map_err(|_| RecomputeError::BadEndpoint(self.endpoint.clone()))?;

        // Refuse a corrupt status rather than substituting one. `from_u16` only rejects
        // values outside 100-999, so this is a broken or never-populated row, and every
        // consumer downstream gates on 2xx: only successful requests are billed, and the
        // canonical `http_analytics` row an apply amends is chosen from among a request's
        // 2xx rows. Coercing to 200 would assert success we cannot evidence; coercing to
        // 500 would assert failure just as baselessly. Surfacing it as un-replayable puts
        // the row in front of a human, consistent with the other refusals here.
        let status = StatusCode::from_u16(self.status_code).map_err(|_| RecomputeError::BadStatus(self.status_code))?;

        // The serializer reads `x-fusillade-stream` to know a streaming response is coming
        // even when the captured body has no `stream:true` (onwards injects it downstream).
        // Setting it here is how a replay reproduces that dispatch.
        let mut headers: HashMap<String, Vec<Bytes>> = HashMap::new();
        if self.streamed {
            headers.insert("x-fusillade-stream".to_string(), vec![Bytes::from_static(b"true")]);
        }

        let request_data = RequestData {
            correlation_id: 0,
            timestamp: SystemTime::UNIX_EPOCH,
            method: Method::POST,
            uri,
            headers,
            body: self.request_body.clone().map(Bytes::from),
            trace_id: None,
            span_id: None,
        };

        let response_data = ResponseData {
            extensions: Default::default(),
            correlation_id: 0,
            timestamp: SystemTime::UNIX_EPOCH,
            status,
            headers: HashMap::new(),
            body: Some(Bytes::from(response_body.clone())),
            // Durations are not re-derived: a recompute corrects token counts and cost, and
            // must never overwrite the latency actually observed at inference time.
            duration: Duration::ZERO,
            duration_to_first_byte: Duration::ZERO,
        };

        Ok((request_data, response_data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recompute::cache_fields::CreationTier;
    use crate::recompute::{TokenSource, recompute_from_stored_response};

    fn exchange(endpoint: &str, request: &str, response: &str) -> StoredExchange {
        StoredExchange {
            endpoint: endpoint.to_string(),
            request_body: Some(request.as_bytes().to_vec()),
            response_body: Some(response.as_bytes().to_vec()),
            status_code: 200,
            streamed: false,
        }
    }

    /// A recompute of a healthy chat completion must reproduce exactly what was already
    /// recorded. This is the property the whole feature rests on: pointed at traffic with
    /// nothing wrong with it, the tool proposes no change.
    #[test]
    fn healthy_chat_completion_recomputes_to_the_same_numbers() {
        let e = exchange(
            "/v1/chat/completions",
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
            r#"{"id":"chatcmpl-1","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":20000,"completion_tokens":8,"total_tokens":20008}}"#,
        );
        let (req, resp) = e.to_outlet_pair().unwrap();
        let got = recompute_from_stored_response(&req, &resp, CreationTier::FiveMinute).unwrap();

        assert_eq!(got.counts.prompt, 20_000);
        assert_eq!(got.counts.completion, 8);
        assert_eq!(got.prompt_source, TokenSource::Reported);
        assert!(got.is_exact());
    }

    /// The Anthropic incident, as a replay. `input_tokens` is 5000 with 12k read from cache
    /// and 3k written; the correct total input is 20000. A recompute through the current
    /// serializer recovers that, and — critically — leaves the cache split exactly as the
    /// response reported it, because that is the part no tokenizer could reconstruct.
    #[test]
    fn anthropic_cached_request_recovers_the_total_and_preserves_the_split() {
        let e = exchange(
            "/v1/messages",
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"max_tokens":16}"#,
            r#"{"id":"msg_1","type":"message","role":"assistant","model":"m","content":[],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":5000,"output_tokens":8,"cache_read_input_tokens":12000,"cache_creation_input_tokens":3000,"cache_creation":{"ephemeral_1h_input_tokens":3000}}}"#,
        );
        let (req, resp) = e.to_outlet_pair().unwrap();
        let got = recompute_from_stored_response(&req, &resp, CreationTier::FiveMinute).unwrap();

        assert_eq!(got.counts.prompt, 20_000, "cached tokens are folded back into the total");
        assert_eq!(got.counts.cache_read, 12_000, "the split is carried through untouched");
        assert_eq!(got.counts.cache_creation_1h, 3_000);
        assert!(got.is_exact());
    }

    /// A *blocking* body is self-describing: `AiResponse` is an untagged enum, so an
    /// Anthropic message still deserialises into the Anthropic variant even when the
    /// endpoint says chat completions. Pinned deliberately — it means a mis-recorded
    /// endpoint degrades gracefully rather than silently zeroing a real request.
    #[test]
    fn blocking_body_is_recognised_regardless_of_endpoint() {
        let anthropic_body = r#"{"id":"msg_1","type":"message","role":"assistant","model":"m","content":[],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":5000,"output_tokens":8}}"#;
        let request = r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"max_tokens":16}"#;

        for endpoint in ["/v1/messages", "/v1/chat/completions"] {
            let (req, resp) = exchange(endpoint, request, anthropic_body).to_outlet_pair().unwrap();
            let got = recompute_from_stored_response(&req, &resp, CreationTier::FiveMinute).unwrap();
            assert_eq!(got.counts.prompt, 5_000, "blocking shape recovered via {endpoint}");
        }
    }

    /// Streaming is where the endpoint genuinely decides the outcome: Anthropic's SSE
    /// lifecycle (`message_start` / `message_delta`) is nothing like a chat-completion
    /// chunk stream, and the wrong parser finds no usage at all. A recompute run on a
    /// mis-recorded endpoint would then propose billing a real request at zero — which is
    /// why [`StoredExchange::endpoint`] is a required field rather than an inferred one.
    #[test]
    fn streaming_dispatch_depends_on_the_endpoint() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"m\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":5000,\"output_tokens\":0,\"cache_read_input_tokens\":12000,\"cache_creation_input_tokens\":3000}}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":5000,\"output_tokens\":8,\"cache_read_input_tokens\":12000,\"cache_creation_input_tokens\":3000}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let request = r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"max_tokens":16,"stream":true}"#;

        let mut right = exchange("/v1/messages", request, sse);
        right.streamed = true;
        let (req, resp) = right.to_outlet_pair().unwrap();
        let right = recompute_from_stored_response(&req, &resp, CreationTier::FiveMinute).unwrap();
        assert_eq!(right.counts.prompt, 20_000, "5000 input + 12000 read + 3000 creation");
        assert_eq!(right.counts.completion, 8);

        let mut wrong = exchange("/v1/chat/completions", request, sse);
        wrong.streamed = true;
        let (req, resp) = wrong.to_outlet_pair().unwrap();
        let wrong = recompute_from_stored_response(&req, &resp, CreationTier::FiveMinute).unwrap();
        assert_eq!(
            wrong.counts.prompt, 0,
            "the chat-completion SSE parser finds no usage in an Anthropic event stream"
        );
    }

    #[test]
    fn a_request_with_no_response_is_refused_not_zeroed() {
        let e = StoredExchange {
            endpoint: "/v1/chat/completions".to_string(),
            request_body: Some(b"{}".to_vec()),
            response_body: None,
            status_code: 0,
            streamed: false,
        };
        assert!(matches!(e.to_outlet_pair(), Err(RecomputeError::NoResponseBody)));
    }

    #[test]
    fn an_unparseable_endpoint_is_refused_not_guessed() {
        let e = StoredExchange {
            endpoint: "not a uri".to_string(),
            request_body: None,
            response_body: Some(b"{}".to_vec()),
            status_code: 200,
            streamed: false,
        };
        assert!(matches!(e.to_outlet_pair(), Err(RecomputeError::BadEndpoint(_))));
    }

    /// End-to-end against a real incident row (handover §2). The stored body carries the
    /// upstream's FLAT `cache_creation_input_tokens` and no nested object — the shape the
    /// live path's nested-only parser reads as zero. Analytics recorded prompt=565,
    /// creation=0; the truth is prompt 18,734 with 538 creation tokens.
    ///
    /// This is the single most important test in the module: it is the incident.
    #[test]
    fn real_incident_row_recomputes_to_the_measured_truth() {
        let e = exchange(
            "/messages?beta=true",
            r#"{"model":"deepseek-ai/DeepSeek-V4-Flash","messages":[{"role":"user","content":"hi"}],"max_tokens":1024}"#,
            r#"{"id":"msg_1","type":"message","role":"assistant","model":"deepseek-ai/DeepSeek-V4-Flash","content":[],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":565,"output_tokens":658,"cache_read_input_tokens":17631,"cache_creation_input_tokens":538}}"#,
        );
        let (req, resp) = e.to_outlet_pair().unwrap();
        let got = recompute_from_stored_response(&req, &resp, CreationTier::FiveMinute).unwrap();

        // input + read + creation = the TOTAL input analytics means by prompt_tokens.
        assert_eq!(got.counts.prompt, 18_734, "565 + 17631 + 538");
        assert_eq!(got.counts.completion, 658);
        assert_eq!(got.counts.cache_read, 17_631);
        assert_eq!(got.counts.cache_creation_5m, 538, "flat creation must not read as zero");
        assert!(got.cache_tier_inferred, "the body gave no tier - say so in the report");

        // And the uncached residual that billing actually charges at full rate.
        assert_eq!(
            got.counts.prompt - got.counts.cache_read - got.counts.cache_creation_5m,
            565,
            "uncached input is the provider's input_tokens"
        );
    }

    /// A stored status we cannot parse means a corrupt or never-populated row. Any
    /// substitute value asserts something about success or failure that the data does not
    /// support, and everything downstream gates on 2xx — so refuse the item instead.
    #[test]
    fn an_unparseable_status_is_refused_not_coerced() {
        let body = r#"{"id":"chatcmpl-1","object":"chat.completion","created":1,"model":"m","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}}"#;
        for bogus in [0u16, 42, 1_000] {
            let e = StoredExchange {
                endpoint: "/v1/chat/completions".to_string(),
                request_body: None,
                response_body: Some(body.as_bytes().to_vec()),
                status_code: bogus,
                streamed: false,
            };
            assert!(
                matches!(e.to_outlet_pair(), Err(RecomputeError::BadStatus(s)) if s == bogus),
                "status {bogus} must be refused, not replayed"
            );
        }
    }

    #[test]
    fn a_valid_status_is_preserved_exactly() {
        let body = r#"{"error":{"message":"nope"}}"#;
        for real in [200u16, 400, 429, 502] {
            let e = StoredExchange {
                endpoint: "/v1/chat/completions".to_string(),
                request_body: None,
                response_body: Some(body.as_bytes().to_vec()),
                status_code: real,
                streamed: false,
            };
            let (_, resp) = e.to_outlet_pair().unwrap();
            assert_eq!(resp.status.as_u16(), real);
        }
    }

    #[test]
    fn stream_flag_is_read_from_the_request_body() {
        assert!(StoredExchange::streamed_from_body(Some(br#"{"stream":true}"#)));
        assert!(!StoredExchange::streamed_from_body(Some(br#"{"stream":false}"#)));
        assert!(!StoredExchange::streamed_from_body(Some(b"{}")));
        assert!(!StoredExchange::streamed_from_body(None), "absent body ⇒ not streaming");
        assert!(!StoredExchange::streamed_from_body(Some(b"not json")));
    }

    /// A streamed chat completion carries its usage in the terminal SSE frame; the replay
    /// has to reproduce the streaming dispatch to find it.
    #[test]
    fn streamed_response_usage_is_read_from_the_terminal_frame() {
        let mut e = exchange(
            "/v1/chat/completions",
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
            "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}],\"usage\":null}\n\ndata: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":5,\"total_tokens\":16}}\n\ndata: [DONE]\n\n",
        );
        e.streamed = true;

        let (req, resp) = e.to_outlet_pair().unwrap();
        let got = recompute_from_stored_response(&req, &resp, CreationTier::FiveMinute).unwrap();
        assert_eq!(got.counts.prompt, 11);
        assert_eq!(got.counts.completion, 5);
    }
}
