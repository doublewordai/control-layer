//! Stateless, authenticated reasoning replay tokens.
//!
//! The encrypted payload is returned to the client and is never stored by the
//! server. It is bound to the model so a token cannot accidentally be replayed
//! against a different model.

use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

use crate::encryption;

const TOKEN_PREFIX: &str = "dwrs1.";
const KEY_DOMAIN: &str = "doubleword.responses.reasoning.v1\0";

#[derive(Clone)]
pub struct ReasoningTokenCodec {
    key: Arc<Vec<u8>>,
}

impl std::fmt::Debug for ReasoningTokenCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReasoningTokenCodec").finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReasoningTokenError {
    #[error("invalid encrypted reasoning token")]
    Invalid,
    #[error("could not encrypt reasoning token")]
    Encrypt,
}

#[derive(Serialize, Deserialize)]
struct Payload {
    model: String,
    reasoning: String,
}

impl ReasoningTokenCodec {
    pub fn from_secret(secret: &str) -> Self {
        Self {
            key: Arc::new(encryption::derive_encryption_key(&format!("{KEY_DOMAIN}{secret}"))),
        }
    }

    pub fn seal(&self, model: &str, reasoning: &str) -> Result<String, ReasoningTokenError> {
        let plaintext = serde_json::to_vec(&Payload {
            model: model.to_string(),
            reasoning: reasoning.to_string(),
        })
        .map_err(|_| ReasoningTokenError::Encrypt)?;
        let sealed = encryption::encrypt(self.key.as_slice(), &plaintext).map_err(|_| ReasoningTokenError::Encrypt)?;
        Ok(format!("{TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(sealed)))
    }

    pub fn open(&self, token: &str, expected_model: &str) -> Result<String, ReasoningTokenError> {
        let encoded = token.strip_prefix(TOKEN_PREFIX).ok_or(ReasoningTokenError::Invalid)?;
        let sealed = URL_SAFE_NO_PAD.decode(encoded).map_err(|_| ReasoningTokenError::Invalid)?;
        let plaintext = encryption::decrypt(self.key.as_slice(), &sealed).map_err(|_| ReasoningTokenError::Invalid)?;
        let payload: Payload = serde_json::from_slice(&plaintext).map_err(|_| ReasoningTokenError::Invalid)?;
        if payload.model != expected_model {
            return Err(ReasoningTokenError::Invalid);
        }
        Ok(payload.reasoning)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_round_trips_without_exposing_plaintext() {
        let codec = ReasoningTokenCodec::from_secret("test secret");
        let token = codec.seal("gpt-oss-20b", "private chain").unwrap();
        assert!(!token.contains("private chain"));
        assert_eq!(codec.open(&token, "gpt-oss-20b").unwrap(), "private chain");
    }

    #[test]
    fn token_is_authenticated_and_model_bound() {
        let codec = ReasoningTokenCodec::from_secret("test secret");
        let mut token = codec.seal("gpt-oss-20b", "private chain").unwrap();
        token.push('x');
        assert!(codec.open(&token, "gpt-oss-20b").is_err());

        let token = codec.seal("gpt-oss-20b", "private chain").unwrap();
        assert!(codec.open(&token, "other-model").is_err());
    }
}
