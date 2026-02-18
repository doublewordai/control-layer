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
    /// Request origin: "api" (direct API), "frontend" (playground), or "fusillade" (batch)
    /// Only present for usage transactions
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_origin: Option<String>,
    /// Batch priority: "Standard (24h)", "High (1h)", or empty string for non-batch
    /// API responses return priority names (converted from internal "1h"/"24h" storage)
    /// Only present for usage transactions
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch_sla: Option<String>,
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
    /// Total number of transactions matching the query (before pagination)
    pub total_count: i64,
    /// Number of items skipped
    pub skip: i64,
    /// Maximum items returned per page
    pub limit: i64,
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
    /// Parse query parameters into TransactionFilters struct
    pub fn to_filters(&self) -> TransactionFilters {
        let transaction_types = self.transaction_types.as_ref().map(|types_str| {
            types_str
                .split(',')
                .filter_map(|t| match t.trim() {
                    "admin_grant" => Some(CreditTransactionType::AdminGrant),
                    "admin_removal" => Some(CreditTransactionType::AdminRemoval),
                    "usage" => Some(CreditTransactionType::Usage),
                    "purchase" => Some(CreditTransactionType::Purchase),
                    _ => None,
                })
                .collect()
        });

        TransactionFilters {
            search: self.search.clone(),
            transaction_types,
            start_date: self.start_date,
            end_date: self.end_date,
        }
    }
}

// Conversions
impl CreditTransactionResponse {
    /// Convert from DB response with optional batch_id (legacy, without category info)
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
            request_origin: None,
            batch_sla: None,
            batch_request_count: None,
        }
    }

    /// Convert from DB response with full category information
    pub fn from_db_with_metadata(
        db: CreditTransactionDBResponse,
        batch_id: Option<Uuid>,
        request_origin: Option<String>,
        batch_sla: Option<String>,
        batch_count: i32,
    ) -> Self {
        use super::completion_window::format_completion_window;

        // Only include batch_request_count for actual batches (count > 1)
        let batch_request_count = if batch_count > 1 { Some(batch_count) } else { None };

        // Format batch_sla for API responses ("24h" → "Standard (24h)")
        let batch_sla = batch_sla.map(|sla| format_completion_window(&sla));

        Self {
            id: db.id,
            user_id: db.user_id,
            transaction_type: db.transaction_type,
            batch_id,
            amount: db.amount,
            source_id: db.source_id,
            description: db.description,
            created_at: db.created_at,
            request_origin,
            batch_sla,
            batch_request_count,
        }
    }
}

impl From<CreditTransactionDBResponse> for CreditTransactionResponse {
    fn from(db: CreditTransactionDBResponse) -> Self {
        Self::from_db_with_batch_id(db, None)
    }
}
