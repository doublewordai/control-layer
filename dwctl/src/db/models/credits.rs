//! Database models for credit transactions.

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
    pub source_id: String,
    pub description: Option<String>,
    /// Batch ID for fusillade batch requests (denormalized from http_analytics)
    pub fusillade_batch_id: Option<Uuid>,
}

impl CreditTransactionCreateDBRequest {
    /// Create an admin grant request with automatically generated random source_id
    pub fn admin_grant(user_id: UserId, grantor_id: UserId, amount: Decimal, description: Option<String>) -> Self {
        Self {
            user_id,
            transaction_type: CreditTransactionType::AdminGrant,
            amount,
            source_id: format!("{}_{}", grantor_id, uuid::Uuid::new_v4()),
            description,
            fusillade_batch_id: None,
        }
    }
}

/// Database response for a credit transaction
#[derive(Debug, Clone)]
pub struct CreditTransactionDBResponse {
    pub id: Uuid,
    pub user_id: UserId,
    pub transaction_type: CreditTransactionType,
    pub amount: Decimal,
    pub source_id: String,
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
}
