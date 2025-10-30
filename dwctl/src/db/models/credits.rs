use crate::types::UserId;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Credit transaction type enum matching the database enum
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::Type, PartialEq, ToSchema)]
#[sqlx(type_name = "credit_transaction_type", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum CreditTransactionType {
    Purchase,
    AdminGrant,
    AdminRemoval,
    Usage,
}

/// Database request for creating a new credit transaction
#[derive(Debug, Clone)]
pub struct CreditTransactionCreateDBRequest {
    pub user_id: UserId,
    pub transaction_type: CreditTransactionType,
    pub amount: Decimal,
    pub description: Option<String>,
}

/// Database response for a credit transaction
#[derive(Debug, Clone)]
pub struct CreditTransactionDBResponse {
    pub id: i64,
    pub user_id: UserId,
    pub transaction_type: CreditTransactionType,
    pub amount: Decimal,
    pub balance_after: Decimal,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// User balance response
#[derive(Debug, Clone)]
pub struct UserCreditBalanceDBResponse {
    pub user_id: UserId,
    pub current_balance: Decimal,
}
