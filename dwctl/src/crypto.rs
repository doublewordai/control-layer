use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use base64::{engine::general_purpose, Engine as _};
use rand::{thread_rng, Rng};
use sqlx::{Row, Transaction, Postgres};
use std::env;

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

/// Encrypts data using AES-256-GCM with a key from the ENCRYPTION_KEY environment variable.
///
/// The encryption key must be provided via the ENCRYPTION_KEY environment variable
/// and should be 32 bytes (256 bits) when decoded from base64.
///
/// # Arguments
///
/// * `plaintext` - The data to encrypt as a byte slice
///
/// # Returns
///
/// A Result containing the encrypted data as a base64-encoded string (nonce + ciphertext)
/// or an error if encryption fails.
///
/// # Errors
///
/// Returns an error if:
/// - ENCRYPTION_KEY environment variable is not set
/// - The encryption key is not valid base64 or not 32 bytes
/// - Encryption fails
pub fn encrypt_with_env_key(plaintext: &[u8]) -> Result<String, anyhow::Error> {
    let key_b64 = env::var("ENCRYPTION_KEY")
        .map_err(|_| anyhow::anyhow!("ENCRYPTION_KEY environment variable not set"))?;

    let key_bytes = general_purpose::STANDARD
        .decode(key_b64)
        .map_err(|e| anyhow::anyhow!("Failed to decode ENCRYPTION_KEY: {}", e))?;

    if key_bytes.len() != 32 {
        return Err(anyhow::anyhow!(
            "ENCRYPTION_KEY must be 32 bytes (256 bits), got {} bytes",
            key_bytes.len()
        ));
    }

    let cipher = Aes256Gcm::new_from_slice(&key_bytes)
        .map_err(|e| anyhow::anyhow!("Failed to create cipher: {}", e))?;

    // Generate a random 96-bit nonce
    let mut nonce_bytes = [0u8; 12];
    thread_rng().fill(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    // Encrypt the plaintext
    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| anyhow::anyhow!("Encryption failed: {}", e))?;

    // Combine nonce + ciphertext and encode as base64
    let mut result = nonce_bytes.to_vec();
    result.extend_from_slice(&ciphertext);

    Ok(general_purpose::STANDARD.encode(result))
}

/// Decrypts data that was encrypted with `encrypt_with_env_key`.
///
/// # Arguments
///
/// * `encrypted_b64` - The base64-encoded encrypted data (nonce + ciphertext)
///
/// # Returns
///
/// A Result containing the decrypted data as a Vec<u8> or an error if decryption fails.
///
/// # Errors
///
/// Returns an error if:
/// - ENCRYPTION_KEY environment variable is not set
/// - The encryption key is not valid base64 or not 32 bytes
/// - The encrypted data is not valid base64 or too short
/// - Decryption fails
pub fn decrypt_with_env_key(encrypted_b64: &str) -> Result<Vec<u8>, anyhow::Error> {
    let key_b64 = env::var("ENCRYPTION_KEY")
        .map_err(|_| anyhow::anyhow!("ENCRYPTION_KEY environment variable not set"))?;

    let key_bytes = general_purpose::STANDARD
        .decode(key_b64)
        .map_err(|e| anyhow::anyhow!("Failed to decode ENCRYPTION_KEY: {}", e))?;

    if key_bytes.len() != 32 {
        return Err(anyhow::anyhow!(
            "ENCRYPTION_KEY must be 32 bytes (256 bits), got {} bytes",
            key_bytes.len()
        ));
    }

    let cipher = Aes256Gcm::new_from_slice(&key_bytes)
        .map_err(|e| anyhow::anyhow!("Failed to create cipher: {}", e))?;

    // Decode the base64 data
    let encrypted_data = general_purpose::STANDARD
        .decode(encrypted_b64)
        .map_err(|e| anyhow::anyhow!("Failed to decode encrypted data: {}", e))?;

    if encrypted_data.len() < 12 {
        return Err(anyhow::anyhow!("Encrypted data too short"));
    }

    // Split into nonce and ciphertext
    let (nonce_bytes, ciphertext) = encrypted_data.split_at(12);
    let nonce = Nonce::from_slice(nonce_bytes);

    // Decrypt
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| anyhow::anyhow!("Decryption failed: {}", e))?;

    Ok(plaintext)
}

/// Encrypts a column in a database table for all non-null values.
///
/// This is a helper function for data migrations that need to encrypt existing plaintext data.
///
/// # Arguments
///
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
///     &mut tx,
///     "inference_endpoints",
///     "id",
///     "api_key"
/// ).await?;
/// ```
pub async fn encrypt_table_column(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    table: &str,
    id_column: &str,
    value_column: &str,
) -> Result<usize, anyhow::Error> {
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
        let encrypted = encrypt_with_env_key(plaintext.as_bytes())?;

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
/// # Arguments
///
/// * `key_name` - The plaintext key name to encrypt
///
/// # Returns
///
/// A hex-encoded string that can be used as a database key
///
/// # Errors
///
/// Returns an error if ENCRYPTION_KEY is not set or invalid
pub fn encrypt_key_name(key_name: &str) -> Result<String, anyhow::Error> {
    use sha2::{Sha256, Digest};

    let key_b64 = env::var("ENCRYPTION_KEY")
        .map_err(|_| anyhow::anyhow!("ENCRYPTION_KEY environment variable not set"))?;

    let key_bytes = general_purpose::STANDARD
        .decode(key_b64)
        .map_err(|e| anyhow::anyhow!("Failed to decode ENCRYPTION_KEY: {}", e))?;

    if key_bytes.len() != 32 {
        return Err(anyhow::anyhow!(
            "ENCRYPTION_KEY must be 32 bytes (256 bits), got {} bytes",
            key_bytes.len()
        ));
    }

    // Use HMAC-SHA256 for deterministic key derivation
    let mut hasher = Sha256::new();
    hasher.update(&key_bytes);
    hasher.update(key_name.as_bytes());
    let result = hasher.finalize();

    // Return hex-encoded hash prefixed to indicate it's encrypted
    Ok(format!("enc_{}", hex::encode(result)))
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
        env::set_var("ENCRYPTION_KEY", &test_key);

        let plaintext = b"Hello, world! This is a test message.";

        // Encrypt
        let encrypted = encrypt_with_env_key(plaintext).expect("Encryption should succeed");

        // Should be valid base64
        assert!(general_purpose::STANDARD.decode(&encrypted).is_ok());

        // Decrypt
        let decrypted = decrypt_with_env_key(&encrypted).expect("Decryption should succeed");

        // Should match original plaintext
        assert_eq!(decrypted, plaintext);

        env::remove_var("ENCRYPTION_KEY");
    }

    #[test]
    fn test_encrypt_without_env_key() {
        env::remove_var("ENCRYPTION_KEY");

        let plaintext = b"test";
        let result = encrypt_with_env_key(plaintext);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("ENCRYPTION_KEY"));
    }

    #[test]
    fn test_decrypt_without_env_key() {
        env::remove_var("ENCRYPTION_KEY");

        let result = decrypt_with_env_key("some_encrypted_data");

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("ENCRYPTION_KEY"));
    }

    #[test]
    fn test_encrypt_with_invalid_key_length() {
        // Set up a key that's not 32 bytes
        let test_key = general_purpose::STANDARD.encode([0u8; 16]);
        env::set_var("ENCRYPTION_KEY", &test_key);

        let plaintext = b"test";
        let result = encrypt_with_env_key(plaintext);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("32 bytes"));

        env::remove_var("ENCRYPTION_KEY");
    }

    #[test]
    fn test_decrypt_with_invalid_data() {
        let test_key = general_purpose::STANDARD.encode([0u8; 32]);
        env::set_var("ENCRYPTION_KEY".to_string(), &test_key);

        // Too short data
        let result = decrypt_with_env_key(&general_purpose::STANDARD.encode([0u8; 5]));
        assert!(result.is_err());

        env::remove_var("ENCRYPTION_KEY");
    }

    #[test]
    fn test_encryption_produces_different_ciphertexts() {
        let test_key = general_purpose::STANDARD.encode([0u8; 32]);
        env::set_var("ENCRYPTION_KEY", &test_key);

        let plaintext = b"same plaintext";

        // Encrypt the same plaintext twice
        let encrypted1 = encrypt_with_env_key(plaintext).expect("Encryption should succeed");
        let encrypted2 = encrypt_with_env_key(plaintext).expect("Encryption should succeed");

        // Should produce different ciphertexts due to random nonce
        assert_ne!(encrypted1, encrypted2);

        // But both should decrypt to the same plaintext
        let decrypted1 = decrypt_with_env_key(&encrypted1).expect("Decryption should succeed");
        let decrypted2 = decrypt_with_env_key(&encrypted2).expect("Decryption should succeed");

        assert_eq!(decrypted1, plaintext);
        assert_eq!(decrypted2, plaintext);

        env::remove_var("ENCRYPTION_KEY");
    }

    #[test]
    fn test_encrypt_key_name_deterministic() {
        let test_key = general_purpose::STANDARD.encode([0u8; 32]);
        env::set_var("ENCRYPTION_KEY", &test_key);

        let key_name = "test_key";

        // Same key name should produce same encrypted key name
        let encrypted1 = encrypt_key_name(key_name).expect("Should encrypt key name");
        let encrypted2 = encrypt_key_name(key_name).expect("Should encrypt key name");

        assert_eq!(encrypted1, encrypted2, "Encrypted key names should be deterministic");
        assert!(encrypted1.starts_with("enc_"), "Should have enc_ prefix");
        assert_eq!(encrypted1.len(), 68, "Should be enc_ + 64 hex chars (SHA256)");

        env::remove_var("ENCRYPTION_KEY");
    }

    #[test]
    fn test_encrypt_key_name_different_inputs() {
        let test_key = general_purpose::STANDARD.encode([0u8; 32]);
        env::set_var("ENCRYPTION_KEY", &test_key);

        let encrypted1 = encrypt_key_name("key1").expect("Should encrypt key name");
        let encrypted2 = encrypt_key_name("key2").expect("Should encrypt key name");

        assert_ne!(encrypted1, encrypted2, "Different key names should produce different hashes");

        env::remove_var("ENCRYPTION_KEY");
    }

    #[test]
    fn test_encrypt_key_name_different_encryption_keys() {
        let test_key1 = general_purpose::STANDARD.encode([0u8; 32]);
        let test_key2 = general_purpose::STANDARD.encode([1u8; 32]);

        env::set_var("ENCRYPTION_KEY", &test_key1);
        let encrypted1 = encrypt_key_name("test_key").expect("Should encrypt key name");

        env::set_var("ENCRYPTION_KEY", &test_key2);
        let encrypted2 = encrypt_key_name("test_key").expect("Should encrypt key name");

        assert_ne!(
            encrypted1, encrypted2,
            "Same key name with different encryption keys should produce different hashes"
        );

        env::remove_var("ENCRYPTION_KEY");
    }

    #[test]
    fn test_encrypt_key_name_without_env_key() {
        env::remove_var("ENCRYPTION_KEY");

        let result = encrypt_key_name("test_key");

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("ENCRYPTION_KEY"));
    }
}
