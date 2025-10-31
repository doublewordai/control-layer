use crate::types::UserId;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// Credit transaction type enum stored as TEXT in database
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::Type, PartialEq, ToSchema)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
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
    pub id: Uuid,
    pub user_id: UserId,
    pub transaction_type: CreditTransactionType,
    pub amount: Decimal,
    pub balance_after: Decimal,
    pub previous_transaction_id: Option<Uuid>,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
}
