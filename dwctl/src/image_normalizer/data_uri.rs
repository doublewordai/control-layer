//! Minimal parser for `data:` URIs carrying image bytes.
//!
//! Spec: <https://datatracker.ietf.org/doc/html/rfc2397>
//!
//! Form: `data:[<mediatype>][;base64],<data>`. We support the base64 form
//! only, since that's how OpenAI-compatible clients embed images.
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DataUriError {
    #[error("not a data: URI")]
    NotADataUri,
    #[error("data: URI missing the comma separator")]
    MissingComma,
    #[error("only base64-encoded data: URIs are supported")]
    NotBase64,
    #[error("media type missing")]
    MissingMediaType,
    #[error("base64 decoding failed: {0}")]
    Base64Decode(String),
}

/// Decoded data: URI with its mediatype and raw bytes.
#[derive(Debug)]
pub struct DecodedDataUri {
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// Parse a `data:[<mediatype>];base64,<data>` URI and decode the payload.
///
/// Returns `Err(NotADataUri)` if the input doesn't start with `data:`. Other
/// errors indicate a malformed data URI.
pub fn parse(input: &str) -> Result<DecodedDataUri, DataUriError> {
    let rest = input.strip_prefix("data:").ok_or(DataUriError::NotADataUri)?;

    let (header, payload) = rest.split_once(',').ok_or(DataUriError::MissingComma)?;

    // Header is `<mediatype>[;param][;base64]`. We require ;base64 and a
    // non-empty mediatype.
    let mut parts = header.split(';');
    let mime = parts.next().unwrap_or("").trim();
    if mime.is_empty() {
        return Err(DataUriError::MissingMediaType);
    }

    let has_base64 = parts.any(|p| p.trim().eq_ignore_ascii_case("base64"));
    if !has_base64 {
        return Err(DataUriError::NotBase64);
    }

    let bytes = B64.decode(payload).map_err(|e| DataUriError::Base64Decode(e.to_string()))?;

    Ok(DecodedDataUri {
        mime: mime.to_ascii_lowercase(),
        bytes,
    })
}

/// True if `input` looks like a data URI (cheap prefix check).
pub fn looks_like_data_uri(input: &str) -> bool {
    input.starts_with("data:")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_png() {
        // 1x1 transparent PNG, base64-encoded.
        let uri = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";
        let parsed = parse(uri).expect("parse");
        assert_eq!(parsed.mime, "image/png");
        assert!(!parsed.bytes.is_empty());
        assert_eq!(&parsed.bytes[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn normalises_mime_case() {
        let uri = "data:Image/JPEG;base64,/9j/4AAQ";
        let parsed = parse(uri).expect("parse");
        assert_eq!(parsed.mime, "image/jpeg");
    }

    #[test]
    fn accepts_extra_params_before_base64() {
        let uri = "data:image/png;charset=utf-8;base64,iVBORw0KGgo=";
        let parsed = parse(uri).expect("parse");
        assert_eq!(parsed.mime, "image/png");
    }

    #[test]
    fn rejects_non_data_uri() {
        assert_eq!(parse("https://example.com/x.png").unwrap_err(), DataUriError::NotADataUri);
    }

    #[test]
    fn rejects_missing_comma() {
        assert_eq!(parse("data:image/png;base64").unwrap_err(), DataUriError::MissingComma);
    }

    #[test]
    fn rejects_non_base64() {
        // We don't support url-encoded data: URIs.
        let err = parse("data:image/png,raw_bytes_here").unwrap_err();
        assert_eq!(err, DataUriError::NotBase64);
    }

    #[test]
    fn rejects_missing_mime() {
        let err = parse("data:;base64,iVBORw0KGgo=").unwrap_err();
        assert_eq!(err, DataUriError::MissingMediaType);
    }

    #[test]
    fn rejects_bad_base64() {
        let err = parse("data:image/png;base64,!!!not-base64!!!").unwrap_err();
        assert!(matches!(err, DataUriError::Base64Decode(_)));
    }

    #[test]
    fn looks_like_data_uri_prefix_check() {
        assert!(looks_like_data_uri("data:image/png;base64,foo"));
        assert!(!looks_like_data_uri("https://example.com/img.png"));
        assert!(!looks_like_data_uri(""));
    }
}
