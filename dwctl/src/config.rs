use clap::Parser;
use figment::{
    providers::{Env, Format, Yaml},
    Figment,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;
use url::Url;

use crate::errors::Error;

/// Simple CLI args - just for specifying config file
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// Path to configuration file
    #[arg(short = 'f', long, env = "DWCTL_CONFIG", default_value = "config.yaml")]
    pub config: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub host: String,
    pub port: u16,
    /// Deprecated: Use `database` field instead. Kept for backward compatibility.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database_url: Option<String>,
    /// Database configuration - either embedded or external
    pub database: DatabaseConfig,
    pub admin_email: String,
    pub admin_password: Option<String>,
    // Global secret key for encryption/signing
    pub secret_key: Option<String>,
    // Model sources are now properly plural
    pub model_sources: Vec<ModelSource>,
    // Frontend metadata
    pub metadata: Metadata,
    // Authentication configuration
    pub auth: AuthConfig,
    // Metrics configuration
    pub enable_metrics: bool,
    // Request logging configuration
    pub enable_request_logging: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DatabaseConfig {
    /// Use embedded PostgreSQL database (requires embedded-db feature)
    Embedded {
        /// Directory where database data will be stored (default: .dwctl_data/postgres)
        #[serde(skip_serializing_if = "Option::is_none")]
        data_dir: Option<PathBuf>,
        /// Whether to persist data between restarts (default: false/ephemeral)
        #[serde(default)]
        persistent: bool,
    },
    /// Use external PostgreSQL database
    External {
        /// Connection string for external database
        url: String,
    },
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        // Default to external for backward compatibility
        DatabaseConfig::External {
            url: "postgres://localhost:5432/dwctl".to_string(),
        }
    }
}

impl DatabaseConfig {
    /// Get the database URL, resolving embedded if needed
    /// This is used during startup to get the connection string
    pub fn is_embedded(&self) -> bool {
        matches!(self, DatabaseConfig::Embedded { .. })
    }

    /// Get external URL if available
    pub fn external_url(&self) -> Option<&str> {
        match self {
            DatabaseConfig::External { url } => Some(url),
            DatabaseConfig::Embedded { .. } => None,
        }
    }

    /// Get embedded data directory if configured
    pub fn embedded_data_dir(&self) -> Option<PathBuf> {
        match self {
            DatabaseConfig::Embedded { data_dir, .. } => data_dir.clone(),
            DatabaseConfig::External { .. } => None,
        }
    }

    /// Get embedded persistence flag if configured
    pub fn embedded_persistent(&self) -> bool {
        match self {
            DatabaseConfig::Embedded { persistent, .. } => *persistent,
            DatabaseConfig::External { .. } => false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Metadata {
    pub region: String,
    pub organization: String,
    pub registration_enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelSource {
    pub name: String,
    pub url: Url,
    pub api_key: Option<String>,
    #[serde(default = "ModelSource::default_sync_interval")]
    #[serde(with = "humantime_serde")]
    pub sync_interval: Duration,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
#[derive(Default)]
pub struct AuthConfig {
    pub native: NativeAuthConfig,
    pub proxy_header: ProxyHeaderAuthConfig,
    pub security: SecurityConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct NativeAuthConfig {
    pub enabled: bool,
    pub allow_registration: bool,
    pub password: PasswordConfig,
    pub session: SessionConfig,
    pub email: EmailConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ProxyHeaderAuthConfig {
    pub enabled: bool,
    pub header_name: String,
    pub groups_field_name: String,
    pub auto_create_users: bool,
    pub blacklisted_sso_groups: Vec<String>,
    pub provider_field_name: String,
    pub import_idp_groups: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SessionConfig {
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,
    pub cookie_name: String,
    pub cookie_secure: bool,
    pub cookie_same_site: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PasswordConfig {
    pub min_length: usize,
    pub max_length: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SecurityConfig {
    #[serde(with = "humantime_serde")]
    pub jwt_expiry: Duration,
    pub cors: CorsConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct CorsConfig {
    pub allowed_origins: Vec<CorsOrigin>,
    pub allow_credentials: bool,
    pub max_age: Option<u64>, // Cache preflight requests (seconds)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct EmailConfig {
    pub smtp: Option<SmtpConfig>,
    pub from_email: String,
    pub from_name: String,
    pub password_reset: PasswordResetEmailConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub use_tls: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PasswordResetEmailConfig {
    #[serde(with = "humantime_serde")]
    pub token_expiry: Duration,
    pub base_url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum CorsOrigin {
    #[serde(deserialize_with = "parse_wildcard")]
    Wildcard,
    #[serde(deserialize_with = "parse_url")]
    Url(Url),
}

fn parse_wildcard<'de, D>(deserializer: D) -> Result<(), D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer)?;
    if s == "*" {
        Ok(())
    } else {
        Err(serde::de::Error::custom("Expected '*'"))
    }
}

fn parse_url<'de, D>(deserializer: D) -> Result<Url, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s: String = Deserialize::deserialize(deserializer)?;
    Url::parse(&s).map_err(serde::de::Error::custom)
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".to_string(),
            port: 3001,
            database_url: None, // Deprecated field
            database: DatabaseConfig::default(),
            admin_email: "test@doubleword.ai".to_string(),
            admin_password: Some("hunter2".to_string()),
            secret_key: None,
            model_sources: vec![],
            metadata: Metadata::default(),
            auth: AuthConfig::default(),
            enable_metrics: true,
            enable_request_logging: true,
        }
    }
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            region: "UK South".to_string(),
            organization: "ACME Corp".to_string(),
            registration_enabled: true,
        }
    }
}

impl Default for ModelSource {
    fn default() -> Self {
        Self {
            name: String::new(),
            url: Url::parse("http://localhost:8080").unwrap(),
            api_key: None,
            sync_interval: Duration::from_secs(10),
        }
    }
}

impl Default for NativeAuthConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_registration: false,
            password: PasswordConfig::default(),
            session: SessionConfig::default(),
            email: EmailConfig::default(),
        }
    }
}

impl Default for ProxyHeaderAuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            header_name: "x-doubleword-user".to_string(),
            groups_field_name: "x-doubleword-user-groups".to_string(),
            provider_field_name: "x-doubleword-sso-provider".to_string(),
            auto_create_users: true,
            blacklisted_sso_groups: Vec::new(),
            import_idp_groups: false,
        }
    }
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(24 * 60 * 60), // 24 hours
            cookie_name: "dwctl_session".to_string(),
            cookie_secure: true,
            cookie_same_site: "strict".to_string(),
        }
    }
}

impl Default for PasswordConfig {
    fn default() -> Self {
        Self {
            min_length: 8,
            max_length: 64,
        }
    }
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            jwt_expiry: Duration::from_secs(24 * 60 * 60), // 24 hours
            cors: CorsConfig::default(),
        }
    }
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            allowed_origins: vec![
                CorsOrigin::Url(Url::parse("htt://localhost:3001").unwrap()), // Development frontend (Vite)
            ],
            allow_credentials: true,
            max_age: Some(3600), // Cache preflight for 1 hour
        }
    }
}

impl Default for EmailConfig {
    fn default() -> Self {
        Self {
            smtp: None, // Will use file transport in development
            from_email: "noreply@example.com".to_string(),
            from_name: "dwctl App".to_string(),
            password_reset: PasswordResetEmailConfig::default(),
        }
    }
}

impl Default for PasswordResetEmailConfig {
    fn default() -> Self {
        Self {
            token_expiry: Duration::from_secs(30 * 60),    // 30 minutes
            base_url: "http://localhost:3001".to_string(), // Frontend URL
        }
    }
}

impl ModelSource {
    fn default_sync_interval() -> Duration {
        Duration::from_secs(10)
    }
}

impl Config {
    #[allow(clippy::result_large_err)]
    pub fn load(args: &Args) -> Result<Self, figment::Error> {
        let mut config: Self = Self::figment(args).extract()?;

        // if database_url is set, use it
        if let Some(url) = config.database_url.take() {
            config.database = DatabaseConfig::External { url };
        }

        config.validate().map_err(|e| figment::Error::from(e.to_string()))?;
        Ok(config)
    }

    /// Get the database connection string
    /// Returns None if using embedded database (connection string will be set at runtime)
    pub fn database_url(&self) -> Option<&str> {
        self.database.external_url()
    }

    /// Validate the configuration for consistency and required fields
    pub fn validate(&self) -> Result<(), Error> {
        // Validate native authentication requirements
        if self.auth.native.enabled {
            if self.secret_key.is_none() {
                return Err(Error::Internal {
                    operation: "Config validation: Native authentication is enabled but secret_key is not configured. \
                     Please set DWCTL_SECRET_KEY environment variable or add secret_key to config file."
                        .to_string(),
                });
            }

            // Validate password requirements
            if self.auth.native.password.min_length > self.auth.native.password.max_length {
                return Err(Error::Internal {
                    operation: format!(
                        "Config validation: Invalid password configuration: min_length ({}) cannot be greater than max_length ({})",
                        self.auth.native.password.min_length, self.auth.native.password.max_length
                    ),
                });
            }

            if self.auth.native.password.min_length < 1 {
                return Err(Error::Internal {
                    operation: "Config validation: Invalid password configuration: min_length must be at least 1".to_string(),
                });
            }
        }

        // Validate JWT expiry duration is reasonable
        if self.auth.security.jwt_expiry.as_secs() < 300 {
            // Less than 5 minutes
            return Err(Error::Internal {
                operation: "Config validation: JWT expiry duration is too short (minimum 5 minutes)".to_string(),
            });
        }

        if self.auth.security.jwt_expiry.as_secs() > 86400 * 30 {
            // More than 30 days
            return Err(Error::Internal {
                operation: "Config validation: JWT expiry duration is too long (maximum 30 days)".to_string(),
            });
        }

        // Validate that at least one auth method is enabled
        if !self.auth.native.enabled && !self.auth.proxy_header.enabled {
            return Err(Error::Internal {
                operation:
                    "Config validation: No authentication methods are enabled. Please enable either native or proxy_header authentication."
                        .to_string(),
            });
        }

        // Validate CORS configuration
        if self.auth.security.cors.allowed_origins.is_empty() {
            return Err(Error::Internal {
                operation: "Config validation: CORS allowed_origins cannot be empty. Add at least one allowed origin.".to_string(),
            });
        }

        // Validate that wildcard is not used with credentials
        let has_wildcard = self
            .auth
            .security
            .cors
            .allowed_origins
            .iter()
            .any(|origin| matches!(origin, CorsOrigin::Wildcard));
        if has_wildcard && self.auth.security.cors.allow_credentials {
            return Err(Error::Internal {
                operation: "Config validation: CORS cannot use wildcard origin '*' with allow_credentials=true. Specify explicit origins."
                    .to_string(),
            });
        }

        Ok(())
    }

    pub fn figment(args: &Args) -> Figment {
        Figment::new()
            // Load base config file
            .merge(Yaml::file(&args.config))
            // Environment variables can still override specific values
            .merge(Env::prefixed("DWCTL_").split("__"))
            // Common DATABASE_URL pattern
            .merge(Env::raw().only(&["DATABASE_URL"]))
    }

    pub fn bind_address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::Jail;

    #[test]
    fn test_model_sources_config() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: hello
model_sources:
  - name: openai
    url: https://api.openai.com
    api_key: sk-test
    sync_interval: 30s
  - name: internal
    url: http://internal:8080
"#,
            )?;

            let args = Args {
                config: "test.yaml".to_string(),
            };

            let config = Config::load(&args)?;

            assert_eq!(config.model_sources.len(), 2);

            let openai = &config.model_sources[0];
            assert_eq!(openai.name, "openai");
            assert_eq!(openai.url.as_str(), "https://api.openai.com/");
            assert_eq!(openai.api_key.as_deref(), Some("sk-test"));
            assert_eq!(openai.sync_interval, Duration::from_secs(30));

            let internal = &config.model_sources[1];
            assert_eq!(internal.name, "internal");
            assert_eq!(internal.sync_interval, Duration::from_secs(10)); // default

            Ok(())
        });
    }

    #[test]
    fn test_env_override() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: hello
metadata:
  region: US East
  organization: Test Corp
"#,
            )?;

            jail.set_env("DWCTL_HOST", "127.0.0.1");
            jail.set_env("DWCTL_PORT", "8080");

            let args = Args {
                config: "test.yaml".to_string(),
            };

            let config = Config::load(&args)?;

            // Env vars should override
            assert_eq!(config.host, "127.0.0.1");
            assert_eq!(config.port, 8080);

            // YAML values should be preserved
            assert_eq!(config.metadata.region, "US East");
            assert_eq!(config.metadata.organization, "Test Corp");

            Ok(())
        });
    }

    #[test]
    fn test_auth_config_override() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "test.yaml",
                r#"
secret_key: "test-secret-key-for-testing"
auth:
  native:
    enabled: true
    allow_registration: false
    password:
      min_length: 12
  proxy_header:
    enabled: false
    header_name: "x-custom-user"
  security:
    jwt_expiry: "2h"
"#,
            )?;

            let args = Args {
                config: "test.yaml".to_string(),
            };

            let config = Config::load(&args)?;

            // Check overridden values
            assert!(config.auth.native.enabled);
            assert!(!config.auth.native.allow_registration);
            assert_eq!(config.auth.native.password.min_length, 12);
            assert_eq!(config.auth.native.password.max_length, 64); // still default

            assert!(!config.auth.proxy_header.enabled);
            assert_eq!(config.auth.proxy_header.header_name, "x-custom-user");

            assert_eq!(config.auth.security.jwt_expiry, Duration::from_secs(2 * 60 * 60));

            Ok(())
        });
    }

    #[test]
    fn test_config_validation_native_auth_missing_secret() {
        let mut config = Config::default();
        config.auth.native.enabled = true;
        config.secret_key = None;

        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("secret_key is not configured"));
    }

    #[test]
    fn test_config_validation_invalid_password_length() {
        let mut config = Config::default();
        config.auth.native.enabled = true;
        config.secret_key = Some("test-key".to_string());
        config.auth.native.password.min_length = 10;
        config.auth.native.password.max_length = 5;

        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("min_length"));
    }

    #[test]
    fn test_config_validation_no_auth_methods_enabled() {
        let mut config = Config::default();
        config.auth.native.enabled = false;
        config.auth.proxy_header.enabled = false;

        let result = config.validate();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No authentication methods"));
    }

    #[test]
    fn test_config_validation_valid_config() {
        let mut config = Config::default();
        config.auth.native.enabled = true;
        config.secret_key = Some("test-secret-key".to_string());

        let result = config.validate();
        assert!(result.is_ok());
    }
}
