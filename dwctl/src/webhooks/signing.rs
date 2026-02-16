//! HMAC-SHA256 signing for Standard Webhooks compliance.
//!
//! Standard Webhooks uses the following signature scheme:
//! - Signature is computed over: `{msg_id}.{timestamp}.{payload}`
//! - The signature is base64-encoded HMAC-SHA256
//! - Headers include: `webhook-id`, `webhook-timestamp`, `webhook-signature`
//!
//! See: <https://www.standardwebhooks.com/>

use base64::{Engine, engine::general_purpose::STANDARD as BASE64_STANDARD};
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Prefix for webhook secrets
pub const SECRET_PREFIX: &str = "whsec_";

/// Generate a new webhook secret.
///
/// Returns a `whsec_` prefixed base64-encoded 32-byte random secret.
pub fn generate_secret() -> String {
    use rand::RngCore;

    let mut secret_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut secret_bytes);

    format!("{}{}", SECRET_PREFIX, BASE64_STANDARD.encode(secret_bytes))
}

/// Extract the raw secret bytes from a `whsec_` prefixed secret.
///
/// Returns `None` if the secret doesn't have the correct prefix or invalid base64.
pub fn decode_secret(secret: &str) -> Option<Vec<u8>> {
    let encoded = secret.strip_prefix(SECRET_PREFIX)?;
    BASE64_STANDARD.decode(encoded).ok()
}

/// Sign a webhook payload according to Standard Webhooks spec.
///
/// The signature is computed over: `{msg_id}.{timestamp}.{payload}`
///
/// # Arguments
///
/// * `msg_id` - The unique message ID (webhook-id header)
/// * `timestamp` - Unix timestamp in seconds
/// * `payload` - The JSON payload body
/// * `secret` - The `whsec_` prefixed secret
///
/// # Returns
///
/// The signature in format `v1,{base64-hmac-sha256}`
pub fn sign_payload(msg_id: &str, timestamp: i64, payload: &str, secret: &str) -> Option<String> {
    let secret_bytes = decode_secret(secret)?;

    let signed_content = format!("{}.{}.{}", msg_id, timestamp, payload);

    let mut mac = HmacSha256::new_from_slice(&secret_bytes).ok()?;
    mac.update(signed_content.as_bytes());
    let signature = mac.finalize().into_bytes();

    Some(format!("v1,{}", BASE64_STANDARD.encode(signature)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_secret() {
        let secret = generate_secret();
        assert!(secret.starts_with(SECRET_PREFIX));

        // Should be able to decode
        let decoded = decode_secret(&secret);
        assert!(decoded.is_some());
        assert_eq!(decoded.unwrap().len(), 32);
    }

    #[test]
    fn test_decode_secret_invalid_prefix() {
        assert!(decode_secret("invalid_secret").is_none());
    }

    #[test]
    fn test_decode_secret_invalid_base64() {
        assert!(decode_secret("whsec_not-valid-base64!!!").is_none());
    }

    #[test]
    fn test_sign_payload() {
        let secret = generate_secret();
        let msg_id = "msg_123";
        let timestamp = 1704067200; // 2024-01-01 00:00:00 UTC
        let payload = r#"{"type":"batch.completed","data":{}}"#;

        let signature = sign_payload(msg_id, timestamp, payload, &secret).expect("should sign");
        assert!(signature.starts_with("v1,"));
    }

    #[test]
    fn test_sign_payload_deterministic() {
        let secret = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
        let msg_id = "msg_p5jXN8AQM9LWM0D4loKWxJek";
        let timestamp = 1614265330;
        let payload = r#"{"test": 2432232314}"#;

        let sig1 = sign_payload(msg_id, timestamp, payload, secret).expect("should sign");
        let sig2 = sign_payload(msg_id, timestamp, payload, secret).expect("should sign");
        assert_eq!(sig1, sig2);
    }
}
