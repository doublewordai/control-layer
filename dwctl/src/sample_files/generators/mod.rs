//! Sample file generators.
//!
//! Each generator produces sample requests for a specific use case,
//! with requirements for model type and capabilities.

mod chat;
mod embeddings;
mod vision;

pub use chat::ChatGenerator;
pub use embeddings::EmbeddingsGenerator;
pub use vision::VisionGenerator;

use crate::db::models::deployments::ModelType;
use fusillade::RequestTemplateInput;

/// Trait for sample file generators.
///
/// Each generator produces sample requests for a specific use case.
/// Generators specify their requirements (model type, capabilities) and
/// produce a configurable number of varied, sensible sample requests.
pub trait SampleGenerator: Send + Sync {
    /// Display name for the sample file (e.g., "Sample: Chat Completions").
    fn name(&self) -> &'static str;

    /// Description shown to users.
    fn description(&self) -> &'static str;

    /// Required model type (Chat, Embeddings, or Reranker).
    fn required_model_type(&self) -> ModelType;

    /// Required capabilities (e.g., ["vision"]).
    /// Empty slice means no specific capabilities required.
    fn required_capabilities(&self) -> &'static [&'static str];

    /// Generate sample requests.
    ///
    /// # Arguments
    /// * `model_alias` - The model alias to use in requests
    /// * `api_key` - The API key for request authentication
    /// * `endpoint` - The batch execution endpoint URL
    /// * `count` - Number of requests to generate
    ///
    /// # Returns
    /// A vector of RequestTemplateInput ready for file creation.
    fn generate(&self, model_alias: &str, api_key: &str, endpoint: &str, count: usize) -> Vec<RequestTemplateInput>;
}

/// Get all registered sample generators.
pub fn get_generators() -> Vec<Box<dyn SampleGenerator>> {
    vec![Box::new(ChatGenerator), Box::new(VisionGenerator), Box::new(EmbeddingsGenerator)]
}
