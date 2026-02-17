use chrono::{DateTime, Utc};
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, FromRow)]
pub struct BatchCapacityReservation {
    pub id: Uuid,
    pub model_id: Uuid,
    pub completion_window: String,
    pub reserved_requests: i64,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub released_at: Option<DateTime<Utc>>,
}