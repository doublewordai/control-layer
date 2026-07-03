//! Flex-specific glue for zero-data-retention (ZDR) requests over the generic
//! [`crate::keystore`].
//!
//! A ZDR flex request has two per-request keys (one per body) held in the
//! keystore, and its stored bodies are self-describing: an encrypted body is
//! prefixed with [`ZDR_BODY_PREFIX`], which a plaintext body (always a serialized
//! JSON object, so it starts with `{`) can never collide with. That sentinel is
//! how dispatch and retrieve recognise a ZDR body without re-checking policy -
//! the body itself records whether it was encrypted.
//!
//! Policy (whether a new request is encrypted at all) is decided once, at submit,
//! by [`is_zdr_request`]. All crypto and storage lives in [`crate::keystore`].

use uuid::Uuid;

use crate::keystore::{self, KeystoreError};

/// Prefix marking a stored body as a ZDR ciphertext envelope. Chosen so it can
/// never be the start of a serialized JSON value (a plaintext body).
const ZDR_BODY_PREFIX: &str = "dwzdr1:";

/// TRANSITIONAL (dwctl ZDR): `batch_metadata` key the processor sets on a ZDR
/// dispatch. fusillade forwards `batch_metadata` entries as
/// `x-fusillade-batch-<key>` headers, so this surfaces on the loopback as
/// [`ZDR_MARKER_HEADER`], which the outlet handler reads to blank the
/// (already-decrypted) body. We piggyback the existing header channel to avoid
/// a fusillade API change; drop both when response reassembly moves into dwctl.
pub const ZDR_MARKER_KEY: &str = "zdr";

/// TRANSITIONAL (dwctl ZDR): the header [`ZDR_MARKER_KEY`] arrives as on the
/// loopback (fusillade prefixes `batch_metadata` keys with `x-fusillade-batch-`).
pub const ZDR_MARKER_HEADER: &str = "x-fusillade-batch-zdr";

/// Whether a request's key opts into ZDR. Decided once, at submit, from the
/// per-key policy map ([`crate::sync::zdr_keys`]); dispatch and retrieve instead
/// key off the body sentinel. A key absent from the map (deleted/invalid, which
/// auth rejects anyway) reads as non-ZDR, and a `None` key never opts in.
///
/// This answers per-key policy only. Callers that must encrypt (the flex path)
/// additionally require a configured keystore.
pub fn is_zdr_request(zdr_cache: &crate::sync::zdr_keys::ZdrKeyCache, api_key: Option<&str>) -> bool {
    api_key.is_some_and(|key| zdr_cache.is_zdr(key))
}

/// Prepare a ZDR flex request for storage: strip the control fields fusillade's
/// sanitiser would have removed (it cannot run on ciphertext), encrypt the body
/// with a fresh request key, and store both per-request keys. Returns the
/// sentinel-prefixed ciphertext to store as the request body.
pub async fn prepare_flex_submit(
    keystore: &crate::keystore::Keystore,
    request_id: &Uuid,
    request_value: &mut serde_json::Value,
) -> Result<String, KeystoreError> {
    if let Some(obj) = request_value.as_object_mut() {
        obj.remove("service_tier");
        obj.remove("background");
    }
    let request_key = keystore::generate_key();
    let response_key = keystore::generate_key();
    let body = encrypt_body(&request_key, &request_value.to_string())?;
    keystore.put(&key_id(request_id, KeyKind::Request), &request_key, None).await?;
    keystore.put(&key_id(request_id, KeyKind::Response), &response_key, None).await?;
    Ok(body)
}

/// Whether a stored body is a ZDR ciphertext envelope.
pub fn is_zdr_body(stored: &str) -> bool {
    stored.starts_with(ZDR_BODY_PREFIX)
}

/// Encrypt a body with a per-request key, producing a sentinel-prefixed,
/// self-describing envelope for storage in fusillade's opaque TEXT column.
pub fn encrypt_body(key: &[u8], plaintext: &str) -> Result<String, KeystoreError> {
    Ok(format!("{ZDR_BODY_PREFIX}{}", keystore::encrypt_value(key, plaintext)?))
}

/// Decrypt a body produced by [`encrypt_body`].
pub fn decrypt_body(key: &[u8], stored: &str) -> Result<String, KeystoreError> {
    let envelope = stored.strip_prefix(ZDR_BODY_PREFIX).ok_or(KeystoreError::MalformedEnvelope)?;
    keystore::decrypt_value(key, envelope)
}

/// Which body a per-request key protects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    Request,
    Response,
}

impl KeyKind {
    fn suffix(self) -> &'static str {
        match self {
            KeyKind::Request => "req",
            KeyKind::Response => "resp",
        }
    }
}

/// Keystore id for a flex request's per-request key, for example
/// `zdr:0e1f...:req`.
pub fn key_id(request_id: &Uuid, kind: KeyKind) -> String {
    format!("zdr:{}:{}", request_id, kind.suffix())
}

/// Outcome of [`decrypt_response_body`]: the resolved body to render, or the
/// signal that it is permanently gone.
#[derive(Debug, PartialEq, Eq)]
pub enum DecryptOutcome {
    /// The stored body was not a ZDR envelope (or was absent): render it as-is.
    Unchanged,
    /// A ZDR envelope that decrypted to this plaintext. The caller renders this
    /// in place of the stored ciphertext.
    Decrypted(String),
    /// A ZDR envelope whose key is gone (deleted on a prior retrieval, or
    /// TTL-expired). The plaintext is permanently unrecoverable; the caller must
    /// surface 410 or blank the body rather than the inert ciphertext.
    Gone,
}

/// Resolve the plaintext response body for a (possibly ZDR) request.
///
/// Given the request id and the stored `response_body`, returns the plaintext
/// to render ([`DecryptOutcome::Decrypted`]), a signal to render the stored body
/// untouched ([`DecryptOutcome::Unchanged`], for non-ZDR or absent bodies), or
/// [`DecryptOutcome::Gone`] when the envelope's key is missing. Used by the
/// retrieval render paths.
///
/// Crypto-shred on retrieval: a successful decrypt deletes the response key, so
/// the body is unrecoverable on any later read (a subsequent retrieval sees the
/// key gone and returns [`DecryptOutcome::Gone`]). The delete is best-effort -
/// if the keystore is briefly unreachable the plaintext is still returned this
/// once and the key's TTL remains the backstop.
pub async fn decrypt_response_body(
    keystore: &crate::keystore::Keystore,
    request_id: &Uuid,
    response_body: Option<&str>,
) -> Result<DecryptOutcome, KeystoreError> {
    let Some(body) = response_body else {
        return Ok(DecryptOutcome::Unchanged);
    };
    if !is_zdr_body(body) {
        return Ok(DecryptOutcome::Unchanged);
    }
    let response_key_id = key_id(request_id, KeyKind::Response);
    match keystore.get(&response_key_id).await? {
        Some(key) => {
            let plaintext = decrypt_body(&key, body)?;
            // Shred on retrieval (plan: "Deleted on retrieval. TTL is the
            // backstop."). Best-effort: a failed delete still returns the body
            // and leans on the key's TTL to shred it later.
            if let Err(e) = keystore.delete(&response_key_id).await {
                tracing::warn!(error = %e, "ZDR response key delete-on-retrieval failed; relying on TTL");
            }
            Ok(DecryptOutcome::Decrypted(plaintext))
        }
        None => Ok(DecryptOutcome::Gone),
    }
}

/// TRANSITIONAL: encrypts ZDR response/error bodies at rest via fusillade's
/// `ResponseTransformer` hook. Exists only because fusillade reassembles the
/// upstream stream and persists the body itself, leaving dwctl no other point at
/// which to encrypt it. Remove when stream reassembly moves into dwctl. See
/// [`crate::keystore`] and fusillade's `transform` module.
pub struct ZdrResponseEncryptor {
    keystore: crate::keystore::Keystore,
}

impl ZdrResponseEncryptor {
    pub fn new(keystore: crate::keystore::Keystore) -> Self {
        Self { keystore }
    }
}

#[async_trait::async_trait]
impl fusillade::ResponseTransformer for ZdrResponseEncryptor {
    async fn transform(&self, request: &fusillade::RequestData, body: &str) -> fusillade::Result<String> {
        // Authoritative ZDR signal: the marker the processor stamped on
        // batch_metadata at dispatch, which rides through to persist. We cannot
        // key off the stored request-body sentinel here - by persist time the
        // request body has already been decrypted to plaintext for dispatch - nor
        // off response-key presence alone, which fails open if the key expired.
        let is_zdr = request.batch_metadata.get(ZDR_MARKER_KEY).is_some_and(|v| v == "1");
        if !is_zdr {
            return Ok(body.to_string());
        }
        let request_id = &request.id.0;
        match self.keystore.get(&key_id(request_id, KeyKind::Response)).await {
            Ok(Some(key)) => {
                let encrypted = encrypt_body(&key, body)
                    .map_err(|e| fusillade::FusilladeError::Other(anyhow::anyhow!("ZDR response encrypt failed: {e}")))?;
                // This is the terminal store (the daemon only reaches persist() on
                // a terminal outcome; retriable-and-will-retry failures reschedule
                // to pending instead), so the prompt is done being dispatched -
                // crypto-shred the request key now (plan: "Deleted when a terminal
                // response is stored"). Best-effort: the key's TTL is the backstop.
                if let Err(e) = self.keystore.delete(&key_id(request_id, KeyKind::Request)).await {
                    tracing::warn!(error = %e, "ZDR request key shred-on-terminal failed; relying on TTL");
                }
                Ok(encrypted)
            }
            // Fail closed on a definitively-absent key: this IS a ZDR request
            // but its response key is gone (TTL expired/deleted), so there is no
            // key to encrypt with. We must not persist the plaintext - but
            // erroring here would bubble up as a persist failure that the daemon
            // does NOT terminalize, stranding the request in `processing` forever.
            // So blank the body instead: no plaintext reaches the DB and the
            // request completes cleanly (the response is simply not retained).
            // With a TTL sized above the flex completion window this is unreachable.
            Ok(None) => {
                tracing::warn!(request_id = %request_id, "ZDR response key gone at persist; storing blank (response not retained)");
                Ok(String::new())
            }
            // Unreachable keystore is transient, not authoritative: propagate the
            // error rather than blank, so a momentary Redis blip does not
            // permanently drop a response that could still be encrypted on retry.
            Err(e) => Err(fusillade::FusilladeError::Other(anyhow::anyhow!(
                "ZDR keystore error during response encrypt: {e}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_ids_are_distinct_per_kind() {
        let id = Uuid::nil();
        let req = key_id(&id, KeyKind::Request);
        let resp = key_id(&id, KeyKind::Response);
        assert_ne!(req, resp);
        assert!(req.ends_with(":req"));
        assert!(resp.ends_with(":resp"));
        assert!(req.starts_with("zdr:"));
    }

    #[test]
    fn body_envelope_is_self_describing() {
        let key = keystore::generate_key();
        let plaintext = r#"{"model":"gpt","input":"hello"}"#;
        let envelope = encrypt_body(&key, plaintext).unwrap();
        // A real plaintext body is a JSON object, never prefixed.
        assert!(!is_zdr_body(plaintext));
        assert!(is_zdr_body(&envelope));
        assert_eq!(decrypt_body(&key, &envelope).unwrap(), plaintext);
    }

    #[test]
    fn decrypt_body_rejects_unprefixed() {
        let key = keystore::generate_key();
        assert!(matches!(decrypt_body(&key, "{}"), Err(KeystoreError::MalformedEnvelope)));
    }

    #[test]
    fn is_zdr_request_reads_the_key_map() {
        let cache = crate::sync::zdr_keys::ZdrKeyCache::from_pairs([("sk-on".to_string(), true), ("sk-off".to_string(), false)]);
        assert!(is_zdr_request(&cache, Some("sk-on")));
        assert!(!is_zdr_request(&cache, Some("sk-off")));
        // Absent key (deleted/invalid, auth-rejected) reads as non-ZDR.
        assert!(!is_zdr_request(&cache, Some("sk-unknown")));
        // No key never opts in.
        assert!(!is_zdr_request(&cache, None));
    }
}
