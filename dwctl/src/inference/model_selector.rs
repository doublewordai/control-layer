//! Model selector syntax for asynchronous ingestion.
//!
//! Flex, background and batch requests are scheduled by their deadline, so a
//! `:suffix` on the model name is discarded before the base alias is
//! resolved. The suffix is only checked for shape: a malformed selector is a
//! 400 rather than a silently different model.

use std::fmt;

/// Separator between a model alias and a suffix (`alias:suffix`).
const SUFFIX_SEPARATOR: char = ':';

/// A model selector with an empty alias, an empty suffix, or more than one
/// suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidModelSelector;

impl fmt::Display for InvalidModelSelector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Invalid model selector: expected a nonempty alias with at most one nonempty ':suffix'.")
    }
}

impl std::error::Error for InvalidModelSelector {}

/// Split `alias:suffix` into the alias and the suffix, checking only shape.
pub fn split_model_suffix(model: &str) -> Result<(&str, Option<&str>), InvalidModelSelector> {
    match model.split_once(SUFFIX_SEPARATOR) {
        None if !model.is_empty() => Ok((model, None)),
        Some((alias, suffix)) if !alias.is_empty() && !suffix.is_empty() && !suffix.contains(SUFFIX_SEPARATOR) => Ok((alias, Some(suffix))),
        _ => Err(InvalidModelSelector),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffix_shape_is_checked_without_interpreting_it() {
        assert_eq!(split_model_suffix("m"), Ok(("m", None)));
        assert_eq!(split_model_suffix("org/m:x-fast"), Ok(("org/m", Some("x-fast"))));
        for malformed in ["", ":x", "m:", "m:a:b"] {
            assert_eq!(split_model_suffix(malformed), Err(InvalidModelSelector), "{malformed:?}");
        }
    }
}
