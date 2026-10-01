//! Opaque token used to reference an image stored in our object store.
//!
//! Tokens are stored in request bodies in place of the original
//! user-supplied URL / data URI. They never reach an upstream provider —
//! the dispatcher resolves them to a fresh signed URL just before sending.
//!
//! The token format is `dw-img://{sha256-hex}.{nonce-hex}`:
//!
//! - `sha256` is the SHA-256 of the image bytes. It is the image's
//!   *identity*: access grants (`image_access`) are keyed on it, and the
//!   prompt cache hashes tokens with the nonce removed
//!   ([`ImageToken::strip_storage_nonces`]), so the same image submitted in
//!   different requests still shares cached prefixes.
//! - `nonce` is 16 random bytes chosen at ingest. It only selects the stored
//!   object: every ingest writes its own object, so concurrent submissions of
//!   the same image never write to the same key, and ingest needs no
//!   existence check before uploading.
//!
//! Legacy tokens carry no nonce (`dw-img://{sha256-hex}`) and resolve to the
//! object stored under the content hash alone; they remain valid.
//!
//! Storing only hashes means the bucket location is not encoded into request
//! bodies, so the bucket / region can be rotated through config without a
//! data migration.
//!
//! The leading `dw-img://` scheme is recognised by the dispatcher and the
//! dashboard renderer; arbitrary HTTP clients will treat it as an opaque
//! string and pass it through unchanged.
//!
//! Parsing also accepts the bare hex form (no scheme prefix) for robustness
//! when reading legacy or hand-written values.
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;

const SCHEME: &str = "dw-img://";
const SHA_HEX_LEN: usize = 64;
const NONCE_HEX_LEN: usize = 32;

/// SHA-256 of the image content (`.0`) plus the per-ingest storage nonce
/// (`.1`; `None` for legacy tokens). Cheap to clone; copy semantics.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ImageToken(pub [u8; 32], pub Option<[u8; 16]>);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TokenParseError {
    #[error("token hex must be exactly 64 characters (got {0})")]
    WrongLength(usize),
    #[error("token hex contains non-hex characters")]
    InvalidHex,
    #[error("token nonce must be exactly 32 hex characters (got {0})")]
    WrongNonceLength(usize),
}

impl ImageToken {
    /// A fresh token for `sha256` with a random storage nonce, so the upload
    /// gets an object key no other ingest uses.
    pub fn new_unique(sha256: [u8; 32]) -> Self {
        ImageToken(sha256, Some(*uuid::Uuid::new_v4().as_bytes()))
    }

    /// Render as the canonical `dw-img://{hex}` form.
    pub fn to_dw_img_uri(self) -> String {
        format!("{SCHEME}{}", self.to_hex())
    }

    /// Bare hex form, no scheme: `{sha256}.{nonce}`, or `{sha256}` for a
    /// legacy token. Used as the object-store key.
    pub fn to_hex(self) -> String {
        match self.1 {
            Some(nonce) => format!("{}.{}", hex::encode(self.0), hex::encode(nonce)),
            None => hex::encode(self.0),
        }
    }

    /// Returns true if `s` looks like a `dw-img://` token (regardless of
    /// whether the hex parses). Useful for fast rejection in walkers.
    pub fn looks_like_token(s: &str) -> bool {
        s.starts_with(SCHEME)
    }

    /// Rewrite every `dw-img://{sha256}.{nonce}` in `bytes` to
    /// `dw-img://{sha256}`, leaving everything else untouched. Used by the
    /// prompt cache so a token hashes by image content, not by which stored
    /// copy it points at. Borrows when there is nothing to rewrite.
    pub fn strip_storage_nonces(bytes: &[u8]) -> Cow<'_, [u8]> {
        let scheme = SCHEME.as_bytes();
        let is_hex = |b: &[u8]| b.iter().all(u8::is_ascii_hexdigit);
        let mut out: Option<Vec<u8>> = None;
        let mut copied = 0;
        let mut i = 0;
        while let Some(pos) = bytes[i..].windows(scheme.len()).position(|w| w == scheme) {
            let sha_start = i + pos + scheme.len();
            let dot = sha_start + SHA_HEX_LEN;
            let end = dot + 1 + NONCE_HEX_LEN;
            if end <= bytes.len() && is_hex(&bytes[sha_start..dot]) && bytes[dot] == b'.' && is_hex(&bytes[dot + 1..end]) {
                let buf = out.get_or_insert_with(|| Vec::with_capacity(bytes.len()));
                buf.extend_from_slice(&bytes[copied..dot]);
                copied = end;
                i = end;
            } else {
                i = sha_start;
            }
        }
        match out {
            Some(mut buf) => {
                buf.extend_from_slice(&bytes[copied..]);
                Cow::Owned(buf)
            }
            None => Cow::Borrowed(bytes),
        }
    }
}

impl fmt::Debug for ImageToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Show the canonical form so logs are self-explanatory.
        write!(f, "ImageToken({})", self.to_dw_img_uri())
    }
}

impl fmt::Display for ImageToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_dw_img_uri())
    }
}

impl FromStr for ImageToken {
    type Err = TokenParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let hex_str = s.strip_prefix(SCHEME).unwrap_or(s);
        let (sha_hex, nonce_hex) = match hex_str.split_once('.') {
            Some((sha, nonce)) => (sha, Some(nonce)),
            None => (hex_str, None),
        };
        if sha_hex.len() != SHA_HEX_LEN {
            return Err(TokenParseError::WrongLength(sha_hex.len()));
        }
        let mut sha = [0u8; 32];
        hex::decode_to_slice(sha_hex, &mut sha).map_err(|_| TokenParseError::InvalidHex)?;
        let nonce = match nonce_hex {
            None => None,
            Some(n) if n.len() != NONCE_HEX_LEN => return Err(TokenParseError::WrongNonceLength(n.len())),
            Some(n) => {
                let mut nonce = [0u8; 16];
                hex::decode_to_slice(n, &mut nonce).map_err(|_| TokenParseError::InvalidHex)?;
                Some(nonce)
            }
        };
        Ok(ImageToken(sha, nonce))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ImageToken {
        let mut bytes = [0u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = i as u8;
        }
        ImageToken(bytes, None)
    }

    #[test]
    fn round_trip_via_dw_img_uri() {
        let t = sample();
        let s = t.to_dw_img_uri();
        assert!(s.starts_with("dw-img://"));
        let parsed: ImageToken = s.parse().unwrap();
        assert_eq!(parsed, t);
    }

    #[test]
    fn round_trip_via_bare_hex() {
        let t = sample();
        let parsed: ImageToken = t.to_hex().parse().unwrap();
        assert_eq!(parsed, t);
    }

    #[test]
    fn round_trip_with_nonce() {
        let t = ImageToken::new_unique(sample().0);
        let s = t.to_dw_img_uri();
        assert_eq!(s.len(), "dw-img://".len() + 64 + 1 + 32);
        assert_eq!(s.parse::<ImageToken>().unwrap(), t);
        assert_eq!(t.to_hex().parse::<ImageToken>().unwrap(), t);
    }

    #[test]
    fn new_unique_keeps_content_hash_and_varies_nonce() {
        let a = ImageToken::new_unique(sample().0);
        let b = ImageToken::new_unique(sample().0);
        assert_eq!(a.0, b.0);
        assert_ne!(a, b);
        assert_ne!(a.to_hex(), b.to_hex());
    }

    #[test]
    fn rejects_wrong_length() {
        let err: TokenParseError = "dw-img://abcd".parse::<ImageToken>().unwrap_err();
        assert!(matches!(err, TokenParseError::WrongLength(4)));
    }

    #[test]
    fn rejects_non_hex_chars() {
        let bad = format!("dw-img://{}", "z".repeat(64));
        let err = bad.parse::<ImageToken>().unwrap_err();
        assert_eq!(err, TokenParseError::InvalidHex);
    }

    #[test]
    fn rejects_bad_nonce() {
        let sha = sample().to_hex();
        assert_eq!(
            format!("dw-img://{sha}.abcd").parse::<ImageToken>().unwrap_err(),
            TokenParseError::WrongNonceLength(4)
        );
        assert_eq!(
            format!("dw-img://{sha}.{}", "z".repeat(32)).parse::<ImageToken>().unwrap_err(),
            TokenParseError::InvalidHex
        );
    }

    #[test]
    fn looks_like_token_only_matches_scheme() {
        assert!(ImageToken::looks_like_token("dw-img://anything"));
        assert!(!ImageToken::looks_like_token("https://example.com/foo"));
        assert!(!ImageToken::looks_like_token("data:image/png;base64,iVB="));
        assert!(!ImageToken::looks_like_token(""));
    }

    #[test]
    fn strip_storage_nonces_maps_tokens_to_content_form() {
        let content = sample();
        let a = ImageToken::new_unique(content.0);
        let b = ImageToken::new_unique(content.0);
        let body = |t: ImageToken| format!(r#"{{"url":"{}","x":"{}"}}"#, t.to_dw_img_uri(), t.to_dw_img_uri());
        let stripped_a = ImageToken::strip_storage_nonces(body(a).as_bytes()).into_owned();
        let stripped_b = ImageToken::strip_storage_nonces(body(b).as_bytes()).into_owned();
        assert_eq!(stripped_a, stripped_b);
        assert_eq!(stripped_a, body(content).into_bytes());
    }

    #[test]
    fn strip_storage_nonces_leaves_other_bytes_alone() {
        let legacy = format!(r#"{{"url":"{}"}}"#, sample().to_dw_img_uri());
        assert!(matches!(ImageToken::strip_storage_nonces(legacy.as_bytes()), Cow::Borrowed(_)));
        for s in ["no tokens here", "dw-img://", "dw-img://abc.def", "dw-img://dw-img://"] {
            assert!(matches!(ImageToken::strip_storage_nonces(s.as_bytes()), Cow::Borrowed(b) if b == s.as_bytes()));
        }
    }
}
