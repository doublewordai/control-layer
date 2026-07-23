//! Persistence-time response transformation hook.

use async_trait::async_trait;
use fusillade_core::{RequestData, Result};

#[async_trait]
pub trait ResponseTransformer: Send + Sync {
    /// Prepare a response body for persistence.
    ///
    /// This hook runs before the storage compare-and-set and may be reused
    /// across database retries. Implementations must not perform destructive
    /// side effects here. Implementations must use
    /// [`fusillade_core::FusilladeError::AttemptPersistenceInfrastructure`]
    /// only for transient dependency outages; deterministic transformation or
    /// validation failures must remain definitive.
    async fn transform(&self, request: &RequestData, body: &str) -> Result<String>;

    /// Best-effort notification after a terminal state was durably applied.
    ///
    /// Storage invokes this only when the exact terminal transition won its
    /// compare-and-set. Failures are logged and left to the implementation's
    /// retention backstop rather than undoing the durable terminal state.
    async fn after_terminal_persisted(&self, _request: &RequestData) -> Result<()> {
        Ok(())
    }
}
