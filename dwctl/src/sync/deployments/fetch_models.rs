//! Model fetching from external sources.

use crate::api::models::inference_endpoints::{AnthropicModelsResponse, OpenAIModelsResponse};
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
        Self {
            client,
            base_url,
            openai_api_key,
            auth_header_name,
            auth_header_prefix,
            request_timeout,
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
}

impl From<&Url> for ModelFormat {
    fn from(value: &Url) -> Self {
        if value.as_str().starts_with("https://api.anthropic.com") {
            return Self::Anthropic;
        }
        Self::OpenAI
    }
}

#[async_trait]
impl FetchModels for FetchModelsReqwest {
    async fn fetch(&self) -> anyhow::Result<OpenAIModelsResponse> {
        debug!("Base URL for fetching models: {}", self.base_url);
        let fmt = (&self.base_url).into();
        debug!("Featching models in format: {:?}", fmt);

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
                    request = request.header(&self.auth_header_name, format!("{}{}", self.auth_header_prefix, api_key));
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
        }
    }
}

/// A static implementation of FetchModels that returns a predefined list of models
/// Used for endpoints where we have a known list of models (e.g., Snowflake Cortex AI)
#[allow(dead_code)]
pub struct StaticModelsFetcher {
    models: OpenAIModelsResponse,
}

#[allow(dead_code)]
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
