//! Shared pagination types for API query parameters.
//!
//! This module provides standardized pagination for all admin API endpoints.
//! All endpoints use offset-based pagination with `skip` and `limit` parameters.

use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use utoipa::{IntoParams, ToSchema};

/// Default number of items to return per page.
pub const DEFAULT_LIMIT: i64 = 10;

/// Maximum number of items that can be requested per page.
pub const MAX_LIMIT: i64 = 100;

/// Standard pagination parameters for admin API list endpoints.
///
/// All admin endpoints use consistent offset-based pagination with:
/// - `skip`: Number of items to skip (default: 0)
/// - `limit`: Maximum items to return (default: 10, max: 100)
///
/// The `limit` is clamped to ensure it's always between 1 and 100,
/// preventing both zero-result queries and excessive data fetching.
#[serde_as]
#[derive(Debug, Default, Deserialize, IntoParams, ToSchema)]
pub struct Pagination {
    /// Number of items to skip (default: 0)
    #[param(default = 0, minimum = 0)]
    #[serde_as(as = "Option<DisplayFromStr>")]
    pub skip: Option<i64>,

    /// Maximum number of items to return (default: 10, max: 100)
    #[param(default = 10, minimum = 1, maximum = 100)]
    #[serde_as(as = "Option<DisplayFromStr>")]
    pub limit: Option<i64>,
}

impl Pagination {
    /// Get the skip value, defaulting to 0 if not specified.
    #[inline]
    pub fn skip(&self) -> i64 {
        self.skip.unwrap_or(0).max(0)
    }

    /// Get the limit value, clamped between 1 and MAX_LIMIT.
    /// Defaults to DEFAULT_LIMIT if not specified.
    #[inline]
    pub fn limit(&self) -> i64 {
        self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
    }

    /// Get both skip and limit as a tuple, useful for destructuring.
    #[inline]
    pub fn params(&self) -> (i64, i64) {
        (self.skip(), self.limit())
    }
}

/// Generic paginated response wrapper for list endpoints.
///
/// Wraps a list of items with pagination metadata including total count
/// for client-side pagination calculations.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PaginatedResponse<T: ToSchema> {
    /// The items for the current page
    pub data: Vec<T>,
    /// Total number of items matching the query (before pagination)
    pub total_count: i64,
    /// Number of items skipped
    pub skip: i64,
    /// Maximum items returned per page
    pub limit: i64,
}

impl<T: ToSchema> PaginatedResponse<T> {
    /// Create a new paginated response
    pub fn new(data: Vec<T>, total_count: i64, skip: i64, limit: i64) -> Self {
        Self {
            data,
            total_count,
            skip,
            limit,
        }
    }
}

/// Default limit for cursor-based pagination (OpenAI batch API compatible).
pub const DEFAULT_CURSOR_LIMIT: i64 = 20;

/// Maximum limit for cursor-based pagination.
pub const MAX_CURSOR_LIMIT: i64 = 100;

/// Cursor-based pagination parameters for OpenAI-compatible endpoints.
///
/// Used by batch and files APIs following OpenAI's pagination pattern:
/// - `after`: Cursor ID to start after (exclusive)
/// - `limit`: Maximum items to return (default: 20, max: 100)
#[derive(Debug, Default, Deserialize, IntoParams, ToSchema)]
pub struct CursorPagination {
    /// A cursor for use in pagination. `after` is an object ID that defines your place in the list.
    pub after: Option<String>,

    /// Maximum number of items to return (default: 20, max: 100)
    #[param(default = 20, minimum = 1, maximum = 100)]
    pub limit: Option<i64>,
}

impl CursorPagination {
    /// Get the after cursor value.
    #[inline]
    pub fn after(&self) -> Option<&str> {
        self.after.as_deref()
    }

    /// Get the limit value, clamped between 1 and MAX_CURSOR_LIMIT.
    /// Defaults to DEFAULT_CURSOR_LIMIT if not specified.
    #[inline]
    pub fn limit(&self) -> i64 {
        self.limit.unwrap_or(DEFAULT_CURSOR_LIMIT).clamp(1, MAX_CURSOR_LIMIT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_values() {
        let p = Pagination::default();
        assert_eq!(p.skip(), 0);
        assert_eq!(p.limit(), DEFAULT_LIMIT);
    }

    #[test]
    fn test_limit_clamping() {
        // Zero is clamped to 1
        let p = Pagination {
            skip: None,
            limit: Some(0),
        };
        assert_eq!(p.limit(), 1);

        // Negative is clamped to 1
        let p = Pagination {
            skip: None,
            limit: Some(-5),
        };
        assert_eq!(p.limit(), 1);

        // Over max is clamped to MAX_LIMIT
        let p = Pagination {
            skip: None,
            limit: Some(1000),
        };
        assert_eq!(p.limit(), MAX_LIMIT);

        // Valid value passes through
        let p = Pagination {
            skip: None,
            limit: Some(50),
        };
        assert_eq!(p.limit(), 50);
    }

    #[test]
    fn test_skip_clamping() {
        // Negative is clamped to 0
        let p = Pagination {
            skip: Some(-10),
            limit: None,
        };
        assert_eq!(p.skip(), 0);

        // Valid value passes through
        let p = Pagination {
            skip: Some(100),
            limit: None,
        };
        assert_eq!(p.skip(), 100);
    }

    #[test]
    fn test_params() {
        let p = Pagination {
            skip: Some(20),
            limit: Some(50),
        };
        assert_eq!(p.params(), (20, 50));
    }

    #[test]
    fn test_cursor_default_values() {
        let p = CursorPagination::default();
        assert_eq!(p.after(), None);
        assert_eq!(p.limit(), DEFAULT_CURSOR_LIMIT);
    }

    #[test]
    fn test_cursor_limit_clamping() {
        // Zero is clamped to 1
        let p = CursorPagination {
            after: None,
            limit: Some(0),
        };
        assert_eq!(p.limit(), 1);

        // Over max is clamped to MAX_CURSOR_LIMIT
        let p = CursorPagination {
            after: None,
            limit: Some(1000),
        };
        assert_eq!(p.limit(), MAX_CURSOR_LIMIT);

        // Valid value passes through
        let p = CursorPagination {
            after: None,
            limit: Some(50),
        };
        assert_eq!(p.limit(), 50);
    }

    #[test]
    fn test_cursor_after() {
        let p = CursorPagination {
            after: Some("cursor_123".to_string()),
            limit: None,
        };
        assert_eq!(p.after(), Some("cursor_123"));
    }
}
