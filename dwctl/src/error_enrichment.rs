//! Error enrichment middleware for AI proxy requests.
//!
//! This module provides middleware that intercepts error responses from the onwards
//! routing layer and enriches them with helpful context. The primary use case is
//! transforming generic 403 errors (insufficient credits) into detailed responses
//! that explain the issue and suggest remediation.
//!
//! ## Architecture
//!
//! The middleware sits in the response path after onwards but before outlet:
//! ```text
//! Request → onwards (routing) → error_enrichment (this) → outlet (logging) → Response
//! ```
//!
//! ## Error Cases Enriched
//!
//! 1. **403 Forbidden - Insufficient Credits**: User's balance ≤ 0 for paid models
//!    - Shows current balance, model pricing, estimated cost
//!    - Lists available free models
//!    - Provides action links
//!
//! Future enhancements could include:
//! - Model access denied (user not in appropriate groups)
//! - Rate limiting errors
//! - Invalid API key context

pub use middleware::error_enrichment_middleware;

pub mod middleware {
    //! Middleware implementation for error enrichment.

    use crate::{db::handlers::Credits, error_enrichment::models::InsufficientCreditsError, types::UserId};
    use axum::{
        body::Body,
        extract::State,
        http::{Request, Response, StatusCode},
        middleware::Next,
    };
    use sqlx::PgPool;
    use tracing::{debug, instrument, warn};

    /// Middleware that enriches error responses from the AI proxy with helpful context
    ///
    /// Currently handles:
    /// - 403 Forbidden errors (likely insufficient credits) → enriched with balance, pricing, free models
    ///
    /// Future enhancements could include:
    /// - 404 Not Found (model access denied) → enriched with available models
    /// - 429 Too Many Requests (rate limiting) → enriched with limit info and reset time
    #[instrument(skip_all, fields(path = %request.uri().path(), method = %request.method()))]
    pub async fn error_enrichment_middleware(State(pool): State<PgPool>, request: Request<Body>, next: Next) -> Response<Body> {
        let path = request.uri().path().to_string();
        let authorization_header = request
            .headers()
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .map(|s| s.to_string());

        // Let the request proceed through onwards
        let response = next.run(request).await;

        // Only enrich errors on /ai/v1/* paths (the AI proxy)
        if !path.starts_with("/ai/") {
            return response;
        }

        // Only enrich 403 errors
        if response.status() != StatusCode::FORBIDDEN {
            return response;
        }

        debug!("Intercepted 403 response on AI proxy path, attempting enrichment");

        // Extract API key from Authorization header
        let api_key = match extract_api_key(&authorization_header) {
            Some(key) => key,
            None => {
                debug!("No API key in Authorization header, returning original response");
                return response;
            }
        };

        // Enrich the error response
        match enrich_forbidden_error(&pool, &api_key, &path).await {
            Ok(enriched_response) => {
                debug!("Successfully enriched 403 error response");
                enriched_response
            }
            Err(e) => {
                warn!("Failed to enrich error response: {}", e);
                // Return original response if enrichment fails
                response
            }
        }
    }

    /// Extracts the API key from the Authorization header
    fn extract_api_key(authorization_header: &Option<String>) -> Option<String> {
        let auth = authorization_header.as_ref()?;

        // Authorization header format: "Bearer <key>"
        if !auth.starts_with("Bearer ") && !auth.starts_with("bearer ") {
            return None;
        }

        Some(auth[7..].trim().to_string())
    }

    /// Enriches a 403 error response with contextual information
    #[instrument(skip(pool), err)]
    async fn enrich_forbidden_error(pool: &PgPool, api_key: &str, path: &str) -> Result<Response<Body>, anyhow::Error> {
        // Look up user_id from API key
        let user_id = lookup_user_id_from_api_key(pool, api_key).await?;

        debug!("Found user_id for API key: {}", user_id);

        // Extract model name from path (e.g., /ai/v1/chat/completions → look in request body)
        // For now, we'll leave the model as "unknown" since we don't have the request body
        // In the future, we could buffer the request body in the middleware
        let model_name = "unknown".to_string();

        // Query user's current balance
        let mut conn = pool.acquire().await?;
        let mut credits_repo = Credits::new(&mut conn);
        let balance = credits_repo.get_user_balance(user_id).await?;

        debug!("User balance: {}", balance);

        // Query model pricing - for now we'll use placeholder values
        // In the future, we could look up the actual model pricing from deployment_models table
        let (input_price, output_price) = (None, None);

        // Query available free models for this user
        let free_models = get_free_models_for_user(pool, user_id).await?;

        debug!("Found {} free models available", free_models.len());

        // Build enriched error response
        let enriched_error = InsufficientCreditsError::new(balance, model_name, input_price, output_price, free_models);

        let json_body = serde_json::to_string(&enriched_error)?;

        let response = Response::builder()
            .status(StatusCode::FORBIDDEN)
            .header("content-type", "application/json")
            .body(Body::from(json_body))?;

        Ok(response)
    }

    /// Looks up the user_id associated with an API key
    #[instrument(skip(pool, api_key), err)]
    async fn lookup_user_id_from_api_key(pool: &PgPool, api_key: &str) -> Result<UserId, anyhow::Error> {
        let result = sqlx::query!(
            r#"
            SELECT user_id
            FROM api_keys
            WHERE secret = $1
            "#,
            api_key
        )
        .fetch_optional(pool)
        .await?;

        match result {
            Some(record) => Ok(record.user_id),
            None => Err(anyhow::anyhow!("API key not found")),
        }
    }

    /// Gets the list of free models available to a user
    ///
    /// Free models are those where both input and output prices are NULL or 0
    #[instrument(skip(pool), err)]
    async fn get_free_models_for_user(pool: &PgPool, user_id: UserId) -> Result<Vec<String>, anyhow::Error> {
        // Query for free models that the user has access to (through groups)
        let records = sqlx::query!(
            r#"
            SELECT DISTINCT d.alias
            FROM deployed_models d
            INNER JOIN deployment_groups dg ON d.id = dg.deployment_id
            INNER JOIN user_groups ug ON dg.group_id = ug.group_id
            WHERE ug.user_id = $1
            AND (d.upstream_input_price_per_token IS NULL OR d.upstream_input_price_per_token = 0)
            AND (d.upstream_output_price_per_token IS NULL OR d.upstream_output_price_per_token = 0)
            ORDER BY d.alias
            LIMIT 10
            "#,
            user_id
        )
        .fetch_all(pool)
        .await?;

        Ok(records.into_iter().map(|r| r.alias).collect())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_extract_api_key_bearer() {
            let auth = Some("Bearer sk-test-key-123".to_string());
            assert_eq!(extract_api_key(&auth), Some("sk-test-key-123".to_string()));
        }

        #[test]
        fn test_extract_api_key_bearer_lowercase() {
            let auth = Some("bearer sk-test-key-456".to_string());
            assert_eq!(extract_api_key(&auth), Some("sk-test-key-456".to_string()));
        }

        #[test]
        fn test_extract_api_key_no_bearer() {
            let auth = Some("sk-test-key-789".to_string());
            assert_eq!(extract_api_key(&auth), None);
        }

        #[test]
        fn test_extract_api_key_none() {
            assert_eq!(extract_api_key(&None), None);
        }

        #[test]
        fn test_extract_api_key_with_whitespace() {
            let auth = Some("Bearer   sk-test-key-with-spaces  ".to_string());
            assert_eq!(extract_api_key(&auth), Some("sk-test-key-with-spaces".to_string()));
        }
    }
}

pub mod models {
    //! Data models for enriched error responses.

    use rust_decimal::Decimal;
    use serde::{Deserialize, Serialize};

    /// Enriched error response for insufficient credits
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct InsufficientCreditsError {
        /// Error type identifier
        pub error: String,
        /// Human-readable error message
        pub message: String,
        /// Detailed context about the error
        pub details: InsufficientCreditsDetails,
        /// Suggested actions to resolve the error
        pub actions: SuggestedActions,
    }

    /// Detailed information about the insufficient credits error
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct InsufficientCreditsDetails {
        /// User's current credit balance (as string to preserve precision)
        pub current_balance: String,
        /// The model that was requested
        pub model: String,
        /// Pricing information for the model
        pub pricing: ModelPricing,
        /// Estimated cost for a typical request (optional, could be calculated from input)
        #[serde(skip_serializing_if = "Option::is_none")]
        pub estimated_cost: Option<String>,
        /// List of free models the user can access
        #[serde(skip_serializing_if = "Vec::is_empty")]
        pub free_models_available: Vec<String>,
    }

    /// Pricing information for a model
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ModelPricing {
        /// Price per input token (as string to preserve precision)
        #[serde(skip_serializing_if = "Option::is_none")]
        pub input_price_per_token: Option<String>,
        /// Price per output token (as string to preserve precision)
        #[serde(skip_serializing_if = "Option::is_none")]
        pub output_price_per_token: Option<String>,
    }

    /// Suggested actions to resolve the error
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct SuggestedActions {
        /// Message indicating user should add credits
        pub message: String,
        /// Whether the user should contact an admin
        #[serde(skip_serializing_if = "Option::is_none")]
        pub contact_admin: Option<bool>,
    }

    impl InsufficientCreditsError {
        /// Creates a new insufficient credits error response
        pub fn new(
            current_balance: Decimal,
            model: String,
            input_price: Option<Decimal>,
            output_price: Option<Decimal>,
            free_models: Vec<String>,
        ) -> Self {
            Self {
                error: "InsufficientCredits".to_string(),
                message: "Your account has insufficient credits to access this model.".to_string(),
                details: InsufficientCreditsDetails {
                    current_balance: current_balance.to_string(),
                    model,
                    pricing: ModelPricing {
                        input_price_per_token: input_price.map(|p| p.to_string()),
                        output_price_per_token: output_price.map(|p| p.to_string()),
                    },
                    estimated_cost: None, // Could be calculated if we parse the request body
                    free_models_available: free_models,
                },
                actions: SuggestedActions {
                    message: "Please add credits to your account or contact your administrator.".to_string(),
                    contact_admin: Some(true),
                },
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use rust_decimal::Decimal;
        use std::str::FromStr;

        #[test]
        fn test_insufficient_credits_error_creation() {
            let balance = Decimal::from_str("0.00").unwrap();
            let model = "gpt-4".to_string();
            let input_price = Some(Decimal::from_str("0.00001").unwrap());
            let output_price = Some(Decimal::from_str("0.00003").unwrap());
            let free_models = vec!["llama-3-8b".to_string(), "phi-3-mini".to_string()];

            let error = InsufficientCreditsError::new(balance, model.clone(), input_price, output_price, free_models.clone());

            assert_eq!(error.error, "InsufficientCredits");
            assert_eq!(error.message, "Your account has insufficient credits to access this model.");
            assert_eq!(error.details.current_balance, "0.00");
            assert_eq!(error.details.model, model);
            assert_eq!(error.details.pricing.input_price_per_token, Some("0.00001".to_string()));
            assert_eq!(error.details.pricing.output_price_per_token, Some("0.00003".to_string()));
            assert_eq!(error.details.free_models_available, free_models);
            assert_eq!(error.actions.contact_admin, Some(true));
        }

        #[test]
        fn test_insufficient_credits_error_serialization() {
            let balance = Decimal::from_str("-5.25").unwrap();
            let model = "gpt-4-turbo".to_string();
            let input_price = Some(Decimal::from_str("0.00001").unwrap());
            let output_price = Some(Decimal::from_str("0.00003").unwrap());
            let free_models = vec![];

            let error = InsufficientCreditsError::new(balance, model, input_price, output_price, free_models);

            let json = serde_json::to_string(&error).unwrap();
            assert!(json.contains("InsufficientCredits"));
            assert!(json.contains("-5.25"));
            assert!(json.contains("gpt-4-turbo"));
        }

        #[test]
        fn test_insufficient_credits_error_no_pricing() {
            let balance = Decimal::from_str("10.50").unwrap();
            let model = "custom-model".to_string();
            let free_models = vec!["free-model-1".to_string()];

            let error = InsufficientCreditsError::new(balance, model, None, None, free_models);

            assert_eq!(error.details.pricing.input_price_per_token, None);
            assert_eq!(error.details.pricing.output_price_per_token, None);
        }
    }
}
