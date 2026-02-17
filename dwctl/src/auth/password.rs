//! Password hashing and verification.

use argon2::{
    Algorithm, Argon2, Params, Version,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng},
};
use base64::{Engine as _, engine::general_purpose};
use rand::prelude::RngExt;
use rand::rng;

use crate::errors::Error;

/// Argon2 hashing parameters.
#[derive(Debug, Clone, Copy)]
pub struct Argon2Params {
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
}

impl Argon2Params {
    /// Create Argon2 instance with these parameters.
    fn to_argon2(self) -> Result<Argon2<'static>, Error> {
        let params = Params::new(self.memory_kib, self.iterations, self.parallelism, None).map_err(|e| Error::Internal {
            operation: format!("create argon2 params: {e}"),
        })?;

        Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
    }
}

impl Default for Argon2Params {
    /// Secure defaults for production (Argon2id RFC recommendations)
    fn default() -> Self {
        Self {
            memory_kib: 19456, // 19 MB
            iterations: 2,
            parallelism: 1,
        }
    }
}

/// Hash a string using Argon2 (used for passwords and tokens).
///
/// Uses the provided parameters or secure defaults if None.
pub fn hash_string_with_params(input: &str, params: Option<Argon2Params>) -> Result<String, Error> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = if let Some(p) = params {
        p.to_argon2()?
    } else {
        Argon2Params::default().to_argon2()?
    };

    let hash = argon2.hash_password(input.as_bytes(), &salt).map_err(|e| Error::Internal {
        operation: format!("hash string: {e}"),
    })?;

    Ok(hash.to_string())
}

/// Hash a string using Argon2 with default secure parameters.
pub fn hash_string(input: &str) -> Result<String, Error> {
    hash_string_with_params(input, None)
}

/// Verify a string against a hash.
///
/// Note: Verification uses the parameters embedded in the hash itself.
pub fn verify_string(input: &str, hash: &str) -> Result<bool, Error> {
    let parsed_hash = PasswordHash::new(hash).map_err(|e| Error::Internal {
        operation: format!("parse hash: {e}"),
    })?;

    // Verification always uses params from the hash
    let argon2 = Argon2::default();
    Ok(argon2.verify_password(input.as_bytes(), &parsed_hash).is_ok())
}

/// Generate a secure random token for password reset
pub fn generate_reset_token() -> String {
    // Generate 32 bytes (256 bits) of cryptographically secure random data
    let mut token_bytes = [0u8; 32];
    rng().fill(&mut token_bytes);

    // Encode as base64url without padding
    general_purpose::URL_SAFE_NO_PAD.encode(token_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_string_hashing() {
        let input = "test_password_123";
        let hash = hash_string(input).unwrap();

        // Hash should not be empty
        assert!(!hash.is_empty());

        // Should verify correctly
        assert!(verify_string(input, &hash).unwrap());

        // Should fail with wrong input
        assert!(!verify_string("wrong_password", &hash).unwrap());
    }

    #[test]
    fn test_different_inputs_different_hashes() {
        let input1 = "password1";
        let input2 = "password2";

        let hash1 = hash_string(input1).unwrap();
        let hash2 = hash_string(input2).unwrap();

        // Different inputs should produce different hashes
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_same_input_different_hashes() {
        let input = "same_password";

        let hash1 = hash_string(input).unwrap();
        let hash2 = hash_string(input).unwrap();

        // Same input should produce different hashes due to salt
        assert_ne!(hash1, hash2);

        // But both should verify correctly
        assert!(verify_string(input, &hash1).unwrap());
        assert!(verify_string(input, &hash2).unwrap());
    }

    #[test]
    fn test_generate_reset_token() {
        let token1 = generate_reset_token();
        let token2 = generate_reset_token();

        // Tokens should be different
        assert_ne!(token1, token2);

        // Tokens should be base64url encoded (43 chars for 32 bytes)
        assert_eq!(token1.len(), 43);
        assert_eq!(token2.len(), 43);

        // Should only contain base64url characters
        assert!(token1.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert!(token2.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));

        // Should not contain padding
        assert!(!token1.contains('='));
        assert!(!token2.contains('='));
    }
}
