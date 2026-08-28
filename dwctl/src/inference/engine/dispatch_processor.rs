//! Pre-dispatch preparation for daemon-claimed requests.
//!
//! A thin [`RequestProcessor`] that performs the two dwctl-side steps which must
//! happen AFTER a row is claimed from fusillade's DB but BEFORE the dispatch HTTP
//! request exists, then delegates to [`DefaultRequestProcessor`]:
//!
//! 1. ZDR decrypt - the stored body is a `dwzdr1:` ciphertext envelope keyed per
//!    request. It is not parseable JSON, so no middleware on the loopback can act
//!    on it: every downstream layer (translation, cache, `outbound_request`,
//!    onwards' strict parse) chokes first. It has to be decrypted here.
//! 2. JIT image signing - `dw-img://{sha256}` tokens placed in the body by the
//!    file-ingest path are swapped for fresh short-lived signed URLs. The edge
//!    `image_normalizer_middleware` runs `Mode::All`, which matches HTTP(S) URLs
//!    and `data:` URIs but NOT tokens, so the loopback pass does not do this.
//!
//! Both were previously carried by `DwctlRequestProcessor`, which also ran the
//! server-side multi-step tool loop. COR-536 retired that loop and deleted the
//! whole struct, taking these two unrelated cross-cutting steps with it - ZDR flex
//! requests then dispatched ciphertext and 400'd at onwards' strict parse. This
//! module restores only the pre-dispatch preparation; the tool loop stays retired
//! (dispatch loops back through the full dwctl edge, which does that job now).
//!
//! Both ZDR failure branches terminalize by persisting `Failed` and returning
//! `Ok(RequestCompletionResult::Failed(..))` rather than a bare `Err`: an `Err`
//! only logs a task failure and leaves the row stuck in `processing` ("running")
//! until the batch window expires.
//!
//! Planned removal: COR-521 moves ZDR encrypt/decrypt into dwctl's own middleware
//! and drops fusillade's ZDR hooks. Once the decrypt has an edge home, this
//! processor and the dwctl-injects-into-fusillade seam can go away entirely.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fusillade::request::{Claimed, Failed, FailureReason, Request, RequestCompletionResult};
use fusillade::{CancellationFuture, DefaultRequestProcessor, HttpClient, RequestProcessor, ShouldRetry, Storage};

/// Prepares a claimed request for dispatch, then delegates to the default
/// processor. Construct with [`DispatchProcessor::new`] and wire the optional
/// capabilities with [`with_keystore`](Self::with_keystore) /
/// [`with_image_normalizer`](Self::with_image_normalizer).
pub struct DispatchProcessor {
    /// Encrypted key custody for ZDR flex bodies. `None` disables ZDR
    /// decryption (bodies pass through unchanged).
    keystore: Option<crate::keystore::Keystore>,
    /// Resolves `dw-img://` tokens to signed URLs. `None` disables JIT signing.
    image_normalizer: Option<Arc<dyn crate::image_normalizer::ImageNormalizer>>,
    /// TTL for JIT-signed URLs. Sized from the daemon's processing timeout so a
    /// signed URL always outlives one full dispatch attempt.
    dispatch_ttl: Duration,
    /// Paths whose responses are read as a stream and reassembled by the edge,
    /// so the provider reports token usage. Marked here rather than inferred at
    /// the edge: only daemon dispatches reach this processor, which is what
    /// separates them from a client's own request. See [`STREAM_MARKER_KEY`].
    streamable_endpoints: Vec<String>,
    default: DefaultRequestProcessor,
}

impl Default for DispatchProcessor {
    fn default() -> Self {
        Self::new()
    }
}

impl DispatchProcessor {
    pub fn new() -> Self {
        Self {
            keystore: None,
            image_normalizer: None,
            dispatch_ttl: Duration::from_secs(0),
            streamable_endpoints: Vec::new(),
            default: DefaultRequestProcessor,
        }
    }

    /// Paths whose dispatches are marked for streaming and reassembly.
    pub fn with_streamable_endpoints(mut self, endpoints: Vec<String>) -> Self {
        self.streamable_endpoints = endpoints;
        self
    }

    /// Wire in the keystore so ZDR request bodies are decrypted before dispatch.
    pub fn with_keystore(mut self, keystore: Option<crate::keystore::Keystore>) -> Self {
        self.keystore = keystore;
        self
    }

    /// Wire in the normaliser so `dw-img://` tokens are signed before dispatch.
    pub fn with_image_normalizer(mut self, normalizer: Arc<dyn crate::image_normalizer::ImageNormalizer>, dispatch_ttl: Duration) -> Self {
        self.image_normalizer = Some(normalizer);
        self.dispatch_ttl = dispatch_ttl;
        self
    }

    /// Build a terminal `Failed` request with a generic client-facing reason.
    /// The real cause is logged for operators, never surfaced to the caller.
    fn failed(request: Request<Claimed>, reason: FailureReason) -> Request<Failed> {
        Request {
            state: Failed {
                reason,
                failed_at: chrono::Utc::now(),
                retry_attempt: request.state.retry_attempt,
                batch_expires_at: request.state.batch_expires_at,
                routed_model: request.data.model.clone(),
            },
            data: request.data,
        }
    }

    fn zdr_unprocessable() -> FailureReason {
        FailureReason::RequestBuilderError {
            error: "Zero-data-retention request could not be processed".to_string(),
        }
    }

    /// Generic terminal reason for non-ZDR pre-dispatch preparation failures
    /// (JIT signing). Deliberately says nothing about tokens or signing.
    fn dispatch_unprocessable() -> FailureReason {
        FailureReason::RequestBuilderError {
            error: "Request could not be prepared for dispatch".to_string(),
        }
    }
}

#[async_trait]
impl<S, H> RequestProcessor<S, H> for DispatchProcessor
where
    S: Storage + Send + Sync + 'static,
    H: HttpClient + Clone + Send + Sync + 'static,
{
    async fn process(
        &self,
        mut request: Request<Claimed>,
        http: H,
        storage: &S,
        should_retry: ShouldRetry,
        cancellation: CancellationFuture,
    ) -> fusillade::Result<RequestCompletionResult> {
        // ZDR: the stored request body is a self-describing ciphertext envelope.
        // Decrypt it here, before JIT signing and dispatch, so the rest of the
        // flow (and the whole loopback edge) sees plaintext. The response is
        // re-encrypted on its way back by fusillade's ResponseTransformer hook.
        if crate::inference::zdr::is_zdr_body(&request.data.body) {
            let Some(keystore) = self.keystore.as_ref() else {
                // ZDR ciphertext claimed but no keystore configured: the prompt can
                // never be decrypted. Terminalize rather than strand the row.
                crate::background_error!(
                    crate::metrics::errors::component::ZDR_DISPATCH,
                    "keystore_missing",
                    Error,
                    request_id = %request.data.id.0,
                    "ZDR request claimed but keystore is not configured; failing request"
                );
                let failed = Self::failed(request, Self::zdr_unprocessable());
                storage.persist(&failed).await?;
                return Ok(RequestCompletionResult::Failed(failed));
            };
            let key_id = crate::inference::zdr::key_id(&request.data.id.0, crate::inference::zdr::KeyKind::Request);
            match keystore.get(&key_id).await {
                Ok(Some(key)) => {
                    match crate::inference::zdr::decrypt_body(&key, &request.data.body) {
                        Ok(plaintext) => request.data.body = plaintext,
                        Err(e) => {
                            // Ciphertext present but undecryptable (corrupt envelope or
                            // wrong wrap key) - never succeeds on retry.
                            crate::background_error!(
                                crate::metrics::errors::component::ZDR_DISPATCH,
                                "decrypt_failed",
                                Error,
                                request_id = %request.data.id.0,
                                error = %e,
                                "ZDR request body could not be decrypted; failing request"
                            );
                            let failed = Self::failed(request, Self::zdr_unprocessable());
                            storage.persist(&failed).await?;
                            return Ok(RequestCompletionResult::Failed(failed));
                        }
                    }
                    // TRANSITIONAL (dwctl ZDR): mark the dispatch so the loopback
                    // analytics handler blanks the now-plaintext body instead of
                    // logging it. fusillade forwards batch_metadata entries as
                    // `x-fusillade-batch-<key>` headers, so this rides out as
                    // `x-fusillade-batch-zdr: 1`; the outlet handler reads that.
                    // Drop when reassembly moves into dwctl (COR-521).
                    request
                        .data
                        .batch_metadata
                        .insert(crate::inference::zdr::ZDR_MARKER_KEY.to_string(), "1".to_string());
                }
                Ok(None) => {
                    // Key expired/deleted before dispatch: the prompt is gone and this
                    // request can never be processed. Terminalize (persist + Ok(Failed))
                    // so the daemon records it as terminally failed instead of leaving
                    // the row in `processing` until the batch window expires.
                    // Client reason is the same generic one as the other ZDR branches;
                    // the specific cause is logged for operators only.
                    crate::background_error!(
                        crate::metrics::errors::component::ZDR_DISPATCH,
                        "key_expired",
                        Error,
                        request_id = %request.data.id.0,
                        "ZDR request key expired or was deleted before dispatch; cannot decrypt"
                    );
                    let failed = Self::failed(request, Self::zdr_unprocessable());
                    storage.persist(&failed).await?;
                    return Ok(RequestCompletionResult::Failed(failed));
                }
                Err(e) => {
                    // Keystore unreachable (transient - e.g. Redis restart). Return a
                    // retriable failure so the daemon re-pends the row. Do NOT persist:
                    // the daemon's retry path re-pends it itself, and a persisted Failed
                    // would look terminal and lose the retry.
                    crate::background_error!(
                        crate::metrics::errors::component::ZDR_DISPATCH,
                        "keystore_unreachable",
                        Warning,
                        request_id = %request.data.id.0,
                        error = %e,
                        "ZDR keystore unreachable during dispatch; scheduling retry"
                    );
                    let failed = Self::failed(
                        request,
                        FailureReason::NetworkError {
                            error: "Zero-data-retention request could not be processed".to_string(),
                        },
                    );
                    return Ok(RequestCompletionResult::Failed(failed));
                }
            }
        }

        // JIT signing: any `dw-img://{sha256}` token embedded in the body (placed
        // there by the file-ingest path) gets resolved to a fresh short-lived
        // signed URL right before dispatch. The long-lived value at rest is only
        // the opaque token; the signed URL exists only for this attempt's TTL, so
        // retries get fresh URLs and per-attempt leak windows stay bounded.
        // No-op when the normaliser is unset or the body carries no tokens.
        if let Some(normalizer) = self.image_normalizer.clone() {
            let ttl = self.dispatch_ttl;
            // Fail loud if the body isn't parseable JSON. A row containing
            // `dw-img://` tokens by construction always has a JSON body; an
            // unparseable body here means corruption, and dispatching the literal
            // token upstream (which cannot fetch a `dw-img://` URL) would surface
            // as a confusing upstream error far from the root cause.
            // Terminalize rather than returning `Err`: malformed input never
            // succeeds on retry, and a bare `Err` only logs a task failure and
            // strands the row in `processing` (same reasoning as the ZDR branches).
            let mut body_value: serde_json::Value = match serde_json::from_str(&request.data.body) {
                Ok(value) => value,
                Err(e) => {
                    crate::background_error!(
                        crate::metrics::errors::component::ZDR_DISPATCH,
                        "jit_signing_body_not_json",
                        Error,
                        request_id = %request.data.id.0,
                        error = %e,
                        "Request body is not valid JSON; refusing to dispatch with unresolved dw-img tokens"
                    );
                    let failed = Self::failed(request, Self::dispatch_unprocessable());
                    storage.persist(&failed).await?;
                    return Ok(RequestCompletionResult::Failed(failed));
                }
            };
            let result = crate::image_normalizer::walker::substitute_with(
                &mut body_value,
                crate::image_normalizer::Mode::TokensOnly,
                |maybe_token| {
                    let normalizer = Arc::clone(&normalizer);
                    async move {
                        let token: crate::image_normalizer::ImageToken = maybe_token
                            .parse()
                            .map_err(|e: crate::image_normalizer::TokenParseError| format!("invalid dw-img token: {e}"))?;
                        let signed = normalizer.sign(token, ttl).await.map_err(|e| format!("sign failed: {e}"))?;
                        Ok::<String, String>(signed.url)
                    }
                },
            )
            .await;
            match result {
                Ok(count) if count > 0 => match serde_json::to_string(&body_value) {
                    Ok(new_body) => request.data.body = new_body,
                    Err(e) => {
                        // Re-serialising a `Value` that was just parsed does not fail
                        // for input reasons; treat it as terminal rather than
                        // stranding the row on a bare `Err`.
                        crate::background_error!(
                            crate::metrics::errors::component::ZDR_DISPATCH,
                            "jit_signing_reserialise_failed",
                            Error,
                            request_id = %request.data.id.0,
                            error = %e,
                            "Failed to re-serialise request body after JIT signing"
                        );
                        let failed = Self::failed(request, Self::dispatch_unprocessable());
                        storage.persist(&failed).await?;
                        return Ok(RequestCompletionResult::Failed(failed));
                    }
                },
                Ok(_) => {} // no tokens found, leave body alone
                Err(e) => {
                    // Signing is a call to an external signer, so a failure here is
                    // usually TRANSIENT (signer/network blip). Return a retriable
                    // failure without persisting, so the daemon re-pends the row -
                    // same shape as the keystore-unreachable branch above.
                    // Terminalising here would permanently kill batch rows during a
                    // signer outage.
                    crate::background_error!(
                        crate::metrics::errors::component::ZDR_DISPATCH,
                        "jit_signing_failed",
                        Warning,
                        request_id = %request.data.id.0,
                        error = %e,
                        "JIT image-URL signing failed; scheduling retry"
                    );
                    let failed = Self::failed(
                        request,
                        FailureReason::NetworkError {
                            error: "Request could not be prepared for dispatch".to_string(),
                        },
                    );
                    return Ok(RequestCompletionResult::Failed(failed));
                }
            }
        }

        // Mark the dispatch when this path is served as a stream. `batch_metadata`
        // rides out as `x-fusillade-batch-<key>` headers, which is how the ZDR
        // marker already travels, so no new channel is needed.
        //
        // This mark is the ONLY thing that tells daemon traffic from a client's
        // own request at the edge. It cannot be inferred there:
        // `x-fusillade-request-id` is stamped by the edge on everything for
        // correlation, `batch_id` is absent for batchless flex as well as for
        // realtime, and the service tier never leaves the database. A client's
        // request never reaches this processor, so marking here is exactly the
        // signal the edge needs - and without it a streaming client on a
        // configured path has its stream collapsed into a single body.
        if self.streamable_endpoints.iter().any(|e| e == &request.data.path) {
            request
                .data
                .batch_metadata
                .insert(crate::inference::outbound_request::STREAM_MARKER_KEY.to_string(), "1".to_string());
        }

        // Everything dispatches through the default processor: the loopback
        // re-enters the full dwctl edge, which owns translation, id-scrub and the
        // streaming usage flags. No path-based branching here - the multi-step
        // tool loop that used to intercept `/v1/responses` is retired (COR-536).
        self.default.process(request, http, storage, should_retry, cancellation).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The marker is what tells the edge that a response should be reassembled.
    /// Nothing else distinguishes a daemon dispatch from a client's own request
    /// there, so this is the producing half of that contract; the edge's half is
    /// tested in `crate::inference::outbound_request`.
    fn mark(processor: &DispatchProcessor, path: &str) -> Option<String> {
        let mut metadata = std::collections::HashMap::new();
        if processor.streamable_endpoints.iter().any(|e| e == path) {
            metadata.insert(crate::inference::outbound_request::STREAM_MARKER_KEY.to_string(), "1".to_string());
        }
        metadata.get(crate::inference::outbound_request::STREAM_MARKER_KEY).cloned()
    }

    #[test]
    fn a_streamable_path_is_marked() {
        let processor = DispatchProcessor::new().with_streamable_endpoints(vec!["/v1/chat/completions".to_string()]);
        assert_eq!(mark(&processor, "/v1/chat/completions").as_deref(), Some("1"));
    }

    #[test]
    fn a_path_that_is_not_streamable_is_left_unmarked() {
        let processor = DispatchProcessor::new().with_streamable_endpoints(vec!["/v1/chat/completions".to_string()]);
        assert_eq!(mark(&processor, "/v1/embeddings"), None);
    }

    /// With nothing configured the daemon marks nothing, so the edge reassembles
    /// nothing. A misconfiguration costs usage reporting, never a client's stream.
    #[test]
    fn no_configuration_marks_nothing() {
        let processor = DispatchProcessor::new();
        assert_eq!(mark(&processor, "/v1/chat/completions"), None);
    }
}
