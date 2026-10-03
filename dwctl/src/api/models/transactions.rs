//! API request/response models for credit transactions.

use super::pagination::Pagination;
use crate::{
    db::models::credits::{CreditTransactionDBResponse, CreditTransactionType},
    types::UserId,
};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

// Subset of the DB Transaction Type enum for API use as only admin transactions are allowed here
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TransactionType {
    AdminGrant,
    AdminRemoval,
}

impl From<&TransactionType> for CreditTransactionType {
    fn from(tx_type: &TransactionType) -> Self {
        match tx_type {
            TransactionType::AdminGrant => CreditTransactionType::AdminGrant,
            TransactionType::AdminRemoval => CreditTransactionType::AdminRemoval,
        }
    }
}

// Request models
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreditTransactionCreate {
    /// User ID (required - UUID format)
    #[schema(value_type = String, format = "uuid")]
    pub user_id: UserId,
    /// Type of transaction (only admin_grant and admin_removal allowed for admin API)
    pub transaction_type: TransactionType,
    /// Amount of credits (absolute value, sent as string to preserve precision)
    #[schema(value_type = String)]
    pub amount: Decimal,
    /// Source ID for the transaction (user UUID, or UUID-suffix for grants)
    pub source_id: String,
    /// Optional description of the transaction
    pub description: Option<String>,
}

// Response models
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreditTransactionResponse {
    /// Transaction ID
    #[schema(value_type = String, format = "uuid")]
    pub id: Uuid,
    /// User ID
    #[schema(value_type = String, format = "uuid")]
    pub user_id: UserId,
    /// Transaction type
    pub transaction_type: CreditTransactionType,
    /// Batch ID (present when this is a grouped batch of multiple usage transactions)
    #[schema(value_type = Option<String>, format = "uuid")]
    pub batch_id: Option<Uuid>,
    /// Amount of credits (returned as string to preserve precision)
    #[schema(value_type = String)]
    pub amount: Decimal,
    /// Source ID
    pub source_id: String,
    /// Description
    pub description: Option<String>,
    /// When the transaction was created
    pub created_at: DateTime<Utc>,
    /// Service tier: "realtime", "flex", "async", or "batch".
    /// Only present for usage transactions
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<String>,
    /// Number of requests in this batch (only present for batch transactions, always > 1)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_request_count: Option<i32>,
}

/// Paginated response for transaction listing with balance context.
/// Mirrors PaginatedResponse structure with additional balance field.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TransactionListResponse {
    /// The transactions for the current page
    pub data: Vec<CreditTransactionResponse>,
    /// Whether more transactions exist beyond this page. There is no total
    /// count: computing one would scan the whole filtered history for a
    /// pager widget.
    pub has_more: bool,
    /// Current user balance when skip=0, or balance at the pagination point (before the
    /// first transaction on this page) when skip>0. Frontend can compute each row's balance
    /// by subtracting signed amounts from this value.
    #[schema(value_type = String)]
    pub page_start_balance: Decimal,
}

/// Query parameters for listing transactions
#[derive(Debug, Deserialize, IntoParams)]
pub struct ListTransactionsQuery {
    /// Filter by user ID (optional, BillingManager only for other users)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>, format = "uuid")]
    pub user_id: Option<UserId>,

    /// Return all transactions across all users (BillingManager only)
    pub all: Option<bool>,

    /// Group transactions by fusillade_batch_id (merges batch requests into single entries)
    pub group_batches: Option<bool>,

    /// Search term for description (case-insensitive)
    pub search: Option<String>,

    /// Filter by transaction types (comma-separated: "admin_grant,purchase" or "usage,admin_removal")
    pub transaction_types: Option<String>,

    /// Filter transactions created on or after this date/time (ISO 8601 format)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>, format = "date-time")]
    pub start_date: Option<DateTime<Utc>>,

    /// Filter transactions created on or before this date/time (ISO 8601 format)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>, format = "date-time")]
    pub end_date: Option<DateTime<Utc>>,

    /// Pagination parameters
    #[serde(flatten)]
    #[param(inline)]
    pub pagination: Pagination,
}

/// Internal filter struct for repository layer
#[derive(Debug, Default, Clone)]
pub struct TransactionFilters {
    pub search: Option<String>,
    pub transaction_types: Option<Vec<CreditTransactionType>>,
    pub start_date: Option<DateTime<Utc>>,
    pub end_date: Option<DateTime<Utc>>,
}

impl ListTransactionsQuery {
    /// Parse query parameters into a `TransactionFilters` struct.
    ///
    /// `transaction_types` is a comma-separated list of closed-enum tokens
    /// (`admin_grant`, `admin_removal`, `usage`, `purchase`). Unlike a free
    /// filter, an unrecognized token is a client error — the create endpoint
    /// rejects unknown `transaction_type` values via serde, and the list
    /// endpoint validates the same taxonomy here. Returns an `Err(message)`
    /// describing the first unrecognized token so the handler can map it to a
    /// `400 BadRequest`. Stray empty tokens (e.g. `",,"`) are tolerated; an
    /// all-empty parse collapses to `None` (no filter) rather than "match no
    /// rows".
    pub fn to_filters(&self) -> Result<TransactionFilters, String> {
        let transaction_types = self
            .transaction_types
            .as_ref()
            .and_then(|types_str| {
                let mut parsed = Vec::new();
                for t in types_str.split(',').map(str::trim) {
                    match t {
                        "admin_grant" => parsed.push(CreditTransactionType::AdminGrant),
                        "admin_removal" => parsed.push(CreditTransactionType::AdminRemoval),
                        "usage" => parsed.push(CreditTransactionType::Usage),
                        "purchase" => parsed.push(CreditTransactionType::Purchase),
                        "" => {} // tolerate stray empty tokens (e.g. ",,")
                        other => return Some(Err(format!("unrecognized transaction_types value: '{other}'"))),
                    }
                }
                if parsed.is_empty() { None } else { Some(Ok(parsed)) }
            })
            .transpose()?;

        Ok(TransactionFilters {
            search: self.search.clone(),
            transaction_types,
            start_date: self.start_date,
            end_date: self.end_date,
        })
    }
}

// Conversions
impl CreditTransactionResponse {
    /// Convert from DB response with optional batch_id (without batch-grouping metadata).
    /// Carries the denormalized `service_tier` from the ledger row so the non-grouped
    /// transactions lists expose the tier just like the grouped path (COR-514).
    pub fn from_db_with_batch_id(db: CreditTransactionDBResponse, batch_id: Option<Uuid>) -> Self {
        Self {
            id: db.id,
            user_id: db.user_id,
            transaction_type: db.transaction_type,
            batch_id,
            amount: db.amount,
            source_id: db.source_id,
            description: db.description,
            created_at: db.created_at,
            service_tier: db.service_tier,
            batch_request_count: None,
        }
    }

    /// Convert from DB response with full category information
    pub fn from_db_with_metadata(
        db: CreditTransactionDBResponse,
        batch_id: Option<Uuid>,
        service_tier: Option<String>,
        batch_count: i32,
    ) -> Self {
        // Only include batch_request_count for actual batches (count > 1)
        let batch_request_count = if batch_count > 1 { Some(batch_count) } else { None };

        Self {
            id: db.id,
            user_id: db.user_id,
            transaction_type: db.transaction_type,
            batch_id,
            amount: db.amount,
            source_id: db.source_id,
            description: db.description,
            created_at: db.created_at,
            service_tier,
            batch_request_count,
        }
    }
}

impl From<CreditTransactionDBResponse> for CreditTransactionResponse {
    fn from(db: CreditTransactionDBResponse) -> Self {
        Self::from_db_with_batch_id(db, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::pagination::Pagination;

    fn query() -> ListTransactionsQuery {
        ListTransactionsQuery {
            user_id: None,
            all: None,
            group_batches: None,
            search: None,
            transaction_types: None,
            start_date: None,
            end_date: None,
            pagination: Pagination::default(),
        }
    }

    fn query_with_types(types: &str) -> ListTransactionsQuery {
        let mut q = query();
        q.transaction_types = Some(types.to_string());
        q
    }

    #[test]
    fn to_filters_all_four_valid_tokens_parse_in_order() {
        let filters = query_with_types("admin_grant,admin_removal,usage,purchase").to_filters().unwrap();
        let types = filters.transaction_types.unwrap();
        assert_eq!(
            types,
            vec![
                CreditTransactionType::AdminGrant,
                CreditTransactionType::AdminRemoval,
                CreditTransactionType::Usage,
                CreditTransactionType::Purchase,
            ]
        );
    }

    #[test]
    fn to_filters_rejects_all_invalid_tokens() {
        let err = query_with_types("admin_grnt_typo").to_filters().unwrap_err();
        assert!(
            err.contains("unrecognized transaction_types value"),
            "error should describe the unrecognized token, got: {err}"
        );
        assert!(err.contains("admin_grnt_typo"), "error should name the offending token, got: {err}");
    }

    #[test]
    fn to_filters_rejects_first_invalid_token_and_stops() {
        // A single valid token mixed with garbage must still be rejected — the
        // taxonomy is closed, just like the create endpoint's serde enum, so a
        // silent `filter_map`-style drop of unknown tokens must not return.
        let err = query_with_types("admin_grant,bogus").to_filters().unwrap_err();
        assert!(err.contains("bogus"), "error should name the offending token, got: {err}");
    }

    #[test]
    fn to_filters_empty_string_collapses_to_no_filter() {
        // An empty `transaction_types=` query param deserializes to Some("")
        // (the field is present). The sole token is empty and deemed stray
        // noise; with nothing left to filter on it collapses to None rather
        // than matching zero rows.
        let filters = query_with_types("").to_filters().unwrap();
        assert!(filters.transaction_types.is_none(), "bare empty string => None (no filter)");
    }
}
