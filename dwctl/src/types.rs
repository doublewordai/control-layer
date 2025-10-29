use serde::Deserialize;
use std::fmt;
use uuid::Uuid;

// Type aliases for IDs
pub type UserId = Uuid;
pub type ApiKeyId = Uuid;
pub type DeploymentId = Uuid;
pub type GroupId = Uuid;
pub type InferenceEndpointId = Uuid;

// Common types for path parameters
#[derive(Debug, Clone, Deserialize)]
pub enum CurrentKeyword {
    #[serde(rename = "current")]
    Current,
}

/// Designed to allow routes like /api-keys/current and /api-keys/{user_id} to hit the same
/// handler.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum UserIdOrCurrent {
    Current(CurrentKeyword),
    Id(UserId),
}

// Operations that can be performed on resources
// *-All means unrestricted access, *-Own means restricted to own resources
// Generics like Create, are justed used for return objects
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operation {
    // Create,
    CreateAll,
    CreateOwn,
    // Read,
    ReadAll,
    ReadOwn,
    // Update,
    UpdateAll,
    UpdateOwn,
    // Delete,
    DeleteAll,
    DeleteOwn,
    // System
    SystemAccess, // Access to system-level data (like deleted models)
}

// Resources that can be operated on
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Resource {
    Users,
    Groups,
    Models,
    Endpoints,
    ApiKeys,
    Analytics,
    Requests,
    Pricing,
    ModelRateLimits,
    Credits,
    Probes,
}

// Permission types for authorization
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Permission {
    /// Simple permission: (Resource, Operation)
    Allow(Resource, Operation),
    /// User must have been granted access to a specific resource instance
    Granted,
    /// Logical combinators
    Any(Vec<Permission>),
    // All(Vec<Permission>),
}

// Add this Display implementation for Operation
impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Operation::CreateAll | Operation::CreateOwn => write!(f, "Create"),
            Operation::ReadAll | Operation::ReadOwn => write!(f, "Read"),
            Operation::UpdateAll | Operation::UpdateOwn => write!(f, "Update"),
            Operation::DeleteAll | Operation::DeleteOwn => write!(f, "Delete"),
            Operation::SystemAccess => write!(f, "Access"),
        }
    }
}
