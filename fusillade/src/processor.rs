//! Per-row processing hook for the daemon.
//!
//! The daemon's per-claim processing pipeline today is a fixed sequence: fire
//! the HTTP request, wait for it (or for a cancellation), persist the result.
//! Multi-step Open Responses orchestration (see
//! `docs/plans/2026-04-28-multi-step-responses.md`) needs to inject custom
//! work in this slot — running an agent loop instead of a single fire — but
//! the existing batch path must keep its current behavior byte-for-byte.
//!
//! The [`RequestProcessor`] trait is that hook. The daemon constructs the
//! cross-cutting state (cancellation tokens, span context, metrics handles)
//! and hands the inner two-phase HTTP fire (`Claimed -> Processing -> terminal`)
//! to the processor. [`DefaultRequestProcessor`] is the no-change default that
//! mirrors today's pipeline; downstream consumers (notably dwctl) implement
//! the trait to dispatch on `request.data.endpoint` and run a different
//! pipeline for `/v1/responses` while delegating everything else to the
//! default.
//!
//! ## What the processor owns vs. what the daemon owns
//!
//! The processor owns only the inner HTTP-fire pair. The daemon retains:
//!
//! - per-model and per-user in-flight gauges + counters
//! - the `tokio::select!` between batch-cancel and shutdown tokens (the
//!   resulting `CancellationReason` future is passed in to the processor as
//!   an opaque future)
//! - retry orchestration after a `Failed` outcome (`can_retry`, persist
//!   `Pending`)
//! - daemon-level `outcome` field on the parent span
//!
//! This keeps cross-cutting concerns out of the trait and avoids forcing
//! every implementor to re-replicate them.

use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use tracing::Instrument;

use crate::error::Result;
use crate::http::{HttpClient, HttpResponse};
use crate::manager::Storage;
use crate::request::{Claimed, Request, RequestCompletionResult, transitions::CancellationReason};

/// Boxed future the daemon hands the processor for cooperative cancellation.
///
/// Constructed in the daemon by `select!`-ing the per-batch token and the
/// daemon-wide shutdown token; resolves to the cancellation reason.
pub type CancellationFuture = Pin<Box<dyn std::future::Future<Output = CancellationReason> + Send>>;

/// Predicate the daemon hands the processor to classify upstream HTTP
/// responses as success vs. retriable failure.
pub type ShouldRetry = Arc<dyn Fn(&HttpResponse) -> bool + Send + Sync>;

/// Per-claim processing hook.
///
/// The trait is generic over the storage and HTTP client types the daemon was
/// constructed with so that the existing typestate methods (`request.process`,
/// `processing.complete`) can be used without `dyn` indirection.
///
/// Implementations must drive a `Request<Claimed>` to a terminal state,
/// returning the appropriate [`RequestCompletionResult`]. The daemon will
/// then handle retry persistence and metric emission against that outcome.
#[async_trait]
pub trait RequestProcessor<S, H>: Send + Sync
where
    S: Storage + Sync,
    H: HttpClient + 'static,
{
    /// Process a single claimed request through to a terminal state.
    ///
    /// Implementations should:
    ///
    /// - Move `request` into a `Processing` state by firing the upstream HTTP
    ///   call (typically via `request.process(http, storage)`).
    /// - Wait for the result (or for `cancellation` to resolve), classifying
    ///   non-2xx responses with `should_retry` to decide whether to retry.
    /// - Return the typed terminal request.
    ///
    /// The default impl ([`DefaultRequestProcessor`]) is the canonical
    /// reference for this contract.
    async fn process(
        &self,
        request: Request<Claimed>,
        http: H,
        storage: &S,
        should_retry: ShouldRetry,
        cancellation: CancellationFuture,
    ) -> Result<RequestCompletionResult>;
}

/// Default processor that preserves today's daemon behavior exactly.
///
/// Wraps the existing two-phase typestate pipeline:
///
/// ```ignore
/// let processing = request.process(http, storage).await?;
/// processing.complete(storage, should_retry, cancellation).await
/// ```
///
/// inside the same `fusillade.state.claimed` and `fusillade.state.processing`
/// spans the daemon used to emit inline. Any consumer that does not provide
/// its own processor gets this for free, so the existing batch path is
/// unchanged.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultRequestProcessor;

#[async_trait]
impl<S, H> RequestProcessor<S, H> for DefaultRequestProcessor
where
    S: Storage + Sync,
    H: HttpClient + 'static,
{
    async fn process(
        &self,
        request: Request<Claimed>,
        http: H,
        storage: &S,
        should_retry: ShouldRetry,
        cancellation: CancellationFuture,
    ) -> Result<RequestCompletionResult> {
        let request_id = request.data.id;
        let daemon_id = request.state.daemon_id;
        let retry_attempt = request.state.retry_attempt;

        // Span 1: claimed -> processing (HTTP request kicked off)
        let processing = async {
            tracing::debug!("Sending batch request to inference endpoint");
            let request_data = request.data.clone();
            let api_key = request_data.api_key.clone();
            let response_fut = async move { http.execute(&request_data, &api_key).await };
            request.process(storage, response_fut).await
        }
        .instrument(tracing::info_span!(
            "fusillade.state.claimed",
            otel.name = "fusillade.state.claimed",
            request_id = %request_id,
            daemon_id = %daemon_id,
            retry_attempt,
        ))
        .await?;

        // Capture for the second span — Completed state drops these fields.
        let retry_attempt_at_completion = processing.state.retry_attempt;

        // Span 2: processing -> terminal (HTTP in-flight, awaiting result or
        // cancellation)
        async {
            processing
                .complete(storage, |response| (should_retry)(response), cancellation)
                .await
        }
        .instrument(tracing::info_span!(
            "fusillade.state.processing",
            otel.name = "fusillade.state.processing",
            request_id = %request_id,
            retry_attempt = retry_attempt_at_completion,
        ))
        .await
    }
}
