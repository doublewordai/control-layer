//! JWT session token creation and verification.

use chrono::Utc;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use crate::{
    api::models::users::{CurrentUser, Role},
    config::Config,
    errors::Error,
    types::UserId,
};

/// JWT session claims
#[derive(Debug, Serialize, Deserialize)]
pub struct SessionClaims {
    pub sub: UserId,      // Subject (user ID)
    pub email: String,    // User email
    pub username: String, // Username
    pub roles: Vec<Role>, // User roles
    pub is_admin: bool,   // Admin flag
    pub exp: i64,         // Expiration time
    pub iat: i64,         // Issued at
}

impl SessionClaims {
    /// Create new session claims for a user
    pub fn new(user: &CurrentUser, config: &Config) -> Self {
        let now = Utc::now();
        let exp = now + config.auth.security.jwt_expiry;

        Self {
            sub: user.id,
            email: user.email.clone(),
            username: user.username.clone(),
            roles: user.roles.clone(),
            is_admin: user.is_admin,
            exp: exp.timestamp(),
            iat: now.timestamp(),
        }
    }
}

impl From<SessionClaims> for CurrentUser {
    fn from(claims: SessionClaims) -> Self {
        Self {
            id: claims.sub,
            email: claims.email,
            username: claims.username,
            roles: claims.roles,
            is_admin: claims.is_admin,
            display_name: None, // Not stored in JWT
            avatar_url: None,   // Not stored in JWT
            payment_provider_id: None,
        }
    }
}

/// Create a JWT token for a user session
pub fn create_session_token(user: &CurrentUser, config: &Config) -> Result<String, Error> {
    let claims = SessionClaims::new(user, config);
    let secret_key = config.secret_key.as_ref().ok_or_else(|| Error::Internal {
        operation: "JWT sessions: secret_key is required".to_string(),
    })?;

    let key = EncodingKey::from_secret(secret_key.as_bytes());
    encode(&Header::default(), &claims, &key).map_err(|e| Error::Internal {
        operation: format!("create JWT: {e}"),
    })
}

/// Verify and decode a JWT session token
pub fn verify_session_token(token: &str, config: &Config) -> Result<CurrentUser, Error> {
    let secret_key = config.secret_key.as_ref().ok_or_else(|| Error::Internal {
        operation: "JWT sessions: secret_key is required".to_string(),
    })?;

    let key = DecodingKey::from_secret(secret_key.as_bytes());
    let validation = Validation::default();

    let token_data = decode::<SessionClaims>(token, &key, &validation).map_err(|e| match e.kind() {
        jsonwebtoken::errors::ErrorKind::ExpiredSignature => Error::Unauthenticated { message: None },
        jsonwebtoken::errors::ErrorKind::InvalidToken => Error::Unauthenticated { message: None },
        // TODO: I'm not sure 500 is right here, lots of the other error variants are clearly 4xx
        _ => Error::Internal {
            operation: format!("JWT verification: {e}"),
        },
    })?;

    Ok(CurrentUser::from(token_data.claims))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthConfig, SecurityConfig};
    use std::time::Duration;
    use uuid::Uuid;

    fn create_test_config() -> Config {
        Config {
            secret_key: Some("test-secret-key-for-jwt".to_string()),
            auth: AuthConfig {
                security: SecurityConfig {
                    jwt_expiry: Duration::from_secs(3600), // 1 hour
                    cors: crate::config::CorsConfig::default(),
                },
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn create_test_user() -> CurrentUser {
        CurrentUser {
            id: Uuid::new_v4(),
            email: "test@example.com".to_string(),
            username: "testuser".to_string(),
            roles: vec![Role::StandardUser],
            is_admin: false,
            display_name: Some("Test User".to_string()),
            avatar_url: None,
            payment_provider_id: None,
        }
    }

    #[test]
    fn test_create_and_verify_session_token() {
        let config = create_test_config();
        let user = create_test_user();

        // Create token
        let token = create_session_token(&user, &config).unwrap();
        assert!(!token.is_empty());

        // Verify token
        let verified_user = verify_session_token(&token, &config).unwrap();

        // Check user data matches
        assert_eq!(verified_user.id, user.id);
        assert_eq!(verified_user.email, user.email);
        assert_eq!(verified_user.username, user.username);
        assert_eq!(verified_user.roles, user.roles);
        assert_eq!(verified_user.is_admin, user.is_admin);
    }

    #[test]
    fn test_verify_invalid_token() {
        let config = create_test_config();

        let result = verify_session_token("invalid.token.here", &config);
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_token_wrong_secret() {
        let mut config = create_test_config();
        let user = create_test_user();

        // Create token with one secret
        let token = create_session_token(&user, &config).unwrap();

        // Try to verify with different secret
        config.secret_key = Some("different-secret".to_string());
        let result = verify_session_token(&token, &config);
        assert!(result.is_err());
    }
}
