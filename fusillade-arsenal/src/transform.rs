//! Persistence-time response transformation hook.

use async_trait::async_trait;
use fusillade_core::{RequestData, Result};

#[async_trait]
pub trait ResponseTransformer: Send + Sync {
    async fn transform(&self, request: &RequestData, body: &str) -> Result<String>;
}
