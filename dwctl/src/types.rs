//! Common type definitions and permission system types.
//!
//! This module defines:
//! - Type aliases for entity IDs (UserId, GroupId, etc.)
//! - Permission and authorization types
//! - Resource and operation enums for access control
//!
//! # ID Types
//!
//! All entity IDs are UUIDs wrapped in type aliases for better type safety:
//!
//! - [`UserId`]: User account identifier
//! - [`ApiKeyId`]: API key identifier
//! - [`DeploymentId`]: Model deployment identifier
//! - [`GroupId`]: Group identifier
//! - [`InferenceEndpointId`]: Backend endpoint identifier
//!
//! # Permission System
//!
//! The permission system is based on three core types:
//!
//! - [`Resource`]: What entity type is being accessed (Users, Groups, Models, etc.)
//! - [`Operation`]: What action is being performed (Read, Create, Update, Delete)
//! - [`Permission`]: Authorization requirement combining resource and operation
//!
//! ## Operations
//!
//! Operations come in two flavors:
//! - **All**: Unrestricted access to all entities (e.g., `ReadAll`, `DeleteAll`)
//! - **Own**: Restricted to user's own entities (e.g., `ReadOwn`, `UpdateOwn`)
//!
//! ## Example Permission Check
//!
//! ```ignore
//! use dwctl::types::{Permission, Resource, Operation};
//!
//! let required = Permission::Allow(Resource::Users, Operation::ReadAll);
//! // Check if user has this permission...
//! ```
//!
//! # Utility Functions
//!
//! - [`abbrev_uuid`]: Abbreviate UUIDs to first 8 chars for logging

use serde::Deserialize;
use std::fmt;
use uuid::Uuid;

// Type aliases for IDs
pub type UserId = Uuid;
pub type ApiKeyId = Uuid;
pub type DeploymentId = Uuid;
pub type GroupId = Uuid;
pub type InferenceEndpointId = Uuid;
#[allow(dead_code)] // TODO: Remove if not needed (currently using fusillade::FileId instead)
pub type FileId = Uuid;

/// Abbreviate a UUID to its first 8 characters for more readable logs and traces
/// Example: "550e8400-e29b-41d4-a716-446655440000" -> "550e8400"
pub fn abbrev_uuid(uuid: &Uuid) -> String {
    uuid.to_string().chars().take(8).collect()
}

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
    CompositeModels,
    Endpoints,
    ApiKeys,
    Analytics,
    Requests,
    Pricing,
    ModelRateLimits,
    Credits,
    Probes,
    Files,
    Batches,
    Webhooks,
    System,
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
