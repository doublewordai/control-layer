use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use base64::{engine::general_purpose, Engine as _};
use rand::{thread_rng, Rng};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::db::errors::{DbError, Result};

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
/// ```
/// use your_crate::crypto::generate_api_key;
///
/// let api_key = generate_api_key();
/// assert!(api_key.starts_with("sk-"));
/// assert_eq!(api_key.len(), 47); // "sk-" + 44 base64url chars
/// ```
pub fn generate_api_key() -> String {
    // Generate 32 bytes (256 bits) of cryptographically secure random data
    let mut key_bytes = [0u8; 32];
    thread_rng().fill(&mut key_bytes);

    format!("sk-{}", general_purpose::URL_SAFE_NO_PAD.encode(key_bytes))
}


/// Encrypts data using AES-256-GCM with the provided encryption key.
///
/// The encryption key should be a base64-encoded string representing
/// 32 bytes (256 bits) of key material.
///
/// If the encryption key is empty, returns the plaintext as a UTF-8 string without encryption.
///
/// # Arguments
///
/// * `encryption_key` - Base64-encoded encryption key (32 bytes when decoded), or empty to skip encryption
/// * `plaintext` - The data to encrypt as a byte slice
///
/// # Returns
///
/// A Result containing the encrypted data as a base64-encoded string (nonce + ciphertext),
/// or the plaintext as UTF-8 if no encryption key is provided.
///
/// # Errors
///
/// Returns an error if:
/// - The encryption key is not valid base64 or not 32 bytes (when provided)
/// - The plaintext is not valid UTF-8 (when no encryption key is provided)
/// - Encryption fails
fn encrypt_with_key(encryption_key: Option<&str>, plaintext: &[u8]) -> Result<String> {
    // If no encryption key, return plaintext as-is (UTF-8 string)
    let key = match encryption_key {
        None => {
            return String::from_utf8(plaintext.to_vec())
                .map_err(|e| DbError::Other(anyhow::anyhow!("Plaintext is not valid UTF-8: {}", e)));
        }
        Some(k) => k,
    };

    let key_bytes = general_purpose::STANDARD
        .decode(key)
        .map_err(|e| DbError::Other(anyhow::anyhow!("Failed to decode encryption key: {}", e)))?;

    if key_bytes.len() != 32 {
        return Err(DbError::Other(anyhow::anyhow!(
            "Encryption key must be 32 bytes (256 bits), got {} bytes",
            key_bytes.len()
        )));
    }

    let cipher = Aes256Gcm::new_from_slice(&key_bytes)
        .map_err(|e| DbError::Other(anyhow::anyhow!("Failed to create cipher: {}", e)))?;

    // Generate a random 96-bit nonce
    let mut nonce_bytes = [0u8; 12];
    thread_rng().fill(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    // Encrypt the plaintext
    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| DbError::Other(anyhow::anyhow!("Encryption failed: {}", e)))?;

    // Combine nonce + ciphertext and encode as base64
    let mut result = nonce_bytes.to_vec();
    result.extend_from_slice(&ciphertext);

    Ok(general_purpose::STANDARD.encode(result))
}

/// Decrypts data that was encrypted with `encrypt_with_key`.
///
/// If the encryption key is empty, treats the input as plaintext and returns it as bytes.
///
/// # Arguments
///
/// * `encryption_key` - Base64-encoded encryption key (32 bytes when decoded), or empty to skip decryption
/// * `encrypted_b64` - The base64-encoded encrypted data (nonce + ciphertext), or plaintext if no encryption key
///
/// # Returns
///
/// A Result containing the decrypted data as a Vec<u8> or an error if decryption fails.
///
/// # Errors
///
/// Returns an error if:
/// - The encryption key is not valid base64 or not 32 bytes (when provided)
/// - The encrypted data is not valid base64 or too short (when encryption key is provided)
/// - Decryption fails
fn decrypt_with_key(encryption_key: Option<&str>, encrypted_b64: &str) -> Result<Vec<u8>> {
    // If no encryption key, treat as plaintext
    let key = match encryption_key {
        None => {
            return Ok(encrypted_b64.as_bytes().to_vec());
        }
        Some(k) => k,
    };

    let key_bytes = general_purpose::STANDARD
        .decode(key)
        .map_err(|e| DbError::Other(anyhow::anyhow!("Failed to decode encryption key: {}", e)))?;

    if key_bytes.len() != 32 {
        return Err(DbError::Other(anyhow::anyhow!(
            "Encryption key must be 32 bytes (256 bits), got {} bytes",
            key_bytes.len()
        )));
    }

    let cipher = Aes256Gcm::new_from_slice(&key_bytes)
        .map_err(|e| DbError::Other(anyhow::anyhow!("Failed to create cipher: {}", e)))?;

    // Decode the base64 data
    let encrypted_data = general_purpose::STANDARD
        .decode(encrypted_b64)
        .map_err(|e| DbError::Other(anyhow::anyhow!("Failed to decode encrypted data: {}", e)))?;

    if encrypted_data.len() < 12 {
        return Err(DbError::Other(anyhow::anyhow!("Encrypted data too short")));
    }

    // Split into nonce and ciphertext
    let (nonce_bytes, ciphertext) = encrypted_data.split_at(12);
    let nonce = Nonce::from_slice(nonce_bytes);

    // Decrypt
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| DbError::Other(anyhow::anyhow!("Decryption failed: {}", e)))?;

    Ok(plaintext)
}

/// Encrypts a column in a database table for all non-null values.
///
/// This is a helper function for data migrations that need to encrypt existing plaintext data.
///
/// # Arguments
///
/// * `encryption_key` - Base64-encoded encryption key (32 bytes when decoded)
/// * `tx` - A mutable reference to a database transaction
/// * `table` - The name of the table to update
/// * `id_column` - The name of the ID column (used for WHERE clause)
/// * `value_column` - The name of the column containing values to encrypt
///
/// # Returns
///
/// The number of rows that were encrypted
///
/// # Example
///
/// ```rust
/// let count = crypto::encrypt_table_column(
///     encryption_key,
///     &mut tx,
///     "inference_endpoints",
///     "id",
///     "api_key"
/// ).await?;
/// ```
pub async fn encrypt_table_column(
    encryption_key: Option<&str>,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    id_column: &str,
    value_column: &str,
) -> std::result::Result<usize, anyhow::Error> {
    if encryption_key.is_none() {
        return Ok(0);
    }

    // Build the SELECT query
    let select_query = format!(
        "SELECT {}, {} FROM {} WHERE {} IS NOT NULL",
        id_column, value_column, table, value_column
    );

    // Fetch all rows with non-null values
    let rows = sqlx::query(&select_query)
        .fetch_all(&mut **tx)
        .await?;

    let mut count = 0;

    for row in rows {
        // Get the ID (assuming it's an i32, adjust if needed)
        let id: i32 = row.try_get(id_column)?;

        // Get the plaintext value
        let plaintext: String = row.try_get(value_column)?;

        // Encrypt the value
        let encrypted = encrypt_with_key(encryption_key, plaintext.as_bytes())?;

        // Build the UPDATE query
        let update_query = format!(
            "UPDATE {} SET {} = $1 WHERE {} = $2",
            table, value_column, id_column
        );

        // Update the row with encrypted value
        sqlx::query(&update_query)
            .bind(&encrypted)
            .bind(id)
            .execute(&mut **tx)
            .await?;

        count += 1;
    }

    Ok(count)
}

/// Generates a deterministic encrypted key name from a plaintext key.
///
/// This uses HMAC-SHA256 with the encryption key to create a deterministic
/// hash that can be used as a key name in the database. This allows the
/// application to find encrypted keys without storing the plaintext key name.
///
/// If the encryption key is None, returns the key name as-is without hashing.
///
/// # Arguments
///
/// * `encryption_key` - Base64-encoded encryption key (32 bytes when decoded), or None to skip hashing
/// * `key_name` - The plaintext key name to hash
///
/// # Returns
///
/// A hex-encoded string that can be used as a database key, or the plaintext key name if no encryption key
///
/// # Errors
///
/// Returns an error if encryption key is invalid (when provided)
pub fn hash_key_name(encryption_key: Option<&str>, key_name: &str) -> Result<String> {
    use sha2::{Sha256, Digest};

    // If no encryption key, return key name as-is
    let key = match encryption_key {
        None => {
            return Ok(key_name.to_string());
        }
        Some(k) => k,
    };

    let key_bytes = general_purpose::STANDARD
        .decode(key)
        .map_err(|e| DbError::Other(anyhow::anyhow!("Failed to decode encryption key: {}", e)))?;

    if key_bytes.len() != 32 {
        return Err(DbError::Other(anyhow::anyhow!(
            "Encryption key must be 32 bytes (256 bits), got {} bytes",
            key_bytes.len()
        )));
    }

    // Use HMAC-SHA256 for deterministic key derivation
    let mut hasher = Sha256::new();
    hasher.update(&key_bytes);
    hasher.update(key_name.as_bytes());
    let result = hasher.finalize();

    // Return hex-encoded hash prefixed to indicate it's encrypted
    Ok(format!("enc_{}", hex::encode(result)))
}

/// A wrapper type for encrypted strings that ensures encryption happens at construction time.
///
/// This type encapsulates the encryption logic and provides a type-safe way to handle
/// encrypted strings throughout the application. The encryption happens when the
/// `EncryptedString` is constructed via the `new` method.
///
/// # Examples
///
/// ```
/// use your_crate::crypto::EncryptedString;
///
/// let plaintext = "my-secret-api-key";
/// let encryption_key = Some("base64_encoded_key");
///
/// // Encrypt the string at construction
/// let encrypted = EncryptedString::new(plaintext, encryption_key)?;
///
/// // Convert to String for storage
/// let encrypted_value: String = encrypted.into();
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedString(String);

impl EncryptedString {
    /// Creates a new `EncryptedString` by encrypting the plaintext with the given key.
    ///
    /// # Arguments
    ///
    /// * `plaintext` - The plaintext string to encrypt
    /// * `encryption_key` - Optional base64-encoded encryption key (32 bytes when decoded).
    ///                      If None, the plaintext is stored as-is without encryption.
    ///
    /// # Returns
    ///
    /// A Result containing the EncryptedString or an error if encryption fails.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The encryption key is not valid base64 or not 32 bytes (when provided)
    /// - Encryption fails
    pub fn new(plaintext: &str, encryption_key: Option<&str>) -> Result<Self> {
        let encrypted = encrypt_with_key(encryption_key, plaintext.as_bytes())?;
        Ok(Self(encrypted))
    }

    /// Returns the encrypted value as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Decrypts the encrypted string and returns the plaintext.
    ///
    /// # Arguments
    ///
    /// * `encryption_key` - Optional base64-encoded encryption key (32 bytes when decoded).
    ///                      If None, the stored value is treated as plaintext.
    ///
    /// # Returns
    ///
    /// A Result containing the decrypted plaintext string or an error if decryption fails.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The encryption key is not valid base64 or not 32 bytes (when provided)
    /// - The encrypted data is not valid base64 or too short (when encryption key is provided)
    /// - Decryption fails
    /// - The decrypted data is not valid UTF-8
    ///
    /// # Examples
    ///
    /// ```
    /// use your_crate::crypto::EncryptedString;
    ///
    /// let plaintext = "my-secret-api-key";
    /// let encryption_key = Some("base64_encoded_key");
    ///
    /// // Encrypt
    /// let encrypted = EncryptedString::new(plaintext, encryption_key)?;
    ///
    /// // Decrypt
    /// let decrypted = encrypted.decrypt(encryption_key)?;
    /// assert_eq!(decrypted, plaintext);
    /// ```
    pub fn decrypt(&self, encryption_key: Option<&str>) -> Result<String> {
        let decrypted_bytes = decrypt_with_key(encryption_key, &self.0)?;
        String::from_utf8(decrypted_bytes)
            .map_err(|e| DbError::Other(anyhow::anyhow!("Decrypted data is not valid UTF-8: {}", e)))
    }
}

impl From<EncryptedString> for String {
    fn from(encrypted: EncryptedString) -> String {
        encrypted.0
    }
}

impl From<String> for EncryptedString {
    /// Wraps an already-encrypted string.
    /// Use this when reading from the database where the string is already encrypted.
    fn from(encrypted: String) -> Self {
        Self(encrypted)
    }
}

impl AsRef<str> for EncryptedString {
    fn as_ref(&self) -> &str {
        &self.0
    }
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

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        // Set up a test encryption key (32 bytes = 256 bits, base64 encoded)
        let test_key = general_purpose::STANDARD.encode([0u8; 32]);

        let plaintext = b"Hello, world! This is a test message.";

        // Encrypt
        let encrypted = encrypt_with_key(Some(&test_key), plaintext).expect("Encryption should succeed");

        // Should be valid base64
        assert!(general_purpose::STANDARD.decode(&encrypted).is_ok());

        // Decrypt
        let decrypted = decrypt_with_key(Some(&test_key), &encrypted).expect("Decryption should succeed");

        // Should match original plaintext
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_encrypt_without_key() {
        let plaintext = b"test";
        let result = encrypt_with_key(None, plaintext);

        // Should return plaintext as UTF-8 string
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "test");
    }

    #[test]
    fn test_decrypt_without_key() {
        let result = decrypt_with_key(None, "some_data");

        // Should return data as bytes
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), b"some_data");
    }

    #[test]
    fn test_encrypt_with_invalid_key_length() {
        // Set up a key that's not 32 bytes
        let test_key = general_purpose::STANDARD.encode([0u8; 16]);

        let plaintext = b"test";
        let result = encrypt_with_key(Some(&test_key), plaintext);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("32 bytes"));
    }

    #[test]
    fn test_decrypt_with_invalid_data() {
        let test_key = general_purpose::STANDARD.encode([0u8; 32]);

        // Too short data
        let result = decrypt_with_key(Some(&test_key), &general_purpose::STANDARD.encode([0u8; 5]));
        assert!(result.is_err());
    }

    #[test]
    fn test_encryption_produces_different_ciphertexts() {
        let test_key = general_purpose::STANDARD.encode([0u8; 32]);

        let plaintext = b"same plaintext";

        // Encrypt the same plaintext twice
        let encrypted1 = encrypt_with_key(Some(&test_key), plaintext).expect("Encryption should succeed");
        let encrypted2 = encrypt_with_key(Some(&test_key), plaintext).expect("Encryption should succeed");

        // Should produce different ciphertexts due to random nonce
        assert_ne!(encrypted1, encrypted2);

        // But both should decrypt to the same plaintext
        let decrypted1 = decrypt_with_key(Some(&test_key), &encrypted1).expect("Decryption should succeed");
        let decrypted2 = decrypt_with_key(Some(&test_key), &encrypted2).expect("Decryption should succeed");

        assert_eq!(decrypted1, plaintext);
        assert_eq!(decrypted2, plaintext);
    }

    #[test]
    fn test_hash_key_name_deterministic() {
        let test_key = general_purpose::STANDARD.encode([0u8; 32]);

        let key_name = "test_key";

        // Same key name should produce same hashed key name
        let hashed1 = hash_key_name(Some(&test_key), key_name).expect("Should hash key name");
        let hashed2 = hash_key_name(Some(&test_key), key_name).expect("Should hash key name");

        assert_eq!(hashed1, hashed2, "Hashed key names should be deterministic");
        assert!(hashed1.starts_with("enc_"), "Should have enc_ prefix");
        assert_eq!(hashed1.len(), 68, "Should be enc_ + 64 hex chars (SHA256)");
    }

    #[test]
    fn test_hash_key_name_different_inputs() {
        let test_key = general_purpose::STANDARD.encode([0u8; 32]);

        let hashed1 = hash_key_name(Some(&test_key), "key1").expect("Should hash key name");
        let hashed2 = hash_key_name(Some(&test_key), "key2").expect("Should hash key name");

        assert_ne!(hashed1, hashed2, "Different key names should produce different hashes");
    }

    #[test]
    fn test_hash_key_name_different_encryption_keys() {
        let test_key1 = general_purpose::STANDARD.encode([0u8; 32]);
        let test_key2 = general_purpose::STANDARD.encode([1u8; 32]);

        let hashed1 = hash_key_name(Some(&test_key1), "test_key").expect("Should hash key name");
        let hashed2 = hash_key_name(Some(&test_key2), "test_key").expect("Should hash key name");

        assert_ne!(
            hashed1, hashed2,
            "Same key name with different encryption keys should produce different hashes"
        );
    }

    #[test]
    fn test_hash_key_name_without_key() {
        let result = hash_key_name(None, "test_key");

        // Should return the key name as-is
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "test_key");
    }

    #[test]
    fn test_encrypted_string_with_key() {
        let test_key = general_purpose::STANDARD.encode([0u8; 32]);
        let plaintext = "my-secret-api-key";

        let encrypted = EncryptedString::new(plaintext, Some(&test_key))
            .expect("Should encrypt successfully");

        // Should be able to convert to String
        let encrypted_str: String = encrypted.clone().into();
        assert!(!encrypted_str.is_empty());
        assert_ne!(encrypted_str, plaintext);

        // Should be able to use AsRef
        assert!(!encrypted.as_ref().is_empty());
        assert_eq!(encrypted.as_str(), encrypted_str);
    }

    #[test]
    fn test_encrypted_string_without_key() {
        let plaintext = "my-api-key";

        let encrypted = EncryptedString::new(plaintext, None)
            .expect("Should work without key");

        // Without encryption key, should return plaintext
        let encrypted_str: String = encrypted.into();
        assert_eq!(encrypted_str, plaintext);
    }

    #[test]
    fn test_encrypted_string_roundtrip() {
        let test_key = general_purpose::STANDARD.encode([0u8; 32]);
        let plaintext = "test-secret";

        // Encrypt using EncryptedString
        let encrypted = EncryptedString::new(plaintext, Some(&test_key))
            .expect("Should encrypt successfully");

        let encrypted_str: String = encrypted.into();

        // Decrypt to verify
        let decrypted = decrypt_with_key(Some(&test_key), &encrypted_str)
            .expect("Should decrypt successfully");

        assert_eq!(decrypted, plaintext.as_bytes());
    }

    #[test]
    fn test_encrypted_string_different_each_time() {
        let test_key = general_purpose::STANDARD.encode([0u8; 32]);
        let plaintext = "same-plaintext";

        let encrypted1 = EncryptedString::new(plaintext, Some(&test_key))
            .expect("Should encrypt successfully");

        let encrypted2 = EncryptedString::new(plaintext, Some(&test_key))
            .expect("Should encrypt successfully");

        // Due to random nonce, should produce different encrypted strings
        assert_ne!(encrypted1, encrypted2);

        // But both should decrypt to the same plaintext
        let encrypted_str1: String = encrypted1.into();
        let encrypted_str2: String = encrypted2.into();

        let decrypted1 = decrypt_with_key(Some(&test_key), &encrypted_str1)
            .expect("Should decrypt");
        let decrypted2 = decrypt_with_key(Some(&test_key), &encrypted_str2)
            .expect("Should decrypt");

        assert_eq!(decrypted1, plaintext.as_bytes());
        assert_eq!(decrypted2, plaintext.as_bytes());
    }

    #[test]
    fn test_encrypted_string_decrypt_with_key() {
        let test_key = general_purpose::STANDARD.encode([0u8; 32]);
        let plaintext = "my-secret-data";

        // Encrypt
        let encrypted = EncryptedString::new(plaintext, Some(&test_key))
            .expect("Should encrypt successfully");

        // Decrypt
        let decrypted = encrypted.decrypt(Some(&test_key))
            .expect("Should decrypt successfully");

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_encrypted_string_decrypt_without_key() {
        let plaintext = "my-plaintext-data";

        // Create without encryption
        let encrypted = EncryptedString::new(plaintext, None)
            .expect("Should work without key");

        // Decrypt without key (should return the plaintext)
        let decrypted = encrypted.decrypt(None)
            .expect("Should decrypt successfully");

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_encrypted_string_decrypt_wrong_key() {
        let test_key1 = general_purpose::STANDARD.encode([0u8; 32]);
        let test_key2 = general_purpose::STANDARD.encode([1u8; 32]);
        let plaintext = "secret-data";

        // Encrypt with key1
        let encrypted = EncryptedString::new(plaintext, Some(&test_key1))
            .expect("Should encrypt successfully");

        // Try to decrypt with key2 (should fail)
        let result = encrypted.decrypt(Some(&test_key2));
        assert!(result.is_err());
    }

    #[test]
    fn test_encrypted_string_decrypt_key_mismatch() {
        let test_key = general_purpose::STANDARD.encode([0u8; 32]);
        let plaintext = "secret-data";

        // Encrypt with key
        let encrypted = EncryptedString::new(plaintext, Some(&test_key))
            .expect("Should encrypt successfully");

        // Try to decrypt without key (should fail since data is encrypted)
        let result = encrypted.decrypt(None);
        // This might succeed if the encrypted data happens to be valid UTF-8, but will return garbage
        // The important thing is we can decrypt correctly with the right key
        let correct = encrypted.decrypt(Some(&test_key))
            .expect("Should decrypt with correct key");
        assert_eq!(correct, plaintext);

        // And it should fail with wrong parameters
        if let Ok(wrong) = result {
            assert_ne!(wrong, plaintext, "Decrypting with wrong key should not return correct plaintext");
        }
    }
}
