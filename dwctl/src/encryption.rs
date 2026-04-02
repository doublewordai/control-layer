//! Authenticated encryption for sensitive data at rest (connection credentials, etc.).
//!
//! Uses AES-256-GCM with a random nonce per encryption. The ciphertext format is:
//! `nonce (12 bytes) || ciphertext || tag (16 bytes)`, stored as BYTEA in PostgreSQL.

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key, Nonce};
use base64::{Engine as _, engine::general_purpose};

/// Errors from encryption/decryption operations.
#[derive(Debug, thiserror::Error)]
pub enum EncryptionError {
    #[error("encryption key must be exactly 32 bytes (got {0})")]
    InvalidKeyLength(usize),

    #[error("encryption failed")]
    EncryptionFailed,

    #[error("decryption failed — wrong key or corrupted ciphertext")]
    DecryptionFailed,

    #[error("ciphertext too short to contain nonce + tag")]
    CiphertextTooShort,

    #[error("base64 decode failed: {0}")]
    Base64Decode(#[from] base64::DecodeError),
}

/// Encrypt a plaintext byte slice. Returns `nonce || ciphertext || tag`.
pub fn encrypt(key_bytes: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, EncryptionError> {
    let key = parse_key(key_bytes)?;
    let cipher = Aes256Gcm::new(&key);
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);

    let ciphertext = cipher.encrypt(&nonce, plaintext).map_err(|_| EncryptionError::EncryptionFailed)?;

    let mut out = Vec::with_capacity(12 + ciphertext.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypt a blob produced by [`encrypt`]. Expects `nonce (12) || ciphertext || tag (16)`.
pub fn decrypt(key_bytes: &[u8], blob: &[u8]) -> Result<Vec<u8>, EncryptionError> {
    if blob.len() < 12 + 16 {
        return Err(EncryptionError::CiphertextTooShort);
    }
    let key = parse_key(key_bytes)?;
    let cipher = Aes256Gcm::new(&key);

    let (nonce_bytes, ciphertext) = blob.split_at(12);
    let nonce = Nonce::from_slice(nonce_bytes);

    cipher.decrypt(nonce, ciphertext).map_err(|_| EncryptionError::DecryptionFailed)
}

/// Encrypt a JSON value, returning the blob as raw bytes for BYTEA storage.
pub fn encrypt_json(key_bytes: &[u8], value: &serde_json::Value) -> Result<Vec<u8>, EncryptionError> {
    let plaintext = serde_json::to_vec(value).map_err(|_| EncryptionError::EncryptionFailed)?;
    encrypt(key_bytes, &plaintext)
}

/// Decrypt a blob back to a JSON value.
pub fn decrypt_json(key_bytes: &[u8], blob: &[u8]) -> Result<serde_json::Value, EncryptionError> {
    let plaintext = decrypt(key_bytes, blob)?;
    serde_json::from_slice(&plaintext).map_err(|_| EncryptionError::DecryptionFailed)
}

/// Derive a 32-byte encryption key from a config secret of any length.
///
/// - If the secret is a valid base64 string that decodes to exactly 32 bytes, use it directly.
/// - If the secret is exactly 32 raw bytes, use it directly.
/// - Otherwise, SHA-256 hash the secret to produce a 32-byte key.
pub fn derive_encryption_key(secret: &str) -> Result<Vec<u8>, EncryptionError> {
    // Try base64 first — allows users to provide a proper random key
    if let Ok(bytes) = general_purpose::STANDARD.decode(secret) {
        if bytes.len() == 32 {
            return Ok(bytes);
        }
    }
    // Try raw bytes
    let bytes = secret.as_bytes();
    if bytes.len() == 32 {
        return Ok(bytes.to_vec());
    }
    // Fall back to SHA-256 to derive a 32-byte key from any-length secret
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(bytes);
    Ok(hash.to_vec())
}

fn parse_key(key_bytes: &[u8]) -> Result<Key<Aes256Gcm>, EncryptionError> {
    if key_bytes.len() != 32 {
        return Err(EncryptionError::InvalidKeyLength(key_bytes.len()));
    }
    Ok(*Key::<Aes256Gcm>::from_slice(key_bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> Vec<u8> {
        vec![0u8; 32]
    }

    #[test]
    fn roundtrip() {
        let plaintext = b"hello world";
        let blob = encrypt(&test_key(), plaintext).unwrap();
        let decrypted = decrypt(&test_key(), &blob).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn roundtrip_json() {
        let value = serde_json::json!({"bucket": "my-bucket", "secret": "s3cr3t"});
        let blob = encrypt_json(&test_key(), &value).unwrap();
        let decrypted = decrypt_json(&test_key(), &blob).unwrap();
        assert_eq!(decrypted, value);
    }

    #[test]
    fn wrong_key_fails() {
        let blob = encrypt(&test_key(), b"secret").unwrap();
        let wrong_key = vec![1u8; 32];
        assert!(decrypt(&wrong_key, &blob).is_err());
    }

    #[test]
    fn corrupted_ciphertext_fails() {
        let mut blob = encrypt(&test_key(), b"secret").unwrap();
        if let Some(last) = blob.last_mut() {
            *last ^= 0xFF;
        }
        assert!(decrypt(&test_key(), &blob).is_err());
    }

    #[test]
    fn too_short_fails() {
        assert!(decrypt(&test_key(), &[0u8; 10]).is_err());
    }
}
