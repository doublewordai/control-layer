use crate::db::models::credits::{CreditTransactionDBResponse, CreditTransactionType, UserCreditBalanceDBResponse};
use crate::types::UserId;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

// Request models
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreditTransactionCreate {
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
    #[schema(value_type = i64)]
    pub id: i64,
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

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UserBalanceResponse {
    /// User ID
    #[schema(value_type = String, format = "uuid")]
    pub user_id: UserId,
    /// Current credit balance
    #[schema(value_type = f64)]
    pub current_balance: Decimal,
}

/// Query parameters for listing transactions
#[derive(Debug, Deserialize, IntoParams, ToSchema)]
pub struct ListTransactionsQuery {
    /// Number of items to skip
    #[param(default = 0, minimum = 0)]
    pub skip: Option<i64>,

    /// Maximum number of items to return
    #[param(default = 100, minimum = 1, maximum = 1000)]
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

impl From<UserCreditBalanceDBResponse> for UserBalanceResponse {
    fn from(db: UserCreditBalanceDBResponse) -> Self {
        Self {
            user_id: db.user_id,
            current_balance: db.current_balance,
        }
    }
}
