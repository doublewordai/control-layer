//! Cryptographic utilities for API key generation and storage.

use base64::{Engine as _, engine::general_purpose};
use rand::prelude::RngExt;
use rand::rng;
use sha2::{Digest, Sha256};

/// Number of leading characters of an API key kept in `api_keys.secret_prefix`:
/// `sk-` plus five random characters (30 bits), enough for a person to tell
/// keys apart without meaningfully weakening the rest of the secret.
pub const API_KEY_PREFIX_LEN: usize = 8;

/// Generates a cryptographically secure API key with 256 bits of entropy.
///
/// The key is formatted as `sk-{base64url_encoded_random_bytes}` where the
/// random bytes are 32 bytes (256 bits) of cryptographically secure random data.
///
/// # Returns
///
/// A string in the format `sk-{44_character_base64url_string}`
///
/// # Examples
///
/// ```ignore
/// use your_crate::crypto::generate_api_key;
///
/// let api_key = generate_api_key();
/// assert!(api_key.starts_with("sk-"));
/// assert_eq!(api_key.len(), 47); // "sk-" + 44 base64url chars
/// ```
pub fn generate_api_key() -> String {
    // Generate 32 bytes (256 bits) of cryptographically secure random data
    let mut key_bytes = [0u8; 32];
    rng().fill(&mut key_bytes);

    format!("sk-{}", general_purpose::URL_SAFE_NO_PAD.encode(key_bytes))
}

/// Hashes an API key secret for storage in `api_keys.secret_hash`.
///
/// Plain SHA-256 over the UTF-8 bytes, with no salt or pepper: generated keys
/// carry 256 bits of entropy, so a slow or keyed hash adds nothing, and this
/// form is reproducible in SQL as `sha256(convert_to(secret, 'UTF8'))`.
pub fn hash_api_key(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

/// The displayable prefix of an API key secret stored in `api_keys.secret_prefix`.
///
/// Takes the first [`API_KEY_PREFIX_LEN`] characters (fewer if the secret is shorter).
pub fn api_key_prefix(secret: &str) -> String {
    secret.chars().take(API_KEY_PREFIX_LEN).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_generate_api_key_format() {
        let key = generate_api_key();

        // Should start with "sk-"
        assert!(key.starts_with("sk-"));

        // Should be correct length: "sk-" (3) + base64url(32 bytes) (43)
        assert_eq!(key.len(), 46);

        // Should only contain valid base64url characters after prefix
        let key_part = &key[3..];
        assert!(key_part.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn test_generate_api_key_uniqueness() {
        let mut keys = HashSet::new();

        // Generate 1000 keys and ensure they're all unique
        for _ in 0..1000 {
            let key = generate_api_key();
            assert!(keys.insert(key), "Generated duplicate API key");
        }
    }

    #[test]
    fn test_hash_api_key_is_sha256() {
        // SHA-256("abc"), FIPS 180-2 test vector.
        assert_eq!(
            hex::encode(hash_api_key("abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let key = generate_api_key();
        assert_eq!(hash_api_key(&key), hash_api_key(&key));
        assert_ne!(hash_api_key(&key), hash_api_key(&generate_api_key()));
    }

    #[test]
    fn test_api_key_prefix() {
        let key = generate_api_key();
        let prefix = api_key_prefix(&key);
        assert_eq!(prefix.len(), API_KEY_PREFIX_LEN);
        assert!(prefix.starts_with("sk-"));
        assert!(key.starts_with(&prefix));
        assert_eq!(api_key_prefix("sk-ab"), "sk-ab");
    }

    #[test]
    fn test_generate_api_key_no_padding() {
        let key = generate_api_key();

        // Should not contain padding characters
        assert!(!key.contains('='));
    }
}
