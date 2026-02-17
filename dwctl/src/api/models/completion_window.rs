use crate::errors::{Error, Result};

/// Normalizes completion window from display format to storage format
/// - "Standard (24h)" → "24h"
/// - "High (1h)" → "1h"
/// - "24h" → "24h" (pass through)
pub fn normalize_completion_window(input: &str) -> Result<String> {
    let trimmed = input.trim();

    if trimmed.is_empty() {
        return Err(Error::BadRequest {
            message: "Completion window cannot be empty".to_string(),
        });
    }

    let lower = trimmed.to_lowercase();

    // Direct raw format: "24h", "1h", etc.
    if trimmed.ends_with('h') && trimmed.chars().take(trimmed.len() - 1).all(|c| c.is_ascii_digit()) {
        return Ok(trimmed.to_lowercase());
    }

    // Support legacy bare priority names (not documented)
    match lower.as_str() {
        "standard" => return Ok("24h".to_string()),
        "high" => return Ok("1h".to_string()),
        _ => {}
    }

    // Display format: "Standard (24h)", "High (1h)"
    if (lower.starts_with("standard") || lower.starts_with("high"))
        && let Some(time) = extract_time_from_parens(&lower)
    {
        return Ok(time);
    }

    Err(Error::BadRequest {
        message: format!(
            "Invalid completion window format: '{}'. Expected formats: 'Standard (24h)', 'High (1h)', '24h', or '1h'",
            input
        ),
    })
}

fn extract_time_from_parens(s: &str) -> Option<String> {
    let start = s.find('(')?;
    let end = s.find(')')?;
    let time = s[start + 1..end].trim();

    // Validate it's a time format
    if time.ends_with('h') && time.chars().take(time.len() - 1).all(|c| c.is_ascii_digit()) {
        Some(time.to_string())
    } else {
        None
    }
}

/// Formats completion window from storage format to display format
/// - "24h" → "Standard (24h)"
/// - "1h" → "High (1h)"
pub fn format_completion_window(raw: &str) -> String {
    match raw {
        "24h" => "Standard (24h)".to_string(),
        "1h" => "High (1h)".to_string(),
        _ => raw.to_string(), // Pass through unknown values
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_raw_24h() {
        assert_eq!(normalize_completion_window("24h").unwrap(), "24h");
    }

    #[test]
    fn test_normalize_display_standard() {
        assert_eq!(normalize_completion_window("Standard (24h)").unwrap(), "24h");
    }

    #[test]
    fn test_normalize_raw_1h() {
        assert_eq!(normalize_completion_window("1h").unwrap(), "1h");
    }

    #[test]
    fn test_normalize_display_high() {
        assert_eq!(normalize_completion_window("High (1h)").unwrap(), "1h");
    }

    #[test]
    fn test_normalize_case_insensitive() {
        assert_eq!(normalize_completion_window("standard (24h)").unwrap(), "24h");
        assert_eq!(normalize_completion_window("high (1h)").unwrap(), "1h");
    }

    #[test]
    fn test_normalize_with_whitespace() {
        assert_eq!(normalize_completion_window(" Standard (24h) ").unwrap(), "24h");
        assert_eq!(normalize_completion_window("Standard ( 24h )").unwrap(), "24h");
    }

    #[test]
    fn test_normalize_bare_standard() {
        assert_eq!(normalize_completion_window("standard").unwrap(), "24h");
        assert_eq!(normalize_completion_window("Standard").unwrap(), "24h");
        assert_eq!(normalize_completion_window("STANDARD").unwrap(), "24h");
    }

    #[test]
    fn test_normalize_bare_high() {
        assert_eq!(normalize_completion_window("high").unwrap(), "1h");
        assert_eq!(normalize_completion_window("High").unwrap(), "1h");
        assert_eq!(normalize_completion_window("HIGH").unwrap(), "1h");
    }

    #[test]
    fn test_normalize_invalid() {
        assert!(normalize_completion_window("invalid").is_err());
        assert!(normalize_completion_window("").is_err());
    }

    #[test]
    fn test_format_24h() {
        assert_eq!(format_completion_window("24h"), "Standard (24h)");
    }

    #[test]
    fn test_format_1h() {
        assert_eq!(format_completion_window("1h"), "High (1h)");
    }

    #[test]
    fn test_format_unknown_passthrough() {
        assert_eq!(format_completion_window("12h"), "12h");
    }
}
