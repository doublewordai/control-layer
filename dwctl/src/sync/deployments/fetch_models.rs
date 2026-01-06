//! Model fetching from external sources.

use crate::api::models::inference_endpoints::{AnthropicModelsResponse, OpenAIModelsResponse, OpenRouterModelsResponse};
use crate::db::models::inference_endpoints::InferenceEndpointDBResponse;
use anyhow::anyhow;
use async_trait::async_trait;
use reqwest::Client;
use std::time::Duration;
use tracing::{debug, instrument};
use url::Url;

#[derive(Debug, Clone)]
pub struct SyncConfig {
    pub openai_api_key: Option<String>,
    pub openai_base_url: Url,
    pub auth_header_name: String,
    pub auth_header_prefix: String,
    pub(crate) request_timeout: Duration,
    /// Override format detection (primarily for testing)
    pub format_override: Option<ModelFormat>,
}

impl SyncConfig {
    /// Default timeout for API requests
    const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

    /// Create a SyncConfig from an endpoint DB response
    #[instrument]
    pub fn from_endpoint(source: &InferenceEndpointDBResponse) -> Self {
        Self {
            openai_api_key: source.api_key.clone(),
            openai_base_url: source.url.clone(),
            auth_header_name: source.auth_header_name.clone(),
            auth_header_prefix: source.auth_header_prefix.clone(),
            request_timeout: Self::DEFAULT_REQUEST_TIMEOUT,
            format_override: None, // Use automatic detection
        }
    }
}

/// A trait for fetching models in openai compatible format.
/// In practise, this is used for fetching models over http from downstream openai compatible
/// endpoints, using the `reqwest` library. See `FetchModelsReqwest` for more info.
#[async_trait]
pub trait FetchModels {
    async fn fetch(&self) -> anyhow::Result<OpenAIModelsResponse>;
}

/// The concrete implementation of `FetchModels`.
pub struct FetchModelsReqwest {
    client: Client,
    base_url: Url,
    openai_api_key: Option<String>,
    auth_header_name: String,
    auth_header_prefix: String,
    request_timeout: Duration,
    format_override: Option<ModelFormat>,
}

impl FetchModelsReqwest {
    pub fn new(config: SyncConfig) -> Self {
        let client = Client::builder()
            .timeout(config.request_timeout)
            .build()
            .expect("Failed to create HTTP client");
        let base_url = config.openai_base_url.clone();
        let openai_api_key = config.openai_api_key.clone();
        let auth_header_name = config.auth_header_name.clone();
        let auth_header_prefix = config.auth_header_prefix.clone();
        let request_timeout = config.request_timeout;
        let format_override = config.format_override.clone();
        Self {
            client,
            base_url,
            openai_api_key,
            auth_header_name,
            auth_header_prefix,
            request_timeout,
            format_override,
        }
    }
}

/// Makes sure a url has a trailing slash.
///
/// This fixes a weird idiosyncracy in rusts 'join' method on urls, where joining URLs like
/// '/hello', 'world' gives you '/world', but '/hello/', 'world' gives you '/hello/world'.
/// Basically, call this before calling .join
fn ensure_slash(url: &Url) -> Url {
    if url.path().ends_with('/') {
        url.clone()
    } else {
        let mut new_url = url.clone();
        let mut path = new_url.path().to_string();
        path.push('/');
        new_url.set_path(&path);
        new_url
    }
}

#[derive(Debug, Clone)]
pub enum ModelFormat {
    OpenAI,
    Anthropic,
    OpenRouter,
}

impl From<&Url> for ModelFormat {
    fn from(value: &Url) -> Self {
        let url_str = value.as_str();
        if url_str.starts_with("https://api.anthropic.com") {
            return Self::Anthropic;
        }
        if url_str.starts_with("https://openrouter.ai") {
            return Self::OpenRouter;
        }
        Self::OpenAI
    }
}

#[async_trait]
impl FetchModels for FetchModelsReqwest {
    async fn fetch(&self) -> anyhow::Result<OpenAIModelsResponse> {
        debug!("Base URL for fetching models: {}", self.base_url);
        let fmt = self.format_override.clone().unwrap_or_else(|| (&self.base_url).into());
        debug!("Fetching models in format: {:?}", fmt);

        let url = ensure_slash(&self.base_url)
            .join("models")
            .map_err(|e| anyhow::anyhow!("Failed to construct models URL: {}", e))?;

        debug!("Fetching models from URL: {}", url);

        let mut request = self.client.get(url.clone());

        match fmt {
            ModelFormat::OpenAI => {
                if let Some(api_key) = &self.openai_api_key {
                    request = request.header(&self.auth_header_name, format!("{}{}", self.auth_header_prefix, api_key));
                };

                let response = request.timeout(self.request_timeout).send().await?;

                if !response.status().is_success() {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    tracing::error!("Failed to make request to openAI API for models");
                    tracing::error!("Url was: {}", url);
                    return Err(anyhow!("OpenAI API error: {} - {}", status, body));
                }

                // Get the response body as text first for logging
                let body_text = response.text().await?;
                tracing::debug!("Models API response body: {}", body_text);

                // Try to parse the JSON
                match serde_json::from_str::<OpenAIModelsResponse>(&body_text) {
                    Ok(parsed) => Ok(parsed),
                    Err(e) => {
                        tracing::error!("Failed to make request to openAI-compatible API for models");
                        tracing::error!("Failed to parse models response as JSON. Error: {}", e);
                        tracing::error!("Response body was: {}", body_text);
                        Err(anyhow!("error decoding response body: {}", e))
                    }
                }
            }
            ModelFormat::Anthropic => {
                if let Some(api_key) = &self.openai_api_key {
                    request = request.header("X-APi-Key", api_key.to_string());
                };

                // Have to set this
                request = request.header("anthropic-version", "2023-06-01");

                let response = request.timeout(self.request_timeout).send().await?;

                if !response.status().is_success() {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    tracing::error!("Failed to make request to anthropic API for models");
                    tracing::error!("Url was: {}", url);
                    return Err(anyhow!("Anthropic API error {}: {}", status, body));
                }

                // Get the response body as text first for logging
                let body_text = response.text().await?;
                tracing::debug!("Models API response body: {}", body_text);

                // Try to parse the JSON
                match serde_json::from_str::<AnthropicModelsResponse>(&body_text) {
                    Ok(parsed) => Ok(parsed.into()),
                    Err(e) => {
                        tracing::error!("Failed to make request to anthropic API for models");
                        tracing::error!("Url was: {}", url);
                        tracing::error!("Failed to parse models response as JSON. Error: {}", e);
                        tracing::error!("Response body was: {}", body_text);
                        Err(anyhow!("error decoding response body: {}", e))
                    }
                }
            }
            ModelFormat::OpenRouter => {
                if let Some(api_key) = &self.openai_api_key {
                    request = request.header(&self.auth_header_name, format!("{}{}", self.auth_header_prefix, api_key));
                };

                let response = request.timeout(self.request_timeout).send().await?;

                if !response.status().is_success() {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    tracing::error!("Failed to make request to OpenRouter API for models");
                    tracing::error!("Url was: {}", url);
                    return Err(anyhow!("OpenRouter API error: {} - {}", status, body));
                }

                // Get the response body as text first for logging
                let body_text = response.text().await?;
                tracing::debug!("Models API response body: {}", body_text);

                // Try to parse the JSON
                match serde_json::from_str::<OpenRouterModelsResponse>(&body_text) {
                    Ok(parsed) => Ok(parsed.into()),
                    Err(e) => {
                        tracing::error!("Failed to make request to OpenRouter API for models");
                        tracing::error!("Url was: {}", url);
                        tracing::error!("Failed to parse models response as JSON. Error: {}", e);
                        tracing::error!("Response body was: {}", body_text);
                        Err(anyhow!("error decoding response body: {}", e))
                    }
                }
            }
        }
    }
}

/// A static implementation of FetchModels that returns a predefined list of models
/// Used for endpoints where we have a known list of models (e.g., Snowflake Cortex AI)
pub struct StaticModelsFetcher {
    models: OpenAIModelsResponse,
}

impl StaticModelsFetcher {
    pub fn new(model_names: Vec<String>) -> Self {
        let models = model_names
            .into_iter()
            .map(|name| crate::api::models::inference_endpoints::OpenAIModel {
                id: name,
                object: "model".to_string(),
                created: Some(0),
                owned_by: String::new(), // Empty string for static models
            })
            .collect();

        Self {
            models: OpenAIModelsResponse {
                object: "list".to_string(),
                data: models,
            },
        }
    }
}

#[async_trait]
impl FetchModels for StaticModelsFetcher {
    async fn fetch(&self) -> anyhow::Result<OpenAIModelsResponse> {
        debug!("Returning static model list with {} models", self.models.data.len());
        Ok(self.models.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn test_fetch_openai_format_with_api_key() {
        let mock_server = MockServer::start().await;

        // Set up mock response
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("Authorization", "Bearer test-api-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [
                    {
                        "id": "gpt-4",
                        "object": "model",
                        "created": 1234567890,
                        "owned_by": "openai"
                    },
                    {
                        "id": "gpt-3.5-turbo",
                        "object": "model",
                        "created": 1234567891,
                        "owned_by": "openai"
                    }
                ]
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        let config = SyncConfig {
            openai_api_key: Some("test-api-key".to_string()),
            openai_base_url: mock_server.uri().parse().unwrap(),
            auth_header_name: "Authorization".to_string(),
            auth_header_prefix: "Bearer ".to_string(),
            request_timeout: Duration::from_secs(30),
            format_override: None,
        };

        let fetcher = FetchModelsReqwest::new(config);
        let result = fetcher.fetch().await.unwrap();

        assert_eq!(result.object, "list");
        assert_eq!(result.data.len(), 2);
        assert_eq!(result.data[0].id, "gpt-4");
        assert_eq!(result.data[1].id, "gpt-3.5-turbo");
    }

    #[tokio::test]
    async fn test_fetch_openai_format_without_api_key() {
        let mock_server = MockServer::start().await;

        // Set up mock response - should NOT have Authorization header
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [
                    {
                        "id": "local-model",
                        "object": "model",
                        "created": 1234567890,
                        "owned_by": "local"
                    }
                ]
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        let config = SyncConfig {
            openai_api_key: None,
            openai_base_url: mock_server.uri().parse().unwrap(),
            auth_header_name: "Authorization".to_string(),
            auth_header_prefix: "Bearer ".to_string(),
            request_timeout: Duration::from_secs(30),
            format_override: None,
        };

        let fetcher = FetchModelsReqwest::new(config);
        let result = fetcher.fetch().await.unwrap();

        assert_eq!(result.object, "list");
        assert_eq!(result.data.len(), 1);
        assert_eq!(result.data[0].id, "local-model");
    }

    #[tokio::test]
    async fn test_fetch_openai_format_custom_auth_headers() {
        let mock_server = MockServer::start().await;

        // Set up mock response with custom auth header
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("X-API-Key", "sk-custom-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [
                    {
                        "id": "custom-model",
                        "object": "model",
                        "created": 1234567890,
                        "owned_by": "custom-provider"
                    }
                ]
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        let config = SyncConfig {
            openai_api_key: Some("custom-key".to_string()),
            openai_base_url: mock_server.uri().parse().unwrap(),
            auth_header_name: "X-API-Key".to_string(),
            auth_header_prefix: "sk-".to_string(),
            request_timeout: Duration::from_secs(30),
            format_override: None,
        };

        let fetcher = FetchModelsReqwest::new(config);
        let result = fetcher.fetch().await.unwrap();

        assert_eq!(result.object, "list");
        assert_eq!(result.data.len(), 1);
        assert_eq!(result.data[0].id, "custom-model");
    }

    #[tokio::test]
    async fn test_fetch_anthropic_format() {
        let mock_server = MockServer::start().await;

        // Set up mock response - Anthropic uses different header names
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("X-APi-Key", "anthropic-key")) // Note: Typo in original code - X-APi-Key
            .and(header("anthropic-version", "2023-06-01"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {
                        "id": "claude-3-5-sonnet-20241022",
                        "display_name": "Claude 3.5 Sonnet",
                        "type": "model",
                        "created_at": "2024-10-22T00:00:00Z"
                    }
                ],
                "first_id": "claude-3-5-sonnet-20241022",
                "has_more": false,
                "last_id": "claude-3-5-sonnet-20241022"
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        // Use format_override to force Anthropic format with mock server URL
        let config = SyncConfig {
            openai_api_key: Some("anthropic-key".to_string()),
            openai_base_url: mock_server.uri().parse().unwrap(),
            auth_header_name: "Authorization".to_string(), // This will be ignored for Anthropic
            auth_header_prefix: "Bearer ".to_string(),
            request_timeout: Duration::from_secs(30),
            format_override: Some(ModelFormat::Anthropic),
        };

        let fetcher = FetchModelsReqwest::new(config);
        let result = fetcher.fetch().await.unwrap();

        // Anthropic response gets converted to OpenAI format
        assert_eq!(result.object, "list");
        assert_eq!(result.data.len(), 1);
        assert_eq!(result.data[0].id, "claude-3-5-sonnet-20241022");
    }

    #[tokio::test]
    async fn test_fetch_error_non_success_status() {
        let mock_server = MockServer::start().await;

        // Set up mock to return 404
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(404).set_body_string("Not found"))
            .expect(1)
            .mount(&mock_server)
            .await;

        let config = SyncConfig {
            openai_api_key: Some("test-key".to_string()),
            openai_base_url: mock_server.uri().parse().unwrap(),
            auth_header_name: "Authorization".to_string(),
            auth_header_prefix: "Bearer ".to_string(),
            request_timeout: Duration::from_secs(30),
            format_override: None,
        };

        let fetcher = FetchModelsReqwest::new(config);
        let result = fetcher.fetch().await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn test_fetch_error_invalid_json() {
        let mock_server = MockServer::start().await;

        // Set up mock to return invalid JSON
        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not valid json"))
            .expect(1)
            .mount(&mock_server)
            .await;

        let config = SyncConfig {
            openai_api_key: Some("test-key".to_string()),
            openai_base_url: mock_server.uri().parse().unwrap(),
            auth_header_name: "Authorization".to_string(),
            auth_header_prefix: "Bearer ".to_string(),
            request_timeout: Duration::from_secs(30),
            format_override: None,
        };

        let fetcher = FetchModelsReqwest::new(config);
        let result = fetcher.fetch().await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("error decoding response body"));
    }

    #[tokio::test]
    async fn test_fetch_url_joining_without_trailing_slash() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": []
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        // URL without trailing slash
        let base_url = format!("{}/v1", mock_server.uri());
        let config = SyncConfig {
            openai_api_key: None,
            openai_base_url: base_url.parse().unwrap(),
            auth_header_name: "Authorization".to_string(),
            auth_header_prefix: "Bearer ".to_string(),
            request_timeout: Duration::from_secs(30),
            format_override: None,
        };

        let fetcher = FetchModelsReqwest::new(config);
        let result = fetcher.fetch().await.unwrap();

        assert_eq!(result.object, "list");
        assert_eq!(result.data.len(), 0);
    }

    #[tokio::test]
    async fn test_fetch_url_joining_with_trailing_slash() {
        let mock_server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": []
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        // URL with trailing slash
        let base_url = format!("{}/v1/", mock_server.uri());
        let config = SyncConfig {
            openai_api_key: None,
            openai_base_url: base_url.parse().unwrap(),
            auth_header_name: "Authorization".to_string(),
            auth_header_prefix: "Bearer ".to_string(),
            request_timeout: Duration::from_secs(30),
            format_override: None,
        };

        let fetcher = FetchModelsReqwest::new(config);
        let result = fetcher.fetch().await.unwrap();

        assert_eq!(result.object, "list");
        assert_eq!(result.data.len(), 0);
    }

    #[test]
    fn test_ensure_slash() {
        let url_without = Url::parse("http://example.com/api").unwrap();
        let url_with_slash = ensure_slash(&url_without);
        assert_eq!(url_with_slash.path(), "/api/");

        // Should be idempotent
        let url_already_with_slash = Url::parse("http://example.com/api/").unwrap();
        let url_still_with_slash = ensure_slash(&url_already_with_slash);
        assert_eq!(url_still_with_slash.path(), "/api/");
    }

    #[test]
    fn test_model_format_detection_openai() {
        let url = Url::parse("https://api.openai.com/v1/").unwrap();
        let format: ModelFormat = (&url).into();
        assert!(matches!(format, ModelFormat::OpenAI));
    }

    #[test]
    fn test_model_format_detection_anthropic() {
        let url = Url::parse("https://api.anthropic.com/v1/").unwrap();
        let format: ModelFormat = (&url).into();
        assert!(matches!(format, ModelFormat::Anthropic));
    }

    #[test]
    fn test_model_format_detection_other() {
        let url = Url::parse("https://some-other-provider.com/v1/").unwrap();
        let format: ModelFormat = (&url).into();
        assert!(matches!(format, ModelFormat::OpenAI));
    }

    #[test]
    fn test_model_format_detection_openrouter() {
        let url = Url::parse("https://openrouter.ai/api/v1/").unwrap();
        let format: ModelFormat = (&url).into();
        assert!(matches!(format, ModelFormat::OpenRouter));
    }

    #[tokio::test]
    async fn test_fetch_openrouter_format() {
        let mock_server = MockServer::start().await;

        // Set up mock response - OpenRouter format
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("Authorization", "Bearer test-api-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {
                        "id": "openai/gpt-4-turbo",
                        "name": "GPT-4 Turbo",
                        "created": 1234567890,
                        "description": "Latest GPT-4 Turbo model"
                    },
                    {
                        "id": "anthropic/claude-3-opus",
                        "name": "Claude 3 Opus",
                        "created": 1234567891,
                        "description": "Most capable Claude model"
                    }
                ]
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        // Use format_override to force OpenRouter format with mock server URL
        let config = SyncConfig {
            openai_api_key: Some("test-api-key".to_string()),
            openai_base_url: mock_server.uri().parse().unwrap(),
            auth_header_name: "Authorization".to_string(),
            auth_header_prefix: "Bearer ".to_string(),
            request_timeout: Duration::from_secs(30),
            format_override: Some(ModelFormat::OpenRouter),
        };

        let fetcher = FetchModelsReqwest::new(config);
        let result = fetcher.fetch().await.unwrap();

        // OpenRouter response gets converted to OpenAI format
        assert_eq!(result.object, "list");
        assert_eq!(result.data.len(), 2);
        assert_eq!(result.data[0].id, "openai/gpt-4-turbo");
        assert_eq!(result.data[1].id, "anthropic/claude-3-opus");
        assert_eq!(result.data[0].object, "model");
        assert_eq!(result.data[0].owned_by, "openrouter");
    }

    #[tokio::test]
    async fn test_fetch_openrouter_format_minimal_fields() {
        let mock_server = MockServer::start().await;

        // Set up mock response with minimal fields (only required fields)
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("Authorization", "Bearer test-api-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {
                        "id": "minimal/model"
                    }
                ]
            })))
            .expect(1)
            .mount(&mock_server)
            .await;

        let config = SyncConfig {
            openai_api_key: Some("test-api-key".to_string()),
            openai_base_url: mock_server.uri().parse().unwrap(),
            auth_header_name: "Authorization".to_string(),
            auth_header_prefix: "Bearer ".to_string(),
            request_timeout: Duration::from_secs(30),
            format_override: Some(ModelFormat::OpenRouter),
        };

        let fetcher = FetchModelsReqwest::new(config);
        let result = fetcher.fetch().await.unwrap();

        assert_eq!(result.object, "list");
        assert_eq!(result.data.len(), 1);
        assert_eq!(result.data[0].id, "minimal/model");
        assert_eq!(result.data[0].object, "model");
    }

    #[tokio::test]
    async fn test_static_models_fetcher() {
        let model_names = vec!["snowflake/arctic-embed-m".to_string(), "snowflake/mistral-large2".to_string()];

        let fetcher = StaticModelsFetcher::new(model_names.clone());
        let result = fetcher.fetch().await.unwrap();

        assert_eq!(result.object, "list");
        assert_eq!(result.data.len(), 2);
        assert_eq!(result.data[0].id, "snowflake/arctic-embed-m");
        assert_eq!(result.data[1].id, "snowflake/mistral-large2");
        assert_eq!(result.data[0].object, "model");
    }
}
