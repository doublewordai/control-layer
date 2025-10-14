use crate::api::models::groups::{GroupCreate, GroupUpdate};
use crate::types::{GroupId, UserId};
use chrono::{DateTime, Utc};

/// Database request for creating a new group
#[derive(Debug, Clone)]
pub struct GroupCreateDBRequest {
    pub name: String,
    pub description: Option<String>,
    pub created_by: UserId,
}

impl GroupCreateDBRequest {
    pub fn new(created_by: UserId, create: GroupCreate) -> Self {
        Self {
            name: create.name,
            description: create.description,
            created_by,
        }
    }
}

/// Database request for updating a group
#[derive(Debug, Clone)]
pub struct GroupUpdateDBRequest {
    pub name: Option<String>,
    pub description: Option<String>,
}

impl From<GroupUpdate> for GroupUpdateDBRequest {
    fn from(update: GroupUpdate) -> Self {
        Self {
            name: update.name,
            description: update.description,
        }
    }
}

/// Database response for a group
#[derive(Debug, Clone)]
pub struct GroupDBResponse {
    pub id: GroupId,
    pub name: String,
    pub description: Option<String>,
    pub created_by: UserId,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
