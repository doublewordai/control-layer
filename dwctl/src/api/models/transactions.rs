use crate::{
    db::models::credits::{CreditTransactionDBResponse, CreditTransactionType},
    types::UserId,
};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

// Request models
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreditTransactionCreate {
    /// User ID (required - UUID format)
    #[schema(value_type = String, format = "uuid")]
    pub user_id: UserId,
    /// Type of transaction (only admin_grant and admin_removal allowed for admin API)
    pub transaction_type: CreditTransactionType,
    /// Amount of credits (absolute value)
    #[schema(value_type = f64)]
    pub amount: Decimal,
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
    /// Amount of credits
    #[schema(value_type = f64)]
    pub amount: Decimal,
    /// Balance after this transaction
    #[schema(value_type = f64)]
    pub balance_after: Decimal,
    /// Description
    pub description: Option<String>,
    /// When the transaction was created
    pub created_at: DateTime<Utc>,
}

/// Query parameters for listing transactions
#[derive(Debug, Deserialize, IntoParams)]
pub struct ListTransactionsQuery {
    /// Filter by user ID (optional, BillingManager only for other users)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>, format = "uuid")]
    pub user_id: Option<UserId>,

    /// Number of items to skip
    pub skip: Option<i64>,

    /// Maximum number of items to return
    pub limit: Option<i64>,
}

// Conversions
impl From<CreditTransactionDBResponse> for CreditTransactionResponse {
    fn from(db: CreditTransactionDBResponse) -> Self {
        Self {
            id: db.id,
            user_id: db.user_id,
            transaction_type: db.transaction_type,
            amount: db.amount,
            balance_after: db.balance_after,
            description: db.description,
            created_at: db.created_at,
        }
    }
}
