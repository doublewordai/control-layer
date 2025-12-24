//! Database repository for groups and memberships.

use crate::db::{
    errors::{DbError, Result},
    handlers::repository::Repository,
    models::groups::{GroupCreateDBRequest, GroupDBResponse, GroupUpdateDBRequest},
};
use crate::types::{DeploymentId, GroupId, Operation, UserId, abbrev_uuid};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgConnection};
use tracing::instrument;
use uuid::Uuid;

/// Filter for listing groups
#[derive(Debug, Clone)]
pub struct GroupFilter {
    pub skip: i64,
    pub limit: i64,
    pub search: Option<String>, // Case-insensitive substring search on name and description
}

impl GroupFilter {
    pub fn new(skip: i64, limit: i64) -> Self {
        Self { skip, limit, search: None }
    }

    pub fn with_search(mut self, search: String) -> Self {
        self.search = Some(search);
        self
    }
}

// Database entity model
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
struct Group {
    pub id: GroupId,
    pub name: String,
    pub description: Option<String>,
    pub created_by: UserId,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub source: String,
}

pub struct Groups<'c> {
    db: &'c mut PgConnection,
}

impl From<Group> for GroupDBResponse {
    fn from(group: Group) -> Self {
        Self {
            id: group.id,
            name: group.name,
            description: group.description,
            created_by: group.created_by,
            created_at: group.created_at,
            updated_at: group.updated_at,
            source: group.source,
        }
    }
}

#[async_trait::async_trait]
impl<'c> Repository for Groups<'c> {
    type CreateRequest = GroupCreateDBRequest;
    type UpdateRequest = GroupUpdateDBRequest;
    type Response = GroupDBResponse;
    type Id = GroupId;
    type Filter = GroupFilter;

    #[instrument(skip(self, request), fields(name = %request.name), err)]
    async fn create(&mut self, request: &Self::CreateRequest) -> Result<Self::Response> {
        // all groups created via handler/api are native, sso groups use the sync function instead
        // created_at and updated_at use database DEFAULT NOW() for consistency
        let group = sqlx::query_as!(
            Group,
            r#"
            INSERT INTO groups (name, description, created_by, source)
            VALUES ($1, $2, $3, 'native')
            RETURNING *
            "#,
            request.name,
            request.description,
            request.created_by
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(GroupDBResponse::from(group))
    }

    #[instrument(skip(self), fields(group_id = %abbrev_uuid(&id)), err)]
    async fn get_by_id(&mut self, id: Self::Id) -> Result<Option<Self::Response>> {
        let group = sqlx::query_as!(Group, "SELECT * FROM groups WHERE id = $1", id)
            .fetch_optional(&mut *self.db)
            .await?;

        Ok(group.map(|g| GroupDBResponse {
            id: g.id,
            name: g.name,
            description: g.description,
            created_by: g.created_by,
            created_at: g.created_at,
            updated_at: g.updated_at,
            source: g.source,
        }))
    }

    #[instrument(skip(self), fields(group_id = %abbrev_uuid(&id)), err)]
    async fn delete(&mut self, id: Self::Id) -> Result<bool> {
        // Prevent deletion of the Everyone group
        if id == uuid::Uuid::nil() {
            return Err(DbError::ProtectedEntity {
                operation: Operation::DeleteAll,
                reason: "Cannot delete the Everyone group".to_string(),
                entity_type: "Group".to_string(),
                entity_id: Some(id.to_string()),
            });
        }

        let result = sqlx::query!("DELETE FROM groups WHERE id = $1", id).execute(&mut *self.db).await?;

        Ok(result.rows_affected() > 0)
    }

    #[instrument(skip(self, request), fields(group_id = %abbrev_uuid(&id)), err)]
    async fn update(&mut self, id: Self::Id, request: &Self::UpdateRequest) -> Result<Self::Response> {
        // Prevent updating of the Everyone group
        if id == uuid::Uuid::nil() {
            return Err(DbError::ProtectedEntity {
                operation: Operation::UpdateAll,
                reason: "Cannot update the Everyone group".to_string(),
                entity_type: "Group".to_string(),
                entity_id: Some(id.to_string()),
            });
        }

        // Atomic update with conditional field updates
        let group = sqlx::query_as!(
            Group,
            r#"
            UPDATE groups SET
                name = COALESCE($2, name),
                description = COALESCE($3, description),
                updated_at = NOW()
            WHERE id = $1
            RETURNING *
            "#,
            id,
            request.name,
            request.description
        )
        .fetch_optional(&mut *self.db)
        .await?
        .ok_or_else(|| DbError::NotFound)?;

        Ok(GroupDBResponse::from(group))
    }

    #[instrument(skip(self, filter), fields(limit = filter.limit, skip = filter.skip), err)]
    async fn list(&mut self, filter: &Self::Filter) -> Result<Vec<Self::Response>> {
        use sqlx::QueryBuilder;

        let mut query = QueryBuilder::new("SELECT * FROM groups WHERE 1=1");

        // Add search filter if specified (case-insensitive substring match on name or description)
        if let Some(ref search) = filter.search {
            tracing::info!("Database layer: Building search query for '{}'", search);
            let search_pattern = format!("%{}%", search.to_lowercase());
            query.push(" AND (LOWER(name) LIKE ");
            query.push_bind(search_pattern.clone());
            query.push(" OR LOWER(COALESCE(description, '')) LIKE ");
            query.push_bind(search_pattern);
            query.push(")");
        } else {
            tracing::info!("Database layer: No search filter in GroupFilter");
        }

        query.push(" ORDER BY name LIMIT ");
        query.push_bind(filter.limit);
        query.push(" OFFSET ");
        query.push_bind(filter.skip);

        let sql = query.sql();
        tracing::info!("Executing SQL: {}", sql);

        let groups = query.build_query_as::<Group>().fetch_all(&mut *self.db).await?;

        tracing::info!("Database layer: Retrieved {} groups", groups.len());

        Ok(groups.into_iter().map(GroupDBResponse::from).collect())
    }

    #[instrument(skip(self, ids), fields(count = ids.len()), err)]
    async fn get_bulk(&mut self, ids: Vec<GroupId>) -> Result<std::collections::HashMap<GroupId, GroupDBResponse>> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let groups = sqlx::query_as!(Group, "SELECT * FROM groups WHERE id = ANY($1)", ids.as_slice())
            .fetch_all(&mut *self.db)
            .await?;

        let mut result = std::collections::HashMap::new();

        for group in groups {
            result.insert(group.id, GroupDBResponse::from(group));
        }

        Ok(result)
    }
}

impl<'c> Groups<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    #[instrument(skip(self, filter), err)]
    pub async fn count(&mut self, filter: &GroupFilter) -> Result<i64> {
        use sqlx::QueryBuilder;

        let mut query = QueryBuilder::new("SELECT COUNT(*) FROM groups WHERE 1=1");

        // Add search filter if specified
        if let Some(ref search) = filter.search {
            let search_pattern = format!("%{}%", search.to_lowercase());
            query.push(" AND (LOWER(name) LIKE ");
            query.push_bind(search_pattern.clone());
            query.push(" OR LOWER(COALESCE(description, '')) LIKE ");
            query.push_bind(search_pattern);
            query.push(")");
        }

        let count: i64 = query.build_query_scalar().fetch_one(&mut *self.db).await?;

        Ok(count)
    }

    #[instrument(skip(self), fields(user_id = %abbrev_uuid(&user_id), group_id = %abbrev_uuid(&group_id)), err)]
    pub async fn add_user_to_group(&mut self, user_id: UserId, group_id: GroupId) -> Result<()> {
        match sqlx::query!(
            "INSERT INTO user_groups (user_id, group_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            user_id,
            group_id
        )
        .execute(&mut *self.db)
        .await
        {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(db_err)) if db_err.is_foreign_key_violation() => {
                // Foreign key violation means either user or group doesn't exist
                Err(DbError::NotFound)
            }
            Err(e) => Err(DbError::from(e)),
        }
    }

    #[instrument(skip(self), fields(user_id = %abbrev_uuid(&user_id), group_id = %abbrev_uuid(&group_id)), err)]
    pub async fn remove_user_from_group(&mut self, user_id: UserId, group_id: GroupId) -> Result<()> {
        let result = sqlx::query!("DELETE FROM user_groups WHERE user_id = $1 AND group_id = $2", user_id, group_id)
            .execute(&mut *self.db)
            .await?;
        if result.rows_affected() > 0 {
            Ok(())
        } else {
            Err(DbError::NotFound)
        }
    }

    #[instrument(skip(self), fields(user_id = %abbrev_uuid(&user_id)), err)]
    pub async fn get_user_groups(&mut self, user_id: UserId) -> Result<Vec<GroupDBResponse>> {
        let mut groups = sqlx::query_as!(
            Group,
            r#"
            SELECT g.* FROM groups g
            INNER JOIN user_groups ug ON g.id = ug.group_id
            WHERE ug.user_id = $1 AND g.id != '00000000-0000-0000-0000-000000000000'
            ORDER BY g.name
            "#,
            user_id
        )
        .fetch_all(&mut *self.db)
        .await?;

        // Always add the Everyone group (it should always exist from migration)
        let everyone_group = sqlx::query_as!(Group, "SELECT * FROM groups WHERE id = '00000000-0000-0000-0000-000000000000'")
            .fetch_one(&mut *self.db)
            .await?;

        groups.push(everyone_group);

        Ok(groups.into_iter().map(GroupDBResponse::from).collect())
    }

    #[instrument(skip(self), fields(group_id = %abbrev_uuid(&group_id)), err)]
    pub async fn get_group_users(&mut self, group_id: GroupId) -> Result<Vec<UserId>> {
        if group_id == Uuid::nil() {
            // Everyone group - return all users (excluding system user)
            let users = sqlx::query!("SELECT id FROM users WHERE id != '00000000-0000-0000-0000-000000000000'")
                .fetch_all(&mut *self.db)
                .await?;
            Ok(users.into_iter().map(|r| r.id).collect())
        } else {
            // Regular group - return users in the group
            let users = sqlx::query!("SELECT user_id FROM user_groups WHERE group_id = $1", group_id)
                .fetch_all(&mut *self.db)
                .await?;
            Ok(users.into_iter().map(|r| r.user_id).collect())
        }
    }

    // Deployment-group management methods

    #[instrument(skip(self), fields(deployment_id = %abbrev_uuid(&deployment_id), group_id = %abbrev_uuid(&group_id)), err)]
    pub async fn add_deployment_to_group(&mut self, deployment_id: DeploymentId, group_id: GroupId, granted_by: UserId) -> Result<()> {
        match sqlx::query!(
            "INSERT INTO deployment_groups (deployment_id, group_id, granted_by) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
            deployment_id,
            group_id,
            granted_by
        )
        .execute(&mut *self.db)
        .await
        {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(db_err)) if db_err.is_foreign_key_violation() => {
                // Foreign key violation means either deployment or group doesn't exist
                Err(DbError::NotFound)
            }
            Err(e) => Err(DbError::from(e)),
        }
    }

    #[instrument(skip(self), fields(deployment_id = %abbrev_uuid(&deployment_id), group_id = %abbrev_uuid(&group_id)), err)]
    pub async fn remove_deployment_from_group(&mut self, deployment_id: DeploymentId, group_id: GroupId) -> Result<()> {
        let result = sqlx::query!(
            "DELETE FROM deployment_groups WHERE deployment_id = $1 AND group_id = $2",
            deployment_id,
            group_id
        )
        .execute(&mut *self.db)
        .await?;
        if result.rows_affected() > 0 {
            Ok(())
        } else {
            Err(DbError::NotFound)
        }
    }

    #[instrument(skip(self), fields(group_id = %abbrev_uuid(&group_id)), err)]
    pub async fn get_group_deployments(&mut self, group_id: GroupId) -> Result<Vec<DeploymentId>> {
        let deployments = sqlx::query!(
            "SELECT dg.deployment_id FROM deployment_groups dg
             JOIN deployed_models dm ON dg.deployment_id = dm.id
             WHERE dg.group_id = $1 AND dm.deleted = false",
            group_id
        )
        .fetch_all(&mut *self.db)
        .await?;
        Ok(deployments.into_iter().map(|r| r.deployment_id).collect())
    }

    #[instrument(skip(self), fields(deployment_id = %abbrev_uuid(&deployment_id)), err)]
    pub async fn get_deployment_groups(&mut self, deployment_id: DeploymentId) -> Result<Vec<GroupId>> {
        let groups = sqlx::query!("SELECT group_id FROM deployment_groups WHERE deployment_id = $1", deployment_id)
            .fetch_all(&mut *self.db)
            .await?;
        Ok(groups.into_iter().map(|r| r.group_id).collect())
    }

    // Bulk relationship fetching methods to avoid N+1 queries

    #[instrument(skip(self, deployment_ids), fields(count = deployment_ids.len()), err)]
    pub async fn get_deployments_groups_bulk(
        &mut self,
        deployment_ids: &[DeploymentId],
    ) -> Result<std::collections::HashMap<DeploymentId, Vec<GroupId>>> {
        if deployment_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let rows = sqlx::query!(
            "SELECT deployment_id, group_id FROM deployment_groups WHERE deployment_id = ANY($1)",
            deployment_ids
        )
        .fetch_all(&mut *self.db)
        .await?;

        let mut result = std::collections::HashMap::new();
        for row in rows {
            result.entry(row.deployment_id).or_insert_with(Vec::new).push(row.group_id);
        }

        Ok(result)
    }

    #[instrument(skip(self, group_ids), fields(count = group_ids.len()), err)]
    pub async fn get_groups_users_bulk(&mut self, group_ids: &[GroupId]) -> Result<std::collections::HashMap<GroupId, Vec<UserId>>> {
        if group_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        // Handle regular groups (excluding Everyone group)
        let regular_group_ids: Vec<GroupId> = group_ids.iter().filter(|&&id| id != Uuid::nil()).copied().collect();

        let mut result = std::collections::HashMap::new();

        if !regular_group_ids.is_empty() {
            let rows = sqlx::query!(
                "SELECT group_id, user_id FROM user_groups WHERE group_id = ANY($1)",
                &regular_group_ids
            )
            .fetch_all(&mut *self.db)
            .await?;

            for row in rows {
                result.entry(row.group_id).or_insert_with(Vec::new).push(row.user_id);
            }
        }

        // Handle Everyone group if requested and it exists
        if group_ids.contains(&Uuid::nil())
            && sqlx::query!("SELECT id FROM groups WHERE id = '00000000-0000-0000-0000-000000000000'")
                .fetch_optional(&mut *self.db)
                .await?
                .is_some()
        {
            let all_users = sqlx::query!("SELECT id FROM users WHERE id != '00000000-0000-0000-0000-000000000000'")
                .fetch_all(&mut *self.db)
                .await?;
            result.insert(Uuid::nil(), all_users.into_iter().map(|r| r.id).collect());
        }

        Ok(result)
    }

    #[instrument(skip(self, user_ids), fields(count = user_ids.len()), err)]
    pub async fn get_users_groups_bulk(&mut self, user_ids: &[UserId]) -> Result<std::collections::HashMap<UserId, Vec<GroupId>>> {
        if user_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let rows = sqlx::query!(
            "SELECT user_id, group_id FROM user_groups WHERE user_id = ANY($1) AND group_id != '00000000-0000-0000-0000-000000000000'",
            user_ids
        )
        .fetch_all(&mut *self.db)
        .await?;

        let mut result = std::collections::HashMap::new();
        for row in rows {
            result.entry(row.user_id).or_insert_with(Vec::new).push(row.group_id);
        }

        // Always add Everyone group to each user (it should always exist from migration)
        sqlx::query!("SELECT id FROM groups WHERE id = '00000000-0000-0000-0000-000000000000'")
            .fetch_one(&mut *self.db)
            .await?;

        for user_id in user_ids {
            result.entry(*user_id).or_insert_with(Vec::new).push(Uuid::nil());
        }

        Ok(result)
    }

    #[instrument(skip(self, group_ids), fields(count = group_ids.len()), err)]
    pub async fn get_groups_deployments_bulk(
        &mut self,
        group_ids: &[GroupId],
    ) -> Result<std::collections::HashMap<GroupId, Vec<DeploymentId>>> {
        if group_ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }

        let rows = sqlx::query!(
            "SELECT group_id, deployment_id FROM deployment_groups WHERE group_id = ANY($1)",
            group_ids
        )
        .fetch_all(&mut *self.db)
        .await?;

        let mut result = std::collections::HashMap::new();
        for row in rows {
            result.entry(row.group_id).or_insert_with(Vec::new).push(row.deployment_id);
        }

        Ok(result)
    }

    #[instrument(skip(self, group_names, source, description), fields(user_id = %abbrev_uuid(&user_id)), err)]
    pub async fn sync_groups_with_sso(
        &mut self,
        user_id: UserId,
        group_names: Vec<String>,
        source: &str,
        description: &str,
    ) -> Result<Vec<Uuid>> {
        let row = sqlx::query!(
        r#"

        -- Unravel the list of group ids to check into a table
        WITH incoming AS (
            SELECT unnest($1::text[]) AS name
        ),

        -- Find the ids of groups that already exist with that name for this source
        existing AS (
            SELECT id, name
            FROM groups
            WHERE name IN (SELECT name FROM incoming)
              AND source = $2
        ),

        -- Insert any non existing ones
        inserted AS (
            INSERT INTO groups (name, description, created_by, created_at, updated_at, source)
            SELECT name, $4, $3, NOW(), NOW(), $2
            FROM incoming i
            WHERE NOT EXISTS (
                SELECT 1 FROM groups g WHERE g.name = i.name
            ) --NB THIS CURRENTLY MEANS YOU CANT HAVE THE SAME NAMES but different sources.
            -- You could change the constraint to allow this, and then we wouldn't need to check every existing one for the name just the existing ones for that sso provider only.
            RETURNING id, name
        ),

        -- Get the ids for the found existing, and the inserted ones
        all_ids AS (
            SELECT * FROM existing
            UNION ALL
            SELECT * FROM inserted
        ),

        -- Get ids of all the groups this user isn't a member of for this source - either they were
        -- never members or they've been removed since being added.
        orphan_ids AS (
            SELECT id FROM groups g
            WHERE g.source = $2
              AND g.id NOT IN (SELECT id FROM all_ids)
        ),

        -- Remove memberships from groups the user shouldn't be in
        deleted_user_groups AS (
            DELETE FROM user_groups ug
            USING orphan_ids o
            WHERE ug.group_id = o.id
            RETURNING ug.group_id
        ),

        -- Add memberships to groups the user should be in, if they're already in then just skip.
        insert_user_groups AS (
            INSERT INTO user_groups (user_id, group_id)
            SELECT $3, g.id
            FROM all_ids g
            ON CONFLICT (user_id, group_id) DO NOTHING
            RETURNING user_id, group_id
        )

        -- We want back the ids of the groups that this user is now in.
        SELECT array_agg(id) AS member_group_ids
        FROM all_ids
        "#,
        &group_names, //1
        source,  //2
        user_id, //3
        description //4
    )
            .fetch_one(&mut *self.db)
            .await?;

        Ok(row.member_group_ids.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::handlers::api_keys::ApiKeys;
    use crate::db::models::api_keys::ApiKeyPurpose;
    use crate::test::utils::get_test_endpoint_id;
    use crate::{
        db::{
            handlers::{Credits, Deployments, Users, users::UserFilter},
            models::{
                api_keys::ApiKeyCreateDBRequest,
                credits::{CreditTransactionCreateDBRequest, CreditTransactionType},
                deployments::DeploymentCreateDBRequest,
            },
        },
        seed_database,
    };
    use rust_decimal::Decimal;
    use sqlx::{Acquire, PgPool};

    // Mock coalesce function to simulate SQL COALESCE behavior for tests
    fn mock_coalesce_update(update_request: &GroupUpdateDBRequest, original_response: &GroupDBResponse) -> GroupDBResponse {
        GroupDBResponse {
            id: original_response.id,
            name: update_request.name.clone().unwrap_or_else(|| original_response.name.clone()),
            description: update_request.description.clone().or_else(|| original_response.description.clone()),
            created_by: original_response.created_by,
            created_at: original_response.created_at,
            updated_at: chrono::Utc::now(),
            source: "native".to_string(),
        }
    }

    // Helper function for deployment-group tests
    async fn setup_test_environment(pool: &PgPool) -> UserId {
        let user_id = UserId::new_v4();

        // Create test user
        sqlx::query!(
            "INSERT INTO users (id, username, email, display_name, auth_source) VALUES ($1, $2, $3, $4, $5)",
            user_id,
            "test_user",
            "test@example.com",
            Some("Test User"),
            "test"
        )
        .execute(pool)
        .await
        .expect("Failed to create test user");

        // Create inference endpoint for deployments
        let config = crate::test::utils::create_test_config();
        seed_database(&config.model_sources, pool)
            .await
            .expect("Failed to create inference endpoints");

        user_id
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_add_deployment_to_group(pool: PgPool) {
        let user_id = setup_test_environment(&pool).await;

        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        let group;
        let deployment;
        {
            let mut tx = pool.begin().await.unwrap();

            {
                let mut group_repo = Groups::new(tx.acquire().await.unwrap());
                // Create a group
                let group_create = GroupCreateDBRequest {
                    name: "Test Group".to_string(),
                    description: Some("Test group for deployment access".to_string()),
                    created_by: user_id,
                };
                group = group_repo.create(&group_create).await.expect("Failed to create test group");
            }

            {
                let mut deployment_repo = Deployments::new(tx.acquire().await.unwrap());
                // Create a deployment
                let deployment_create = DeploymentCreateDBRequest::builder()
                    .created_by(user_id)
                    .model_name("test-model".to_string())
                    .alias("test-alias".to_string())
                    .hosted_on(test_endpoint_id)
                    .build();
                deployment = deployment_repo
                    .create(&deployment_create)
                    .await
                    .expect("Failed to create test deployment");
            }

            {
                let mut group_repo = Groups::new(tx.acquire().await.unwrap());
                // Add deployment to group
                group_repo
                    .add_deployment_to_group(deployment.id, group.id, user_id)
                    .await
                    .expect("Failed to add deployment to group");
            }

            tx.commit().await.unwrap();
        }

        // Verify deployment is in group
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut pool_conn);
        let group_deployments = group_repo
            .get_group_deployments(group.id)
            .await
            .expect("Failed to get group deployments");
        assert!(group_deployments.contains(&deployment.id));

        // Verify group has access to deployment
        let deployment_groups = group_repo
            .get_deployment_groups(deployment.id)
            .await
            .expect("Failed to get deployment groups");
        assert!(deployment_groups.contains(&group.id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_remove_deployment_from_group(pool: PgPool) {
        let user_id = setup_test_environment(&pool).await;

        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        let group;
        let deployment;
        {
            let mut tx = pool.begin().await.unwrap();

            {
                let mut group_repo = Groups::new(tx.acquire().await.unwrap());
                // Create a group
                let group_create = GroupCreateDBRequest {
                    name: "Test Group".to_string(),
                    description: Some("Test group for deployment access".to_string()),
                    created_by: user_id,
                };
                group = group_repo.create(&group_create).await.expect("Failed to create test group");
            }

            {
                let mut deployment_repo = Deployments::new(tx.acquire().await.unwrap());
                // Create a deployment
                let deployment_create = DeploymentCreateDBRequest::builder()
                    .created_by(user_id)
                    .model_name("test-model".to_string())
                    .alias("test-alias".to_string())
                    .hosted_on(test_endpoint_id)
                    .build();
                deployment = deployment_repo
                    .create(&deployment_create)
                    .await
                    .expect("Failed to create test deployment");
            }

            {
                let mut group_repo = Groups::new(tx.acquire().await.unwrap());
                // Add deployment to group
                group_repo
                    .add_deployment_to_group(deployment.id, group.id, user_id)
                    .await
                    .expect("Failed to add deployment to group");
            }

            tx.commit().await.unwrap();
        }

        // Verify deployment is in group
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut pool_conn);
        let group_deployments = group_repo
            .get_group_deployments(group.id)
            .await
            .expect("Failed to get group deployments");
        assert!(group_deployments.contains(&deployment.id));

        // Remove deployment from group
        group_repo
            .remove_deployment_from_group(deployment.id, group.id)
            .await
            .expect("Failed to remove deployment from group");

        // Verify deployment is no longer in group
        let group_deployments = group_repo
            .get_group_deployments(group.id)
            .await
            .expect("Failed to get group deployments");
        assert!(!group_deployments.contains(&deployment.id));

        // Verify group no longer has access to deployment
        let deployment_groups = group_repo
            .get_deployment_groups(deployment.id)
            .await
            .expect("Failed to get deployment groups");
        assert!(!deployment_groups.contains(&group.id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_multiple_groups_per_deployment(pool: PgPool) {
        let user_id = setup_test_environment(&pool).await;

        // Create multiple groups
        let mut conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut conn);
        let mut group_ids = vec![];

        for i in 0..3 {
            let group_create = GroupCreateDBRequest {
                name: format!("Test Group {i}"),
                description: Some(format!("Test group {i} for deployment access")),
                created_by: user_id,
            };
            let group = group_repo.create(&group_create).await.expect("Failed to create test group");
            group_ids.push(group.id);
        }

        // Get a valid endpoint ID
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        // Create a deployment
        let mut deployment_pool_conn = pool.acquire().await.unwrap();
        let mut deployment_repo = Deployments::new(&mut deployment_pool_conn);
        let mut deployment_create = DeploymentCreateDBRequest::builder()
            .created_by(user_id)
            .model_name("test-model".to_string())
            .alias("test-alias".to_string())
            .build();
        deployment_create.hosted_on = test_endpoint_id;
        let deployment = deployment_repo
            .create(&deployment_create)
            .await
            .expect("Failed to create test deployment");

        // Add deployment to all groups
        for group_id in &group_ids {
            group_repo
                .add_deployment_to_group(deployment.id, *group_id, user_id)
                .await
                .expect("Failed to add deployment to group");
        }

        // Verify deployment is accessible by all groups
        let deployment_groups = group_repo
            .get_deployment_groups(deployment.id)
            .await
            .expect("Failed to get deployment groups");
        assert_eq!(deployment_groups.len(), 3);
        for group_id in &group_ids {
            assert!(deployment_groups.contains(group_id));
        }

        // Remove deployment from one group
        group_repo
            .remove_deployment_from_group(deployment.id, group_ids[0])
            .await
            .expect("Failed to remove deployment from group");

        // Verify deployment is still accessible by remaining groups
        let deployment_groups = group_repo
            .get_deployment_groups(deployment.id)
            .await
            .expect("Failed to get deployment groups");
        assert_eq!(deployment_groups.len(), 2);
        assert!(!deployment_groups.contains(&group_ids[0]));
        assert!(deployment_groups.contains(&group_ids[1]));
        assert!(deployment_groups.contains(&group_ids[2]));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_multiple_deployments_per_group(pool: PgPool) {
        let user_id = setup_test_environment(&pool).await;

        // Create a group
        let mut conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut conn);

        let group_create = GroupCreateDBRequest {
            name: "Test Group".to_string(),
            description: Some("Test group for multiple deployments".to_string()),
            created_by: user_id,
        };
        let group = group_repo.create(&group_create).await.expect("Failed to create test group");

        // Create multiple deployments
        let mut conn = pool.acquire().await.unwrap();

        let mut deployment_repo = Deployments::new(&mut conn);
        let test_endpoint_id = get_test_endpoint_id(&pool).await;
        let mut deployment_ids = vec![];

        for i in 0..3 {
            let mut deployment_create = DeploymentCreateDBRequest::builder()
                .created_by(user_id)
                .model_name(format!("test-model-{i}"))
                .alias(format!("test-alias-{i}"))
                .build();
            deployment_create.hosted_on = test_endpoint_id;
            let deployment = deployment_repo
                .create(&deployment_create)
                .await
                .expect("Failed to create test deployment");
            deployment_ids.push(deployment.id);
        }

        // Add all deployments to the group
        for deployment_id in &deployment_ids {
            group_repo
                .add_deployment_to_group(*deployment_id, group.id, user_id)
                .await
                .expect("Failed to add deployment to group");
        }

        // Verify group has access to all deployments
        let group_deployments = group_repo
            .get_group_deployments(group.id)
            .await
            .expect("Failed to get group deployments");
        assert_eq!(group_deployments.len(), 3);
        for deployment_id in &deployment_ids {
            assert!(group_deployments.contains(deployment_id));
        }

        // Remove one deployment from group
        group_repo
            .remove_deployment_from_group(deployment_ids[0], group.id)
            .await
            .expect("Failed to remove deployment from group");

        // Verify group still has access to remaining deployments
        let group_deployments = group_repo
            .get_group_deployments(group.id)
            .await
            .expect("Failed to get group deployments");
        assert_eq!(group_deployments.len(), 2);
        assert!(!group_deployments.contains(&deployment_ids[0]));
        assert!(group_deployments.contains(&deployment_ids[1]));
        assert!(group_deployments.contains(&deployment_ids[2]));
    }

    // Tests for CASCADE delete behavior

    #[sqlx::test]
    #[test_log::test]
    async fn test_cascade_delete_user_removes_group_membership(pool: PgPool) {
        let user_id = setup_test_environment(&pool).await;

        // Create a group
        let mut conn = pool.acquire().await.unwrap();

        let mut group_repo = Groups::new(&mut conn);
        let group_create = GroupCreateDBRequest {
            name: "Test Group".to_string(),
            description: Some("Test group for CASCADE delete".to_string()),
            created_by: user_id,
        };
        let group = group_repo.create(&group_create).await.expect("Failed to create test group");

        // Create another user to add to the group
        let other_user_id = UserId::new_v4();
        sqlx::query!(
            "INSERT INTO users (id, username, email, display_name, auth_source) VALUES ($1, $2, $3, $4, $5)",
            other_user_id,
            "test_user_cascade",
            "cascade@example.com",
            Some("Cascade Test User"),
            "test"
        )
        .execute(&pool)
        .await
        .expect("Failed to create test user");

        // Add user to group
        group_repo
            .add_user_to_group(other_user_id, group.id)
            .await
            .expect("Failed to add user to group");

        // Verify user is in group
        let group_users = group_repo.get_group_users(group.id).await.expect("Failed to get group users");
        assert!(group_users.contains(&other_user_id));

        // Delete the user (this should CASCADE delete the user_groups entry)
        sqlx::query!("DELETE FROM users WHERE id = $1", other_user_id)
            .execute(&pool)
            .await
            .expect("Failed to delete user");

        // Verify user is no longer in group (CASCADE delete worked)
        let group_users = group_repo.get_group_users(group.id).await.expect("Failed to get group users");
        assert!(!group_users.contains(&other_user_id));

        // Verify the user_groups entry was automatically deleted
        let user_group_count = sqlx::query_scalar!("SELECT COUNT(*) FROM user_groups WHERE user_id = $1", other_user_id)
            .fetch_one(&pool)
            .await
            .expect("Failed to count user_groups");
        assert_eq!(user_group_count.unwrap(), 0);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_cascade_delete_group_removes_all_memberships(pool: PgPool) {
        let user_id = setup_test_environment(&pool).await;

        let group;
        let mut test_user_ids = vec![];

        // Create multiple test users first
        for i in 0..3 {
            let test_user_id = UserId::new_v4();
            sqlx::query!(
                "INSERT INTO users (id, username, email, display_name, auth_source) VALUES ($1, $2, $3, $4, $5)",
                test_user_id,
                format!("cascade_user_{}", i),
                format!("cascade{}@example.com", i),
                Some(format!("Cascade User {}", i)),
                "test"
            )
            .execute(&pool)
            .await
            .expect("Failed to create test user");
            test_user_ids.push(test_user_id);
        }

        {
            let mut tx = pool.begin().await.unwrap();
            {
                let mut group_repo = Groups::new(tx.acquire().await.unwrap());
                // Create a group
                let group_create = GroupCreateDBRequest {
                    name: "Test Group CASCADE".to_string(),
                    description: Some("Test group for CASCADE delete".to_string()),
                    created_by: user_id,
                };
                group = group_repo.create(&group_create).await.expect("Failed to create test group");

                // Add all users to group
                for test_user_id in &test_user_ids {
                    group_repo
                        .add_user_to_group(*test_user_id, group.id)
                        .await
                        .expect("Failed to add user to group");
                }

                // Verify all users are in group
                let group_users = group_repo.get_group_users(group.id).await.expect("Failed to get group users");
                assert_eq!(group_users.len(), 3);
                for user_id in &test_user_ids {
                    assert!(group_users.contains(user_id));
                }

                // Delete the group (this should CASCADE delete all user_groups entries)
                group_repo.delete(group.id).await.expect("Failed to delete group");
            }
            tx.commit().await.unwrap();
        }

        // Verify all user_groups entries were automatically deleted
        for user_id in &test_user_ids {
            let user_group_count = sqlx::query_scalar!(
                "SELECT COUNT(*) FROM user_groups WHERE user_id = $1 AND group_id = $2",
                user_id,
                group.id
            )
            .fetch_one(&pool)
            .await
            .expect("Failed to count user_groups");
            assert_eq!(user_group_count.unwrap(), 0);
        }

        // Verify the group is actually deleted
        let mut pool_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut pool_conn);
        let deleted_group = group_repo.get_by_id(group.id).await.expect("Failed to check group");
        assert!(deleted_group.is_none());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_cascade_delete_user_groups_removes_api_key_deployments(pool: PgPool) {
        let user_id = setup_test_environment(&pool).await;

        // Create a group
        let mut group_conn = pool.acquire().await.unwrap();

        let mut group_repo = Groups::new(&mut group_conn);
        let group_create = GroupCreateDBRequest {
            name: "API Key CASCADE Group".to_string(),
            description: Some("Test group for API key CASCADE delete".to_string()),
            created_by: user_id,
        };
        let group = group_repo.create(&group_create).await.expect("Failed to create test group");

        // Create a test user
        let test_user_id = UserId::new_v4();
        sqlx::query!(
            "INSERT INTO users (id, username, email, display_name, auth_source) VALUES ($1, $2, $3, $4, $5)",
            test_user_id,
            "api_cascade_user",
            "apicascade@example.com",
            Some("API Cascade User"),
            "test"
        )
        .execute(&pool)
        .await
        .expect("Failed to create test user");

        // Create a deployment
        let deployment;
        {
            let mut pool_conn = pool.acquire().await.unwrap();
            let mut deployment_repo = Deployments::new(&mut pool_conn);
            let test_endpoint_id = get_test_endpoint_id(&pool).await;
            let mut deployment_create = DeploymentCreateDBRequest::builder()
                .created_by(user_id)
                .model_name("cascade-model".to_string())
                .alias("cascade-alias".to_string())
                .build();
            deployment_create.hosted_on = test_endpoint_id;
            deployment = deployment_repo
                .create(&deployment_create)
                .await
                .expect("Failed to create deployment");
        }

        // Add user to group FIRST
        group_repo
            .add_user_to_group(test_user_id, group.id)
            .await
            .expect("Failed to add user to group");

        // Add deployment to group
        group_repo
            .add_deployment_to_group(deployment.id, group.id, user_id)
            .await
            .expect("Failed to add deployment to group");

        // Add credits to user
        {
            let mut pool_conn = pool.acquire().await.unwrap();
            let mut credits_repo = Credits::new(&mut pool_conn);
            credits_repo
                .create_transaction(&CreditTransactionCreateDBRequest {
                    user_id: test_user_id,
                    transaction_type: CreditTransactionType::Purchase,
                    amount: Decimal::new(1000, 2),
                    source_id: "test".to_string(),
                    description: Some("Added credits".to_string()),
                })
                .await
                .unwrap();
        }

        // NOW create an API key - this will automatically create api_key_deployments entries
        let mut conn = pool.acquire().await.unwrap();

        let mut api_key_repo = ApiKeys::new(&mut conn);
        let api_key_create = ApiKeyCreateDBRequest {
            user_id: test_user_id,
            name: "CASCADE Test Key".to_string(),
            description: Some("API key for CASCADE delete test".to_string()),
            purpose: ApiKeyPurpose::Realtime,
            requests_per_second: None,
            burst_size: None,
        };
        let api_key = api_key_repo.create(&api_key_create).await.expect("Failed to create API key");

        // Verify API key has access to deployment (should include this deployment in model_access)
        assert!(api_key.model_access.contains(&deployment.id));

        // Verify API key has access to deployment through group membership
        let keys_for_deployment = api_key_repo
            .get_api_keys_for_deployment_with_sufficient_credit(deployment.id)
            .await
            .expect("Failed to get keys for deployment");
        assert!(keys_for_deployment.iter().any(|k| k.secret == api_key.secret));

        // Remove user from group (this should CASCADE delete user_groups and api_key_deployments)
        group_repo
            .remove_user_from_group(test_user_id, group.id)
            .await
            .expect("Failed to remove user from group");

        // Verify user_groups entry was deleted
        let user_group_count = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM user_groups WHERE user_id = $1 AND group_id = $2",
            test_user_id,
            group.id
        )
        .fetch_one(&pool)
        .await
        .expect("Failed to count user_groups");
        assert_eq!(user_group_count.unwrap(), 0);

        // Verify API key no longer has access to deployment (get updated model_access)
        let updated_api_key = api_key_repo
            .get_by_id(api_key.id)
            .await
            .expect("Failed to get API key")
            .expect("API key should exist");
        assert!(!updated_api_key.model_access.contains(&deployment.id));

        // Verify API key no longer has access to deployment
        let keys_for_deployment = api_key_repo
            .get_api_keys_for_deployment_with_sufficient_credit(deployment.id)
            .await
            .expect("Failed to get keys for deployment");
        assert!(!keys_for_deployment.iter().any(|k| k.secret == api_key.secret));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_cascade_delete_deployment_removes_access_entries(pool: PgPool) {
        let user_id = setup_test_environment(&pool).await;

        // Create a group
        let mut conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut conn);

        let group_create = GroupCreateDBRequest {
            name: "Deployment CASCADE Group".to_string(),
            description: Some("Test group for deployment CASCADE delete".to_string()),
            created_by: user_id,
        };
        let group = group_repo.create(&group_create).await.expect("Failed to create test group");

        // Get a valid endpoint ID
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        // Create a deployment
        let mut conn = pool.acquire().await.unwrap();
        let mut deployment_repo = Deployments::new(&mut conn);

        let mut deployment_create = DeploymentCreateDBRequest::builder()
            .created_by(user_id)
            .model_name("delete-cascade-model".to_string())
            .alias("delete-cascade-alias".to_string())
            .build();
        deployment_create.hosted_on = test_endpoint_id;
        let deployment = deployment_repo
            .create(&deployment_create)
            .await
            .expect("Failed to create deployment");

        // Add deployment to group
        group_repo
            .add_deployment_to_group(deployment.id, group.id, user_id)
            .await
            .expect("Failed to add deployment to group");

        // Verify deployment_groups entry exists
        let deployment_group_count = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM deployment_groups WHERE deployment_id = $1 AND group_id = $2",
            deployment.id,
            group.id
        )
        .fetch_one(&pool)
        .await
        .expect("Failed to count deployment_groups");
        assert_eq!(deployment_group_count.unwrap(), 1);

        // Delete the deployment (hard delete)
        deployment_repo.delete(deployment.id).await.expect("Failed to delete deployment");

        // Verify deployment_groups relationship is removed (hard deletion cascades)
        let deployment_group_count = sqlx::query_scalar!(
            "SELECT COUNT(*) FROM deployment_groups WHERE deployment_id = $1 AND group_id = $2",
            deployment.id,
            group.id
        )
        .fetch_one(&pool)
        .await
        .expect("Failed to count deployment_groups");
        assert_eq!(deployment_group_count.unwrap(), 0); // Relationship removed

        // Verify deployment is completely removed from the database
        let deployment_count = sqlx::query_scalar!("SELECT COUNT(*) FROM deployed_models WHERE id = $1", deployment.id)
            .fetch_one(&pool)
            .await
            .expect("Failed to count deployments");
        assert_eq!(deployment_count.unwrap(), 0);

        // Verify group's deployment list no longer includes the deleted deployment
        let group_deployments = group_repo
            .get_group_deployments(group.id)
            .await
            .expect("Failed to get group deployments");
        assert!(!group_deployments.contains(&deployment.id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_bulk_relationship_fetching_methods(pool: PgPool) {
        let user_id = setup_test_environment(&pool).await;
        let mut group_conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut group_conn);

        let mut depl_con = pool.acquire().await.unwrap();
        let mut deployment_repo = Deployments::new(&mut depl_con);

        // Create multiple groups
        let mut group_ids = vec![];
        for i in 0..3 {
            let group_create = GroupCreateDBRequest {
                name: format!("Test Group {i}"),
                description: Some(format!("Test group {i} for bulk testing")),
                created_by: user_id,
            };
            let group = group_repo.create(&group_create).await.expect("Failed to create test group");
            group_ids.push(group.id);
        }

        // Create multiple users
        let mut user_ids = vec![user_id];
        for i in 1..3 {
            let user_create_id = UserId::new_v4();
            sqlx::query!(
                "INSERT INTO users (id, username, email, display_name, auth_source) VALUES ($1, $2, $3, $4, $5)",
                user_create_id,
                format!("testuser{i}"),
                format!("testuser{i}@example.com"),
                Some(format!("Test User {i}")),
                "test"
            )
            .execute(&pool)
            .await
            .expect("Failed to create test user");
            user_ids.push(user_create_id);
        }

        // Get a valid endpoint ID
        let test_endpoint_id = get_test_endpoint_id(&pool).await;

        // Create multiple deployments
        let mut deployment_ids = vec![];
        for i in 0..3 {
            let mut deployment_create = DeploymentCreateDBRequest::builder()
                .created_by(user_id)
                .model_name(format!("test-model-{i}"))
                .alias(format!("test-alias-{i}"))
                .build();
            deployment_create.hosted_on = test_endpoint_id;
            let deployment = deployment_repo
                .create(&deployment_create)
                .await
                .expect("Failed to create test deployment");
            deployment_ids.push(deployment.id);
        }

        // Add users to groups (user 0 -> group 0, user 1 -> groups 0,1, user 2 -> group 2)
        group_repo
            .add_user_to_group(user_ids[0], group_ids[0])
            .await
            .expect("Failed to add user to group");
        group_repo
            .add_user_to_group(user_ids[1], group_ids[0])
            .await
            .expect("Failed to add user to group");
        group_repo
            .add_user_to_group(user_ids[1], group_ids[1])
            .await
            .expect("Failed to add user to group");
        group_repo
            .add_user_to_group(user_ids[2], group_ids[2])
            .await
            .expect("Failed to add user to group");

        // Add deployments to groups (deployment 0 -> group 0, deployment 1 -> groups 0,1, deployment 2 -> group 2)
        group_repo
            .add_deployment_to_group(deployment_ids[0], group_ids[0], user_id)
            .await
            .expect("Failed to add deployment to group");
        group_repo
            .add_deployment_to_group(deployment_ids[1], group_ids[0], user_id)
            .await
            .expect("Failed to add deployment to group");
        group_repo
            .add_deployment_to_group(deployment_ids[1], group_ids[1], user_id)
            .await
            .expect("Failed to add deployment to group");
        group_repo
            .add_deployment_to_group(deployment_ids[2], group_ids[2], user_id)
            .await
            .expect("Failed to add deployment to group");

        // Test get_groups_users_bulk
        let groups_users = group_repo
            .get_groups_users_bulk(&group_ids)
            .await
            .expect("Failed to get groups users bulk");

        // Group 0 should have users 0 and 1
        let group0_users = groups_users.get(&group_ids[0]).unwrap();
        assert_eq!(group0_users.len(), 2);
        assert!(group0_users.contains(&user_ids[0]));
        assert!(group0_users.contains(&user_ids[1]));

        // Group 1 should have user 1
        let group1_users = groups_users.get(&group_ids[1]).unwrap();
        assert_eq!(group1_users.len(), 1);
        assert!(group1_users.contains(&user_ids[1]));

        // Group 2 should have user 2
        let group2_users = groups_users.get(&group_ids[2]).unwrap();
        assert_eq!(group2_users.len(), 1);
        assert!(group2_users.contains(&user_ids[2]));

        // Test get_users_groups_bulk
        let users_groups = group_repo
            .get_users_groups_bulk(&user_ids)
            .await
            .expect("Failed to get users groups bulk");

        let everyone_group_id = Uuid::nil();

        // User 0 should be in group 0 + Everyone
        let user0_groups = users_groups.get(&user_ids[0]).unwrap();
        assert_eq!(user0_groups.len(), 2); // group 0 + Everyone
        assert!(user0_groups.contains(&group_ids[0]));
        assert!(user0_groups.contains(&everyone_group_id));

        // User 1 should be in groups 0, 1 + Everyone
        let user1_groups = users_groups.get(&user_ids[1]).unwrap();
        assert_eq!(user1_groups.len(), 3); // groups 0, 1 + Everyone
        assert!(user1_groups.contains(&group_ids[0]));
        assert!(user1_groups.contains(&group_ids[1]));
        assert!(user1_groups.contains(&everyone_group_id));

        // User 2 should be in group 2 + Everyone
        let user2_groups = users_groups.get(&user_ids[2]).unwrap();
        assert_eq!(user2_groups.len(), 2); // group 2 + Everyone
        assert!(user2_groups.contains(&group_ids[2]));
        assert!(user2_groups.contains(&everyone_group_id));

        // Test get_groups_deployments_bulk
        let groups_deployments = group_repo
            .get_groups_deployments_bulk(&group_ids)
            .await
            .expect("Failed to get groups deployments bulk");

        // Group 0 should have deployments 0 and 1
        let group0_deployments = groups_deployments.get(&group_ids[0]).unwrap();
        assert_eq!(group0_deployments.len(), 2);
        assert!(group0_deployments.contains(&deployment_ids[0]));
        assert!(group0_deployments.contains(&deployment_ids[1]));

        // Group 1 should have deployment 1
        let group1_deployments = groups_deployments.get(&group_ids[1]).unwrap();
        assert_eq!(group1_deployments.len(), 1);
        assert!(group1_deployments.contains(&deployment_ids[1]));

        // Group 2 should have deployment 2
        let group2_deployments = groups_deployments.get(&group_ids[2]).unwrap();
        assert_eq!(group2_deployments.len(), 1);
        assert!(group2_deployments.contains(&deployment_ids[2]));

        // Test get_deployments_groups_bulk
        let deployments_groups = group_repo
            .get_deployments_groups_bulk(&deployment_ids)
            .await
            .expect("Failed to get deployments groups bulk");

        // Deployment 0 should be in group 0
        let deployment0_groups = deployments_groups.get(&deployment_ids[0]).unwrap();
        assert_eq!(deployment0_groups.len(), 1);
        assert!(deployment0_groups.contains(&group_ids[0]));

        // Deployment 1 should be in groups 0 and 1
        let deployment1_groups = deployments_groups.get(&deployment_ids[1]).unwrap();
        assert_eq!(deployment1_groups.len(), 2);
        assert!(deployment1_groups.contains(&group_ids[0]));
        assert!(deployment1_groups.contains(&group_ids[1]));

        // Deployment 2 should be in group 2
        let deployment2_groups = deployments_groups.get(&deployment_ids[2]).unwrap();
        assert_eq!(deployment2_groups.len(), 1);
        assert!(deployment2_groups.contains(&group_ids[2]));

        // Test with empty input - should return empty hashmap
        let empty_result = group_repo
            .get_groups_users_bulk(&[])
            .await
            .expect("Failed to get empty groups users bulk");
        assert!(empty_result.is_empty());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_everyone_group_virtual_membership(pool: PgPool) {
        let user_id = setup_test_environment(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut conn);

        // Create additional test users
        let user2_id = UserId::new_v4();
        let user3_id = UserId::new_v4();

        for (i, id) in [(2, user2_id), (3, user3_id)] {
            sqlx::query!(
                "INSERT INTO users (id, username, email, display_name, auth_source) VALUES ($1, $2, $3, $4, $5)",
                id,
                format!("testuser{}", i),
                format!("testuser{}@example.com", i),
                Some(format!("Test User {}", i)),
                "test"
            )
            .execute(&pool)
            .await
            .expect("Failed to create test user");
        }

        let everyone_group_id = Uuid::nil();

        // Test that Everyone group contains all users
        let everyone_users = group_repo
            .get_group_users(everyone_group_id)
            .await
            .expect("Failed to get Everyone group users");

        assert_eq!(everyone_users.len(), 3); // Should contain all 3 test users
        assert!(everyone_users.contains(&user_id));
        assert!(everyone_users.contains(&user2_id));
        assert!(everyone_users.contains(&user3_id));

        // Test that every user belongs to Everyone group
        for test_user_id in [user_id, user2_id, user3_id] {
            let user_groups = group_repo.get_user_groups(test_user_id).await.expect("Failed to get user groups");

            // Should contain at least the Everyone group
            assert!(!user_groups.is_empty());
            let everyone_group = user_groups.iter().find(|g| g.id == everyone_group_id);
            assert!(everyone_group.is_some(), "User should belong to Everyone group");
            assert_eq!(everyone_group.unwrap().name, "Everyone");
        }

        // Test bulk methods include Everyone group
        let users_groups = group_repo
            .get_users_groups_bulk(&[user_id, user2_id, user3_id])
            .await
            .expect("Failed to get users groups bulk");

        for test_user_id in [user_id, user2_id, user3_id] {
            let user_groups = users_groups.get(&test_user_id).unwrap();
            assert!(user_groups.contains(&everyone_group_id), "Bulk query should include Everyone group");
        }

        // Test that Everyone group appears in groups_users_bulk
        let groups_users = group_repo
            .get_groups_users_bulk(&[everyone_group_id])
            .await
            .expect("Failed to get groups users bulk");

        let everyone_users_bulk = groups_users.get(&everyone_group_id).unwrap();
        assert_eq!(everyone_users_bulk.len(), 3);
        assert!(everyone_users_bulk.contains(&user_id));
        assert!(everyone_users_bulk.contains(&user2_id));
        assert!(everyone_users_bulk.contains(&user3_id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_everyone_group_with_regular_groups(pool: PgPool) {
        let user_id = setup_test_environment(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut conn);

        // Create a regular group
        let regular_group_create = GroupCreateDBRequest {
            name: "Regular Group".to_string(),
            description: Some("A normal group".to_string()),
            created_by: user_id,
        };
        let regular_group = group_repo
            .create(&regular_group_create)
            .await
            .expect("Failed to create regular group");

        // Add user to the regular group
        group_repo
            .add_user_to_group(user_id, regular_group.id)
            .await
            .expect("Failed to add user to regular group");

        // User should be in both regular group and Everyone group
        let user_groups = group_repo.get_user_groups(user_id).await.expect("Failed to get user groups");

        assert_eq!(user_groups.len(), 2); // Regular group + Everyone group

        let group_names: Vec<&str> = user_groups.iter().map(|g| g.name.as_str()).collect();
        assert!(group_names.contains(&"Regular Group"));
        assert!(group_names.contains(&"Everyone"));

        // Test bulk operations with mixed group types
        let everyone_group_id = Uuid::nil();
        let groups_users = group_repo
            .get_groups_users_bulk(&[regular_group.id, everyone_group_id])
            .await
            .expect("Failed to get groups users bulk");

        // Regular group should have just the user we added
        let regular_users = groups_users.get(&regular_group.id).unwrap();
        assert_eq!(regular_users.len(), 1);
        assert!(regular_users.contains(&user_id));

        // Everyone group should have all users (in this case, just the setup user)
        let everyone_users = groups_users.get(&everyone_group_id).unwrap();
        assert_eq!(everyone_users.len(), 1);
        assert!(everyone_users.contains(&user_id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_everyone_group_excludes_system_user(pool: PgPool) {
        let user_id = setup_test_environment(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut conn);

        let everyone_group_id = Uuid::nil();
        let system_user_id = Uuid::nil(); // Same as Everyone group ID, but in user context

        // Everyone group should not contain system user
        let everyone_users = group_repo
            .get_group_users(everyone_group_id)
            .await
            .expect("Failed to get Everyone group users");

        assert!(
            !everyone_users.contains(&system_user_id),
            "Everyone group should not contain system user"
        );
        assert!(everyone_users.contains(&user_id), "Everyone group should contain regular users");

        // System user should not appear in user groups queries either
        // (This is already handled by the users repository filtering, but let's verify)
        let mut conn = pool.acquire().await.unwrap();

        let mut users_repo = Users::new(&mut conn);
        let all_users = users_repo.list(&UserFilter::new(0, 100)).await.expect("Failed to list users");

        // Should not contain system user
        assert!(!all_users.iter().any(|u| u.id == system_user_id));
        // Should contain our test user
        assert!(all_users.iter().any(|u| u.id == user_id));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_everyone_group_cannot_be_deleted(pool: PgPool) {
        let mut conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut conn);
        let everyone_group_id = Uuid::nil();

        // Attempt to delete the Everyone group should fail
        let result = group_repo.delete(everyone_group_id).await;
        assert!(result.is_err(), "Deleting Everyone group should fail");

        let error_message = format!("{}", result.unwrap_err());
        assert!(error_message.contains("Cannot delete the Everyone group"));

        // Verify the Everyone group still exists
        let everyone_group = group_repo
            .get_by_id(everyone_group_id)
            .await
            .expect("Failed to query Everyone group")
            .expect("Everyone group should still exist");
        assert_eq!(everyone_group.name, "Everyone");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_group_with_all_fields(pool: PgPool) {
        let user_id = setup_test_environment(&pool).await;

        let group;
        {
            let mut conn = pool.acquire().await.unwrap();
            let mut group_repo = Groups::new(&mut conn);

            // Create a group to update
            let group_create = GroupCreateDBRequest {
                name: "Original Group".to_string(),
                description: Some("Original description".to_string()),
                created_by: user_id,
            };
            group = group_repo.create(&group_create).await.expect("Failed to create test group");

            // Update with both name and description
            let update_request = GroupUpdateDBRequest {
                name: Some("Updated Group Name".to_string()),
                description: Some("Updated description".to_string()),
            };

            let updated_group = group_repo.update(group.id, &update_request).await.expect("Failed to update group");

            // Verify all fields were updated
            assert_eq!(updated_group.id, group.id);
            assert_eq!(updated_group.name, "Updated Group Name");
            assert_eq!(updated_group.description, Some("Updated description".to_string()));
            assert_eq!(updated_group.created_by, user_id);
            assert_eq!(updated_group.created_at, group.created_at);
            assert!(updated_group.updated_at >= group.updated_at);

            // Verify the update persisted in the database
            let retrieved_group = group_repo
                .get_by_id(group.id)
                .await
                .expect("Failed to retrieve group")
                .expect("Group should exist");

            assert_eq!(retrieved_group.name, "Updated Group Name");
            assert_eq!(retrieved_group.description, Some("Updated description".to_string()));
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_group_with_partial_fields_name_only(pool: PgPool) {
        let user_id = setup_test_environment(&pool).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut conn);

        // Create a group to update
        let group_create = GroupCreateDBRequest {
            name: "Original Group".to_string(),
            description: Some("Original description".to_string()),
            created_by: user_id,
        };
        let group = group_repo.create(&group_create).await.expect("Failed to create test group");

        // Update only the name
        let update_request = GroupUpdateDBRequest {
            name: Some("Updated Name Only".to_string()),
            description: None,
        };

        let updated_group = group_repo.update(group.id, &update_request).await.expect("Failed to update group");

        // Verify only name was updated, description unchanged
        assert_eq!(updated_group.name, "Updated Name Only");
        assert_eq!(updated_group.description, Some("Original description".to_string()));
        assert!(updated_group.updated_at >= group.updated_at);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_group_with_partial_fields_description_only(pool: PgPool) {
        let user_id = setup_test_environment(&pool).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut conn);

        // Create a group to update
        let group_create = GroupCreateDBRequest {
            name: "Original Group".to_string(),
            description: Some("Original description".to_string()),
            created_by: user_id,
        };
        let group = group_repo.create(&group_create).await.expect("Failed to create test group");

        // Update only the description
        let update_request = GroupUpdateDBRequest {
            name: None,
            description: Some("Updated description only".to_string()),
        };

        let updated_group = group_repo.update(group.id, &update_request).await.expect("Failed to update group");

        // Verify only description was updated, name unchanged
        assert_eq!(updated_group.name, "Original Group");
        assert_eq!(updated_group.description, Some("Updated description only".to_string()));
        assert!(updated_group.updated_at >= group.updated_at);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_group_clear_description(pool: PgPool) {
        let user_id = setup_test_environment(&pool).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut conn);

        // Create a group with description
        let group_create = GroupCreateDBRequest {
            name: "Test Group".to_string(),
            description: Some("Has description".to_string()),
            created_by: user_id,
        };
        let group = group_repo.create(&group_create).await.expect("Failed to create test group");

        // Clear the description by setting it to Some("".to_string())
        let update_request = GroupUpdateDBRequest {
            name: None,
            description: Some("".to_string()),
        };

        let updated_group = group_repo.update(group.id, &update_request).await.expect("Failed to update group");

        // Verify description was cleared
        assert_eq!(updated_group.name, "Test Group");
        assert_eq!(updated_group.description, Some("".to_string()));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_group_with_no_changes(pool: PgPool) {
        let user_id = setup_test_environment(&pool).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut conn);

        // Create a group to update
        let group_create = GroupCreateDBRequest {
            name: "Original Group".to_string(),
            description: Some("Original description".to_string()),
            created_by: user_id,
        };
        let group = group_repo.create(&group_create).await.expect("Failed to create test group");

        // Update with no changes (all None)
        let update_request = GroupUpdateDBRequest {
            name: None,
            description: None,
        };

        let updated_group = group_repo.update(group.id, &update_request).await.expect("Failed to update group");

        // Verify values unchanged but updated_at changed
        assert_eq!(updated_group.name, "Original Group");
        assert_eq!(updated_group.description, Some("Original description".to_string()));
        assert!(updated_group.updated_at >= group.updated_at);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_nonexistent_group(pool: PgPool) {
        let _user_id = setup_test_environment(&pool).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut conn);

        let nonexistent_id = GroupId::new_v4();
        let update_request = GroupUpdateDBRequest {
            name: Some("Updated Name".to_string()),
            description: Some("Updated description".to_string()),
        };

        // Attempt to update nonexistent group should fail
        let result = group_repo.update(nonexistent_id, &update_request).await;
        assert!(result.is_err(), "Updating nonexistent group should fail");

        // Verify it's a NotFound error
        match result.unwrap_err() {
            DbError::NotFound => {} // Expected
            other => panic!("Expected NotFound error, got: {other:?}"),
        }
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_update_everyone_group_fails(pool: PgPool) {
        let _user_id = setup_test_environment(&pool).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut group_repo = Groups::new(&mut conn);

        let everyone_group_id = Uuid::nil();
        let update_request = GroupUpdateDBRequest {
            name: Some("Hacked Everyone".to_string()),
            description: Some("Trying to hack".to_string()),
        };

        // Attempt to update Everyone group should fail
        let result = group_repo.update(everyone_group_id, &update_request).await;
        assert!(result.is_err(), "Updating Everyone group should fail");

        // Verify it's a ProtectedEntity error
        match result.unwrap_err() {
            DbError::ProtectedEntity {
                operation,
                reason,
                entity_type,
                ..
            } => {
                assert_eq!(operation, Operation::UpdateAll);
                assert!(reason.contains("Cannot update the Everyone group"));
                assert_eq!(entity_type, "Group");
            }
            other => panic!("Expected ProtectedEntity error, got: {other:?}"),
        }

        // Verify Everyone group is unchanged
        let everyone_group = group_repo
            .get_by_id(everyone_group_id)
            .await
            .expect("Failed to get Everyone group")
            .expect("Everyone group should exist");
        assert_eq!(everyone_group.name, "Everyone");
    }

    #[test]
    fn test_apply_update_trait_all_fields() {
        use chrono::Utc;

        // Create a mock group response
        let original_time = Utc::now();
        let group = GroupDBResponse {
            id: GroupId::new_v4(),
            name: "Original Group".to_string(),
            description: Some("Original description".to_string()),
            created_by: UserId::new_v4(),
            created_at: original_time,
            updated_at: original_time,
            source: "native".to_string(),
        };

        // Test ApplyUpdate trait directly
        let update_request = GroupUpdateDBRequest {
            name: Some("Applied Name".to_string()),
            description: Some("Applied description".to_string()),
        };

        let updated = mock_coalesce_update(&update_request, &group);

        // Verify ApplyUpdate behavior
        assert_eq!(updated.id, group.id);
        assert_eq!(updated.name, "Applied Name");
        assert_eq!(updated.description, Some("Applied description".to_string()));
        assert_eq!(updated.created_by, group.created_by);
        assert_eq!(updated.created_at, group.created_at);
        assert!(updated.updated_at > group.updated_at);
    }

    #[test]
    fn test_apply_update_trait_partial_fields() {
        use chrono::Utc;

        // Create a mock group response
        let original_time = Utc::now();
        let group = GroupDBResponse {
            id: GroupId::new_v4(),
            name: "Original Group".to_string(),
            description: Some("Original description".to_string()),
            created_by: UserId::new_v4(),
            created_at: original_time,
            updated_at: original_time,
            source: "native".to_string(),
        };

        // Test ApplyUpdate with only name
        let update_request = GroupUpdateDBRequest {
            name: Some("Applied Name Only".to_string()),
            description: None,
        };

        let updated = mock_coalesce_update(&update_request, &group);

        // Verify only name was updated
        assert_eq!(updated.name, "Applied Name Only");
        assert_eq!(updated.description, Some("Original description".to_string()));
        assert!(updated.updated_at > group.updated_at);

        // Test ApplyUpdate with only description
        let update_request2 = GroupUpdateDBRequest {
            name: None,
            description: Some("Applied description only".to_string()),
        };

        let updated2 = mock_coalesce_update(&update_request2, &group);

        // Verify only description was updated
        assert_eq!(updated2.name, "Original Group");
        assert_eq!(updated2.description, Some("Applied description only".to_string()));
        assert!(updated2.updated_at > group.updated_at);
    }

    #[test]
    fn test_apply_update_trait_no_changes() {
        use chrono::Utc;

        // Create a mock group response
        let original_time = Utc::now();
        let group = GroupDBResponse {
            id: GroupId::new_v4(),
            name: "Original Group".to_string(),
            description: Some("Original description".to_string()),
            created_by: UserId::new_v4(),
            created_at: original_time,
            updated_at: original_time,
            source: "native".to_string(),
        };

        // Test ApplyUpdate with no changes
        let update_request = GroupUpdateDBRequest {
            name: None,
            description: None,
        };

        let updated = mock_coalesce_update(&update_request, &group);

        // Verify values unchanged but updated_at changed
        assert_eq!(updated.name, "Original Group");
        assert_eq!(updated.description, Some("Original description".to_string()));
        assert!(updated.updated_at > group.updated_at);
    }

    #[test]
    fn test_apply_update_trait_clears_description() {
        use chrono::Utc;

        // Create a mock group response with description
        let original_time = Utc::now();
        let group = GroupDBResponse {
            id: GroupId::new_v4(),
            name: "Test Group".to_string(),
            description: Some("Has description".to_string()),
            created_by: UserId::new_v4(),
            created_at: original_time,
            updated_at: original_time,
            source: "native".to_string(),
        };

        // Test clearing description with empty string
        let update_request = GroupUpdateDBRequest {
            name: None,
            description: Some("".to_string()),
        };

        let updated = mock_coalesce_update(&update_request, &group);

        // Verify description was cleared to empty string
        assert_eq!(updated.name, "Test Group");
        assert_eq!(updated.description, Some("".to_string()));
        assert!(updated.updated_at > group.updated_at);
    }
}
