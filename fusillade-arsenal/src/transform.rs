//! Persistence-time response transformation hook.

use async_trait::async_trait;
use fusillade_core::{RequestData, Result};

#[async_trait]
pub trait ResponseTransformer: Send + Sync {
    /// Transform an outcome body before persistence.
    ///
    /// Implementations must use
    /// [`fusillade_core::FusilladeError::AttemptPersistenceInfrastructure`]
    /// only for transient dependency outages. Deterministic transformation or
    /// validation failures must remain definitive.
    async fn transform(&self, request: &RequestData, body: &str) -> Result<String>;
}
