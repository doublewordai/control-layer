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

    /// Pagination parameters
    #[serde(flatten)]
    #[param(inline)]
    pub pagination: Pagination,
}

// Conversions
impl CreditTransactionResponse {
    /// Convert from DB response with optional batch_id
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
        }
    }
}

impl From<CreditTransactionDBResponse> for CreditTransactionResponse {
    fn from(db: CreditTransactionDBResponse) -> Self {
        Self::from_db_with_batch_id(db, None)
    }
}
