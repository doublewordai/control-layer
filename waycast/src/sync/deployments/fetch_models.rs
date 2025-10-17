use crate::api::models::inference_endpoints::OpenAIModelsResponse;
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
        let request_timeout = config.request_timeout;
        Self {
            client,
            base_url,
            openai_api_key,
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

#[async_trait]
impl FetchModels for FetchModelsReqwest {
    async fn fetch(&self) -> anyhow::Result<OpenAIModelsResponse> {
        let url = ensure_slash(&self.base_url)
            .join("v1/models")
            .map_err(|e| anyhow::anyhow!("Failed to construct models URL: {}", e))?;

        debug!("Fetching models from URL: {}", url);

        let mut request = self.client.get(url);

        if let Some(api_key) = &self.openai_api_key {
            request = request.header("Authorization", format!("Bearer {api_key}"));
        };

        let response = request.timeout(self.request_timeout).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("OpenAI API error: {} - {}", status, body));
        }

        // Get the response body as text first for logging
        let body_text = response.text().await?;
        tracing::debug!("Models API response body: {}", body_text);

        // Try to parse the JSON
        match serde_json::from_str::<OpenAIModelsResponse>(&body_text) {
            Ok(parsed) => Ok(parsed),
            Err(e) => {
                tracing::error!("Failed to parse models response as JSON. Error: {}", e);
                tracing::error!("Response body was: {}", body_text);
                Err(anyhow!("error decoding response body: {}", e))
            }
        }
    }
}
