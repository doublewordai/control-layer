//! Base repository trait for database operations.

use std::collections::HashMap;

/// Contains the Repository trait.
///
/// A repository is basically a data access layer for a postgres table that provides methods. It
/// provides methods for creating, reading, updating, and deleting entities, as well as listing
/// them with simple filters.
///
/// Each repository is generic over the entity type T, which must implement sqlx::FromRow.
use crate::db::errors::Result;

/// Base repository trait providing common database operations
///
/// This trait has separate associated types for create requests, update requests, and responses.
#[async_trait::async_trait]
pub trait Repository {
    /// The request type for creating entities
    type CreateRequest;

    /// The request type for updating entities
    type UpdateRequest;

    /// The response/DTO type returned by operations
    type Response;

    /// The identifier type for lookups
    type Id: Send + Sync;

    /// The filter type for list operations
    type Filter: Send + Sync;

    /// Create a new entity
    async fn create(&mut self, request: &Self::CreateRequest) -> Result<Self::Response>;

    /// Get an entity by ID
    async fn get_by_id(&mut self, id: Self::Id) -> Result<Option<Self::Response>>;

    /// Get lots of entities by their IDs, keyed by ID
    async fn get_bulk(&mut self, ids: Vec<Self::Id>) -> Result<HashMap<Self::Id, Self::Response>>;

    /// List entities with filtering and pagination
    async fn list(&mut self, filter: &Self::Filter) -> Result<Vec<Self::Response>>;

    /// Delete an entity by ID
    async fn delete(&mut self, id: Self::Id) -> Result<bool>;

    /// Update an entity by ID
    async fn update(&mut self, id: Self::Id, request: &Self::UpdateRequest) -> Result<Self::Response>;
}
