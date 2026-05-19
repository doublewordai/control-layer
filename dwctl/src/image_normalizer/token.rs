//! Opaque token used to reference an image stored in our content-addressed
//! object store.
//!
//! Tokens are stored in request bodies in place of the original
//! user-supplied URL / data URI. They never reach an upstream provider —
//! the dispatcher resolves them to a fresh signed URL just before sending.
//!
//! The token format is `dw-img://{lowercase-hex-sha256}`. Storing only the
//! content hash means the bucket location is not encoded into request bodies,
//! so the bucket / region can be rotated through config without a data
//! migration.
//!
//! The leading `dw-img://` scheme is recognised by the dispatcher and the
//! dashboard renderer; arbitrary HTTP clients will treat it as an opaque
//! string and pass it through unchanged.
//!
//! Parsing also accepts bare hex (no scheme prefix) for robustness when
//! reading legacy or hand-written values.
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

const SCHEME: &str = "dw-img://";

/// 32-byte SHA-256 of the image content. Cheap to clone; copy semantics.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ImageToken(pub [u8; 32]);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TokenParseError {
    #[error("token hex must be exactly 64 characters (got {0})")]
    WrongLength(usize),
    #[error("token hex contains non-hex characters")]
    InvalidHex,
}

impl ImageToken {
    /// Render as the canonical `dw-img://{hex}` form.
    pub fn to_dw_img_uri(self) -> String {
        format!("{SCHEME}{}", hex::encode(self.0))
    }

    /// Bare hex form, no scheme — used as object-store keys.
    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }

    /// Returns true if `s` looks like a `dw-img://` token (regardless of
    /// whether the hex parses). Useful for fast rejection in walkers.
    pub fn looks_like_token(s: &str) -> bool {
        s.starts_with(SCHEME)
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
        if hex_str.len() != 64 {
            return Err(TokenParseError::WrongLength(hex_str.len()));
        }
        let mut out = [0u8; 32];
        hex::decode_to_slice(hex_str, &mut out).map_err(|_| TokenParseError::InvalidHex)?;
        Ok(ImageToken(out))
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
        ImageToken(bytes)
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
    fn looks_like_token_only_matches_scheme() {
        assert!(ImageToken::looks_like_token("dw-img://anything"));
        assert!(!ImageToken::looks_like_token("https://example.com/foo"));
        assert!(!ImageToken::looks_like_token("data:image/png;base64,iVB="));
        assert!(!ImageToken::looks_like_token(""));
    }
}
