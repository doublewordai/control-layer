//! TRANSITIONAL ZDR hook: transform a response/error body just before it is
//! persisted.
//!
//! This only exists because fusillade currently reassembles the upstream SSE
//! stream and writes the whole body itself, so the dwctl proxy layer never sees
//! the assembled body and has no other point at which to encrypt it for
//! zero-data-retention. dwctl installs a transformer that encrypts ZDR bodies at
//! rest.
//!
//! REMOVE ME once stream reassembly moves into dwctl: at that point dwctl owns
//! the assembled body directly and encrypts it there, with no fusillade hook.
//! The whole hook is this module plus the `response_transformer` field,
//! `set_response_transformer`, and the apply point in `persist` (the daemon
//! completion path). ZDR flex requests always complete through `persist`; the
//! realtime `complete_request`/`fail_request` methods are only reached by the
//! server-side-tool loop, which ZDR rejects at submit, so the hook lives only
//! in `persist`. Grep `ResponseTransformer` to find all of it.

use async_trait::async_trait;

use crate::{RequestData, Result};

/// Prepares a response or error body for persistence and optionally performs
/// best-effort cleanup after a terminal write commits. With no transformer
/// installed (the default) the behaviour is identity.
#[async_trait]
pub trait ResponseTransformer: Send + Sync {
    /// Return the body to persist for `request`. Implementations must return the
    /// input unchanged when they do not apply to this request.
    ///
    /// `request` is the request being persisted - its id and `batch_metadata`
    /// let an implementation decide whether (and how) to transform without a
    /// separate lookup. `body` is the terminal response/error body to persist.
    ///
    /// This method runs before the storage compare-and-set and must not perform
    /// destructive side effects.
    ///
    /// Return [`crate::FusilladeError::AttemptPersistenceInfrastructure`] only
    /// when retrying the same transformation can recover from a transient
    /// dependency outage. Deterministic encryption, validation, or preparation
    /// failures must use another error variant so daemon durability does not
    /// retry them indefinitely.
    async fn transform(&self, request: &RequestData, body: &str) -> Result<String>;

    /// Best-effort notification after the exact terminal transition was
    /// durably applied. The default has no cleanup to perform.
    async fn after_terminal_persisted(&self, _request: &RequestData) -> Result<()> {
        Ok(())
    }
}
