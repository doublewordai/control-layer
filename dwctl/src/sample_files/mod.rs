//! Sample file generation for new users.
//!
//! This module provides functionality to generate sample JSONL batch files for new users
//! during account creation. Sample files are dynamically generated based on the user's
//! accessible models and their capabilities.

pub mod generators;

use crate::db::models::deployments::DeploymentDBResponse;
use crate::errors::Result;
use fusillade::{FileId, Storage};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub use generators::{SampleGenerator, get_generators};

/// Configuration for sample file generation.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SampleFilesConfig {
    /// Whether sample file generation is enabled.
    pub enabled: bool,
    /// Number of requests to generate per sample file.
    pub requests_per_file: usize,
}

impl Default for SampleFilesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            requests_per_file: 2000,
        }
    }
}

/// Find a model matching the generator's requirements from user's accessible deployments.
///
/// Returns the model alias if a matching deployment is found, or None if no suitable model exists.
pub fn find_matching_model(deployments: &[DeploymentDBResponse], generator: &dyn SampleGenerator) -> Option<String> {
    deployments
        .iter()
        .find(|d| {
            // Check model type matches
            let type_matches = d.model_type.as_ref() == Some(&generator.required_model_type());

            // Check all required capabilities are present
            let required_caps = generator.required_capabilities();
            let caps_match = required_caps.iter().all(|req_cap| {
                d.capabilities
                    .as_ref()
                    .map(|caps| caps.iter().any(|c| c == *req_cap))
                    .unwrap_or(false)
            });

            // Empty capabilities requirement always matches
            let caps_ok = required_caps.is_empty() || caps_match;

            type_matches && caps_ok
        })
        .map(|d| d.alias.clone())
}

/// Create sample files for a user based on their accessible models.
///
/// This function iterates through all registered sample generators and creates
/// a sample file for each one where the user has access to a matching model.
/// Files are created via the fusillade storage layer with proper ownership metadata.
///
/// # Arguments
/// * `storage` - The fusillade storage implementation
/// * `user_id` - The user's UUID
/// * `api_key` - The user's batch API key for request execution
/// * `endpoint` - The batch execution endpoint URL
/// * `accessible_deployments` - List of deployments the user has access to
/// * `config` - Sample files configuration
///
/// # Returns
/// A vector of created file IDs, or an error if file creation fails.
pub async fn create_sample_files_for_user<S: Storage>(
    storage: &S,
    user_id: Uuid,
    api_key: &str,
    endpoint: &str,
    accessible_deployments: &[DeploymentDBResponse],
    config: &SampleFilesConfig,
) -> Result<Vec<FileId>> {
    use fusillade::{FileMetadata, FileStreamItem};
    use futures::stream;

    let mut created_files = Vec::new();

    for generator in get_generators() {
        // Find a matching model for this generator
        let Some(model_alias) = find_matching_model(accessible_deployments, generator.as_ref()) else {
            tracing::debug!(
                generator = generator.name(),
                user_id = %user_id,
                "Skipping sample file - no matching model"
            );
            continue;
        };

        // Generate requests using the generator
        let templates = generator.generate(&model_alias, api_key, endpoint, config.requests_per_file);

        // Calculate file size (each template serialized as JSON + newline)
        let size_bytes: i64 = templates
            .iter()
            .map(|t| {
                serde_json::to_string(t)
                    .map(|s| s.len() as i64 + 1) // +1 for newline
                    .unwrap_or(0)
            })
            .sum();

        // Build stream items with metadata including purpose, uploaded_by, and size
        let mut items = vec![FileStreamItem::Metadata(FileMetadata {
            filename: Some(generator.name().to_string()),
            description: Some(generator.description().to_string()),
            purpose: Some("batch".to_string()),
            uploaded_by: Some(user_id.to_string()),
            size_bytes: Some(size_bytes),
            ..Default::default()
        })];

        for template in templates {
            items.push(FileStreamItem::Template(template));
        }

        // Create file via fusillade with streaming to include metadata
        let file_id = storage
            .create_file_stream(stream::iter(items))
            .await
            .map_err(|e| crate::errors::Error::Internal {
                operation: format!("create sample file '{}': {}", generator.name(), e),
            })?;

        tracing::info!(
            generator = generator.name(),
            file_id = %file_id,
            user_id = %user_id,
            request_count = config.requests_per_file,
            "Created sample file for user"
        );

        created_files.push(file_id);
    }

    Ok(created_files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::deployments::ModelType;

    fn create_test_deployment(alias: &str, model_type: ModelType, capabilities: Option<Vec<String>>) -> DeploymentDBResponse {
        DeploymentDBResponse {
            id: uuid::Uuid::new_v4(),
            model_name: alias.to_string(),
            alias: alias.to_string(),
            description: None,
            model_type: Some(model_type),
            capabilities,
            hosted_on: Some(uuid::Uuid::new_v4()),
            requests_per_second: None,
            burst_size: None,
            capacity: None,
            batch_capacity: None,
            status: crate::db::models::deployments::ModelStatus::Active,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            created_by: uuid::Uuid::new_v4(),
            deleted: false,
            last_sync: None,
            provider_pricing: None,
            // Composite model fields (regular model = not composite)
            is_composite: false,
            lb_strategy: crate::db::models::deployments::LoadBalancingStrategy::default(),
            fallback_enabled: true,
            fallback_on_rate_limit: true,
            fallback_on_status: vec![429, 500, 502, 503, 504],
        }
    }

    #[test]
    fn test_find_matching_model_chat() {
        let deployments = vec![
            create_test_deployment("gpt-4", ModelType::Chat, None),
            create_test_deployment("text-embedding-ada", ModelType::Embeddings, None),
        ];

        let chat_generator = generators::ChatGenerator;
        let result = find_matching_model(&deployments, &chat_generator);
        assert_eq!(result, Some("gpt-4".to_string()));
    }

    #[test]
    fn test_find_matching_model_embeddings() {
        let deployments = vec![
            create_test_deployment("gpt-4", ModelType::Chat, None),
            create_test_deployment("text-embedding-ada", ModelType::Embeddings, None),
        ];

        let embeddings_generator = generators::EmbeddingsGenerator;
        let result = find_matching_model(&deployments, &embeddings_generator);
        assert_eq!(result, Some("text-embedding-ada".to_string()));
    }

    #[test]
    fn test_find_matching_model_vision_requires_capability() {
        let deployments = vec![
            create_test_deployment("gpt-4", ModelType::Chat, None),
            create_test_deployment("gpt-4-vision", ModelType::Chat, Some(vec!["vision".to_string()])),
        ];

        let vision_generator = generators::VisionGenerator;
        let result = find_matching_model(&deployments, &vision_generator);
        assert_eq!(result, Some("gpt-4-vision".to_string()));
    }

    #[test]
    fn test_find_matching_model_no_match() {
        let deployments = vec![create_test_deployment("gpt-4", ModelType::Chat, None)];

        let embeddings_generator = generators::EmbeddingsGenerator;
        let result = find_matching_model(&deployments, &embeddings_generator);
        assert_eq!(result, None);
    }

    #[test]
    fn test_find_matching_model_vision_without_capability_no_match() {
        let deployments = vec![create_test_deployment("gpt-4", ModelType::Chat, None)];

        let vision_generator = generators::VisionGenerator;
        let result = find_matching_model(&deployments, &vision_generator);
        assert_eq!(result, None);
    }
}
