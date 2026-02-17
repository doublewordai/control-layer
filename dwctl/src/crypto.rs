//! Cryptographic utilities for API key generation.

use base64::{Engine as _, engine::general_purpose};
use rand::prelude::RngExt;
use rand::rng;

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
    fn test_generate_api_key_no_padding() {
        let key = generate_api_key();

        // Should not contain padding characters
        assert!(!key.contains('='));
    }
}
