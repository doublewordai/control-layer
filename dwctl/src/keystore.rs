//! Encrypted key custody: a Redis-backed store for small secrets (today, the
//! per-request keys that decrypt zero-data-retention flex bodies; potentially
//! other wrapped keys later, such as connection credentials).
//!
//! Each stored value is wrapped by a rotatable wrap keyring before it is written
//! to Redis, so a Redis snapshot or memory dump is useless on its own. Values
//! are keyed by an arbitrary string id chosen by the caller and expire after a
//! TTL, so deleting or expiring a value crypto-shreds whatever it protected.
//!
//! Two key tiers:
//! - Stored values (per-request keys, etc.): the caller's secrets, wrapped and
//!   held under a caller-chosen id with a TTL.
//! - A wrap keyring from config, held as id-to-key so it can be rotated: each
//!   wrapped value is tagged with the id that sealed it; we wrap with the current
//!   key and unwrap by the recorded id. To rotate, add a new current key and keep
//!   the previous key(s) for at least one TTL so in-flight values still unwrap,
//!   then retire the old key once that window has passed.
//!
//! The AES-256-GCM primitive is reused from [`crate::encryption`]; nothing here
//! reimplements crypto.

use std::collections::HashMap;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose};
use deadpool_redis::{Config as RedisConfig, Pool, Runtime};
use rand::prelude::RngExt;
use rand::rng;
use serde::{Deserialize, Serialize};

use crate::encryption::{self, EncryptionError};

/// Value-envelope format version, prepended before the AES-GCM blob so the
/// format can evolve. Bumped only on an incompatible change.
const VALUE_ENVELOPE_VERSION: u8 = 1;

/// Errors from the keystore.
///
/// Callers distinguish three outcomes when fetching: `Ok(None)` (absent: deleted
/// or expired, a definitive gone result), `Err(Unreachable)` (transient transport
/// problem: retry, do not treat as gone), and any other `Err` (crypto/config
/// problem: terminal, do not retry).
#[derive(Debug, thiserror::Error)]
pub enum KeystoreError {
    #[error("crypto error: {0}")]
    Crypto(#[from] EncryptionError),

    #[error("unknown wrap-key id {0:?}; cannot unwrap (key retired too early?)")]
    UnknownWrapKeyId(String),

    #[error("malformed wrapped value in keystore")]
    MalformedWrappedValue,

    #[error("malformed value envelope")]
    MalformedEnvelope,

    #[error("keystore unreachable: {0}")]
    Unreachable(String),

    #[error("keystore configuration error: {0}")]
    Config(String),
}

impl KeystoreError {
    /// True when the failure is a transient transport problem and the caller
    /// should retry rather than treat the value as gone.
    pub fn is_unreachable(&self) -> bool {
        matches!(self, KeystoreError::Unreachable(_))
    }
}

/// Generate a fresh 32-byte key.
pub fn generate_key() -> [u8; 32] {
    let mut key = [0u8; 32];
    rng().fill(&mut key);
    key
}

/// A rotatable set of wrap keys, each identified by a short id. New values are
/// sealed with `current_id`; existing values are opened with the id recorded in
/// their envelope, so several wrap-key versions can be valid at once.
#[derive(Clone)]
pub struct WrapKeyring {
    current_id: String,
    keys: HashMap<String, Vec<u8>>,
}

// Manual Debug so key material is never printed; only the ids are shown.
impl std::fmt::Debug for WrapKeyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WrapKeyring")
            .field("current_id", &self.current_id)
            .field("key_ids", &self.keys.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl WrapKeyring {
    /// Build a keyring. `current_id` must be present in `keys`, and every id must
    /// fit in a single length byte (at most 255 bytes).
    pub fn new(current_id: String, keys: HashMap<String, Vec<u8>>) -> Result<Self, KeystoreError> {
        if !keys.contains_key(&current_id) {
            return Err(KeystoreError::Config(format!(
                "current wrap-key id {current_id:?} is not present in the keyring"
            )));
        }
        if let Some(id) = keys.keys().find(|id| id.len() > u8::MAX as usize) {
            return Err(KeystoreError::Config(format!("wrap-key id {id:?} is too long (max 255 bytes)")));
        }
        Ok(Self { current_id, keys })
    }

    /// Wrap a value with the current wrap key. Output layout:
    /// `id_len (1) || id || encryption::encrypt(wrap_key, value)`.
    pub fn wrap(&self, value: &[u8]) -> Result<Vec<u8>, KeystoreError> {
        let wrap_key = self
            .keys
            .get(&self.current_id)
            .ok_or_else(|| KeystoreError::Config("current wrap key missing from keyring".into()))?;
        let sealed = encryption::encrypt(wrap_key, value)?;
        let id = self.current_id.as_bytes();
        let mut out = Vec::with_capacity(1 + id.len() + sealed.len());
        out.push(id.len() as u8);
        out.extend_from_slice(id);
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    /// Unwrap a blob produced by [`WrapKeyring::wrap`], selecting the wrap key by
    /// the id tag it carries.
    pub fn unwrap(&self, blob: &[u8]) -> Result<Vec<u8>, KeystoreError> {
        let (&id_len, rest) = blob.split_first().ok_or(KeystoreError::MalformedWrappedValue)?;
        let id_len = id_len as usize;
        if rest.len() < id_len {
            return Err(KeystoreError::MalformedWrappedValue);
        }
        let (id_bytes, sealed) = rest.split_at(id_len);
        let id = std::str::from_utf8(id_bytes).map_err(|_| KeystoreError::MalformedWrappedValue)?;
        let wrap_key = self.keys.get(id).ok_or_else(|| KeystoreError::UnknownWrapKeyId(id.to_string()))?;
        Ok(encryption::decrypt(wrap_key, sealed)?)
    }
}

/// Encrypt a string value with a key, returning a base64 string suitable for a
/// TEXT column. Pre-base64 layout: `version (1) || nonce || ciphertext || tag`.
pub fn encrypt_value(key: &[u8], plaintext: &str) -> Result<String, KeystoreError> {
    let blob = encryption::encrypt(key, plaintext.as_bytes())?;
    let mut framed = Vec::with_capacity(1 + blob.len());
    framed.push(VALUE_ENVELOPE_VERSION);
    framed.extend_from_slice(&blob);
    Ok(general_purpose::STANDARD.encode(framed))
}

/// Decrypt a value produced by [`encrypt_value`].
pub fn decrypt_value(key: &[u8], ciphertext: &str) -> Result<String, KeystoreError> {
    let framed = general_purpose::STANDARD
        .decode(ciphertext)
        .map_err(|_| KeystoreError::MalformedEnvelope)?;
    let (&version, blob) = framed.split_first().ok_or(KeystoreError::MalformedEnvelope)?;
    if version != VALUE_ENVELOPE_VERSION {
        return Err(KeystoreError::MalformedEnvelope);
    }
    let plaintext = encryption::decrypt(key, blob)?;
    String::from_utf8(plaintext).map_err(|_| KeystoreError::MalformedEnvelope)
}

/// Configuration for the keystore. The wrap keys come from a Kubernetes secret.
/// `default_ttl_seconds` is the TTL applied to `put`s that do not specify one;
/// for ZDR flex it must exceed the longest completion window plus a retrieval
/// grace, or in-flight requests self-destruct before processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeystoreConfig {
    /// Redis connection URL for the keystore.
    pub redis_url: String,
    /// Default value time-to-live, in seconds. Optional: defaults to
    /// [`default_keystore_ttl_seconds`] so a deployment can enable the keystore
    /// without pinning a TTL. Set it above the longest flex completion window
    /// plus a retrieval grace.
    #[serde(default = "default_keystore_ttl_seconds")]
    pub default_ttl_seconds: u64,
    /// Id of the wrap key used to seal new values.
    pub current_wrap_key_id: String,
    /// Wrap-key secrets by id; each is run through
    /// [`crate::encryption::derive_encryption_key`].
    pub wrap_keys: HashMap<String, String>,
}

/// Default keystore TTL (2 hours): comfortably above the 1h async completion
/// window plus a retrieval grace, so in-flight keys do not expire mid-request.
fn default_keystore_ttl_seconds() -> u64 {
    7200
}

/// The Redis-backed keystore plus the wrap keyring. Cheap to clone (the pool and
/// keyring are shared).
#[derive(Clone)]
pub struct Keystore {
    pool: Pool,
    keyring: WrapKeyring,
    default_ttl: Duration,
}

impl Keystore {
    /// Build from config: derive the keyring and create the Redis pool.
    pub fn from_config(cfg: &KeystoreConfig) -> Result<Self, KeystoreError> {
        let keys = cfg
            .wrap_keys
            .iter()
            .map(|(id, secret)| (id.clone(), encryption::derive_encryption_key(secret)))
            .collect::<HashMap<_, _>>();
        let keyring = WrapKeyring::new(cfg.current_wrap_key_id.clone(), keys)?;
        let pool = RedisConfig::from_url(cfg.redis_url.clone())
            .create_pool(Some(Runtime::Tokio1))
            .map_err(|e| KeystoreError::Config(format!("failed to create redis pool: {e}")))?;
        Ok(Self {
            pool,
            keyring,
            default_ttl: Duration::from_secs(cfg.default_ttl_seconds),
        })
    }

    /// The configured default TTL, applied when `put` is called with `None`.
    pub fn default_ttl(&self) -> Duration {
        self.default_ttl
    }

    /// Store a value, wrapped, under `id`. `ttl` overrides the configured default.
    pub async fn put(&self, id: &str, value: &[u8], ttl: Option<Duration>) -> Result<(), KeystoreError> {
        let wrapped = self.keyring.wrap(value)?;
        let secs = ttl.unwrap_or(self.default_ttl).as_secs();
        let mut conn = self.pool.get().await.map_err(|e| KeystoreError::Unreachable(e.to_string()))?;
        deadpool_redis::redis::cmd("SET")
            .arg(id)
            .arg(wrapped)
            .arg("EX")
            .arg(secs)
            .query_async::<()>(&mut conn)
            .await
            .map_err(|e| KeystoreError::Unreachable(e.to_string()))?;
        Ok(())
    }

    /// Fetch and unwrap a value.
    ///
    /// `Ok(None)` means the value is absent (deleted or expired): a definitive
    /// gone result. `Err(Unreachable)` is a transport problem the caller should
    /// retry. Any other `Err` is a terminal crypto/config failure.
    pub async fn get(&self, id: &str) -> Result<Option<Vec<u8>>, KeystoreError> {
        let mut conn = self.pool.get().await.map_err(|e| KeystoreError::Unreachable(e.to_string()))?;
        let wrapped: Option<Vec<u8>> = deadpool_redis::redis::cmd("GET")
            .arg(id)
            .query_async(&mut conn)
            .await
            .map_err(|e| KeystoreError::Unreachable(e.to_string()))?;
        match wrapped {
            None => Ok(None),
            Some(blob) => Ok(Some(self.keyring.unwrap(&blob)?)),
        }
    }

    /// Delete a value, crypto-shredding whatever it protected.
    pub async fn delete(&self, id: &str) -> Result<(), KeystoreError> {
        let mut conn = self.pool.get().await.map_err(|e| KeystoreError::Unreachable(e.to_string()))?;
        deadpool_redis::redis::cmd("DEL")
            .arg(id)
            .query_async::<()>(&mut conn)
            .await
            .map_err(|e| KeystoreError::Unreachable(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keyring() -> WrapKeyring {
        let mut keys = HashMap::new();
        keys.insert("k1".to_string(), encryption::derive_encryption_key("secret-one"));
        keys.insert("k2".to_string(), encryption::derive_encryption_key("secret-two"));
        WrapKeyring::new("k2".to_string(), keys).unwrap()
    }

    #[test]
    fn wrap_unwrap_roundtrip() {
        let kr = keyring();
        let value = generate_key();
        let wrapped = kr.wrap(&value).unwrap();
        assert_ne!(wrapped.as_slice(), value.as_slice());
        assert_eq!(kr.unwrap(&wrapped).unwrap(), value.to_vec());
    }

    #[test]
    fn unwrap_with_old_key_after_rotation_still_works() {
        // Seal with k2 (current), then rotate so k3 is current but k2 is kept in
        // the keyring during the overlap window.
        let sealed = keyring().wrap(&generate_key()).unwrap();
        let mut keys = HashMap::new();
        keys.insert("k2".to_string(), encryption::derive_encryption_key("secret-two"));
        keys.insert("k3".to_string(), encryption::derive_encryption_key("secret-three"));
        let rotated = WrapKeyring::new("k3".to_string(), keys).unwrap();
        assert!(rotated.unwrap(&sealed).is_ok());
    }

    #[test]
    fn unwrap_after_old_key_retired_errors() {
        let sealed = keyring().wrap(&generate_key()).unwrap();
        // Keyring no longer contains k2 (retired after the TTL window).
        let mut keys = HashMap::new();
        keys.insert("k3".to_string(), encryption::derive_encryption_key("secret-three"));
        let retired = WrapKeyring::new("k3".to_string(), keys).unwrap();
        assert!(matches!(retired.unwrap(&sealed).unwrap_err(), KeystoreError::UnknownWrapKeyId(_)));
    }

    #[test]
    fn value_roundtrip() {
        let key = generate_key();
        let value = r#"{"model":"gpt","input":"hello"}"#;
        let ciphertext = encrypt_value(&key, value).unwrap();
        assert_ne!(ciphertext, value);
        assert_eq!(decrypt_value(&key, &ciphertext).unwrap(), value);
    }

    #[test]
    fn value_decrypt_with_wrong_key_fails() {
        let ciphertext = encrypt_value(&generate_key(), "secret prompt").unwrap();
        assert!(decrypt_value(&generate_key(), &ciphertext).is_err());
    }

    #[test]
    fn current_id_must_be_in_keyring() {
        let mut keys = HashMap::new();
        keys.insert("a".to_string(), encryption::derive_encryption_key("x"));
        assert!(matches!(
            WrapKeyring::new("missing".to_string(), keys).unwrap_err(),
            KeystoreError::Config(_)
        ));
    }
}
