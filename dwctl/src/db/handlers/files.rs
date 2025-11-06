use crate::db::{
    errors::{DbError, Result},
    handlers::repository::Repository,
    models::files::{FileCreateDBRequest, FileDBResponse, FileStatus, FileUpdateDBRequest},
};
use crate::types::{abbrev_uuid, FileId, UserId};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{query_builder::QueryBuilder, FromRow, PgConnection};
use std::collections::HashMap;
use tracing::instrument;

/// Filter for listing files
#[derive(Debug, Clone)]
pub struct FileFilter {
    pub skip: i64,
    pub limit: i64,
    pub uploaded_by: Option<UserId>,
    pub status: Option<FileStatus>,
}

impl FileFilter {
    pub fn new(skip: i64, limit: i64) -> Self {
        Self {
            skip,
            limit,
            uploaded_by: None,
            status: None,
        }
    }

    pub fn with_uploaded_by(mut self, user_id: UserId) -> Self {
        self.uploaded_by = Some(user_id);
        self
    }

    pub fn with_status(mut self, status: FileStatus) -> Self {
        self.status = Some(status);
        self
    }
}

// Database entity model
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
struct File {
    pub id: FileId,
    pub filename: String,
    pub size_bytes: i64,
    pub status: String,
    pub error_message: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub uploaded_by: UserId,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<File> for FileDBResponse {
    fn from(f: File) -> Self {
        Self {
            id: f.id,
            filename: f.filename,
            size_bytes: f.size_bytes,
            status: FileStatus::from_db_string(&f.status),
            error_message: f.error_message,
            expires_at: f.expires_at,
            deleted_at: f.deleted_at,
            uploaded_by: f.uploaded_by,
            created_at: f.created_at,
            updated_at: f.updated_at,
        }
    }
}

pub struct Files<'c> {
    db: &'c mut PgConnection,
}

impl<'c> Files<'c> {
    pub fn new(db: &'c mut PgConnection) -> Self {
        Self { db }
    }

    /// Soft-delete file - marks as deleted
    /// Metadata is retained for audit purposes
    #[instrument(skip(self), fields(file_id = %abbrev_uuid(&id)), err)]
    pub async fn soft_delete(&mut self, id: FileId) -> Result<bool> {
        let result = sqlx::query!(
            r#"
            UPDATE files
            SET status = 'deleted', deleted_at = NOW(), updated_at = NOW()
            WHERE id = $1 AND status NOT IN ('deleted', 'expired')
            "#,
            id
        )
        .execute(&mut *self.db)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Mark a file as having an error during processing
    #[instrument(skip(self), fields(file_id = %abbrev_uuid(&id)), err)]
    #[cfg(test)]
    pub async fn mark_error(&mut self, id: FileId, error_message: String) -> Result<bool> {
        let result = sqlx::query!(
            r#"
            UPDATE files
            SET status = 'error', error_message = $2, updated_at = NOW()
            WHERE id = $1
            "#,
            id,
            error_message
        )
        .execute(&mut *self.db)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Mark files as expired (called by cleanup job)
    #[instrument(skip(self), err)]
    #[cfg(test)]
    pub async fn mark_expired(&mut self) -> Result<Vec<FileId>> {
        let expired_ids = sqlx::query_scalar!(
            r#"
            UPDATE files
            SET status = 'expired', updated_at = NOW()
            WHERE status = 'processed'
              AND expires_at IS NOT NULL
              AND expires_at <= NOW()
            RETURNING id
            "#,
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(expired_ids)
    }
}

#[async_trait]
impl<'c> Repository for Files<'c> {
    type CreateRequest = FileCreateDBRequest;
    type UpdateRequest = FileUpdateDBRequest;
    type Response = FileDBResponse;
    type Id = FileId;
    type Filter = FileFilter;

    #[instrument(skip(self, request), fields(filename = %request.filename), err)]
    async fn create(&mut self, request: &Self::CreateRequest) -> Result<Self::Response> {
        let created_at = Utc::now();
        let updated_at = created_at;
        let status_str = request.status.to_db_string();

        let file = sqlx::query_as!(
            File,
            r#"
            INSERT INTO files (id, filename, size_bytes, uploaded_by, status, error_message, expires_at, created_at, updated_at)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            RETURNING *
            "#,
            request.id, // <-- Use the provided ID
            request.filename,
            request.size_bytes,
            request.uploaded_by,
            status_str,
            request.error_message,
            request.expires_at,
            created_at,
            updated_at
        )
        .fetch_one(&mut *self.db)
        .await?;

        Ok(FileDBResponse::from(file))
    }

    #[instrument(skip(self), fields(file_id = %abbrev_uuid(&id)), err)]
    async fn get_by_id(&mut self, id: Self::Id) -> Result<Option<Self::Response>> {
        let file = sqlx::query_as!(File, "SELECT * FROM files WHERE id = $1", id)
            .fetch_optional(&mut *self.db)
            .await?;

        Ok(file.map(FileDBResponse::from))
    }

    #[instrument(skip(self, ids), fields(count = ids.len()), err)]
    async fn get_bulk(&mut self, ids: Vec<Self::Id>) -> Result<HashMap<Self::Id, Self::Response>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }

        let files = sqlx::query_as!(File, "SELECT * FROM files WHERE id = ANY($1)", &ids)
            .fetch_all(&mut *self.db)
            .await?;

        Ok(files.into_iter().map(|f| (f.id, FileDBResponse::from(f))).collect())
    }

    #[instrument(skip(self, filter), fields(limit = filter.limit, skip = filter.skip), err)]
    async fn list(&mut self, filter: &Self::Filter) -> Result<Vec<Self::Response>> {
        let mut query = QueryBuilder::new("SELECT * FROM files WHERE 1=1");

        // Add uploaded_by filter if specified
        if let Some(user_id) = filter.uploaded_by {
            query.push(" AND uploaded_by = ");
            query.push_bind(user_id);
        }

        // Add status filter if specified
        if let Some(status) = filter.status {
            query.push(" AND status = ");
            query.push_bind(status.to_db_string());
        }

        // Add ordering and pagination
        query.push(" ORDER BY created_at DESC LIMIT ");
        query.push_bind(filter.limit);
        query.push(" OFFSET ");
        query.push_bind(filter.skip);

        let files = query.build_query_as::<File>().fetch_all(&mut *self.db).await?;

        Ok(files.into_iter().map(FileDBResponse::from).collect())
    }

    #[instrument(skip(self), fields(file_id = %abbrev_uuid(&id)), err)]
    async fn delete(&mut self, id: Self::Id) -> Result<bool> {
        // Hard delete from database (use soft_delete for normal deletions)
        let result = sqlx::query!("DELETE FROM files WHERE id = $1", id).execute(&mut *self.db).await?;

        Ok(result.rows_affected() > 0)
    }

    #[instrument(skip(self, request), fields(file_id = %abbrev_uuid(&id)), err)]
    async fn update(&mut self, id: Self::Id, request: &Self::UpdateRequest) -> Result<Self::Response> {
        let status_str = request.status.as_ref().map(|s| s.to_db_string());

        let file = sqlx::query_as!(
            File,
            r#"
            UPDATE files
            SET
                filename = COALESCE($2, filename),
                status = COALESCE($3, status),
                error_message = CASE
                    WHEN $4 THEN $5
                    ELSE error_message
                END,
                deleted_at = COALESCE($6, deleted_at),
                updated_at = NOW()
            WHERE id = $1
            RETURNING *
            "#,
            id,
            request.filename,
            status_str,
            request.error_message.is_some() as bool,
            request.error_message.as_ref().and_then(|inner| inner.as_ref()),
            request.deleted_at
        )
        .fetch_optional(&mut *self.db)
        .await?
        .ok_or(DbError::NotFound)?;

        Ok(FileDBResponse::from(file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::models::users::{Role, UserCreate},
        db::handlers::Users,
        db::models::users::UserCreateDBRequest,
    };
    use sqlx::PgPool;
    use uuid::Uuid;

    async fn create_test_user(pool: &PgPool) -> UserId {
        let mut tx = pool.begin().await.unwrap();
        let mut user_repo = Users::new(&mut tx);

        let user_create = UserCreateDBRequest::from(UserCreate {
            username: format!("fileuser_{}", Uuid::new_v4()),
            email: format!("file_{}@example.com", Uuid::new_v4()),
            display_name: None,
            avatar_url: None,
            roles: vec![Role::StandardUser],
        });

        let user = user_repo.create(&user_create).await.unwrap();
        tx.commit().await.unwrap();
        user.id
    }

    #[sqlx::test]
    async fn test_create_file(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Files::new(&mut conn);

        let create_request = FileCreateDBRequest {
            id: Uuid::new_v4(),
            filename: "test.jsonl".to_string(),
            size_bytes: 1024,
            uploaded_by: user_id,
            status: FileStatus::Processed,
            error_message: None,
            expires_at: None,
        };

        let file = repo.create(&create_request).await.unwrap();

        assert_eq!(file.filename, "test.jsonl");
        assert_eq!(file.size_bytes, 1024);
        assert_eq!(file.status, FileStatus::Processed);
        assert_eq!(file.uploaded_by, user_id);
        assert!(file.error_message.is_none());
    }

    #[sqlx::test]
    async fn test_get_file_by_id(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Files::new(&mut conn);

        let create_request = FileCreateDBRequest {
            id: Uuid::new_v4(),
            filename: "test.jsonl".to_string(),
            size_bytes: 1024,
            uploaded_by: user_id,
            status: FileStatus::Processed,
            error_message: None,
            expires_at: None,
        };

        let created = repo.create(&create_request).await.unwrap();
        let retrieved = repo.get_by_id(created.id).await.unwrap();

        assert!(retrieved.is_some());
        let file = retrieved.unwrap();
        assert_eq!(file.id, created.id);
        assert_eq!(file.filename, "test.jsonl");
    }

    #[sqlx::test]
    async fn test_soft_delete_file(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Files::new(&mut conn);

        let create_request = FileCreateDBRequest {
            id: Uuid::new_v4(),
            filename: "to_delete.jsonl".to_string(),
            size_bytes: 1024,
            uploaded_by: user_id,
            status: FileStatus::Processed,
            error_message: None,
            expires_at: None,
        };

        let file = repo.create(&create_request).await.unwrap();
        assert_eq!(file.status, FileStatus::Processed);

        let deleted = repo.soft_delete(file.id).await.unwrap();
        assert!(deleted);

        let retrieved = repo.get_by_id(file.id).await.unwrap().unwrap();
        assert_eq!(retrieved.status, FileStatus::Deleted);
        assert!(retrieved.deleted_at.is_some());
    }

    #[sqlx::test]
    async fn test_mark_error(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Files::new(&mut conn);

        let create_request = FileCreateDBRequest {
            id: Uuid::new_v4(),
            filename: "error.jsonl".to_string(),
            size_bytes: 1024,
            uploaded_by: user_id,
            status: FileStatus::Processed,
            error_message: None,
            expires_at: None,
        };

        let file = repo.create(&create_request).await.unwrap();

        let updated = repo.mark_error(file.id, "Processing failed".to_string()).await.unwrap();
        assert!(updated);

        let retrieved = repo.get_by_id(file.id).await.unwrap().unwrap();
        assert_eq!(retrieved.status, FileStatus::Error);
        assert_eq!(retrieved.error_message, Some("Processing failed".to_string()));
    }

    #[sqlx::test]
    async fn test_list_files_with_filter(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Files::new(&mut conn);

        for i in 0..5 {
            let create_request = FileCreateDBRequest {
                id: Uuid::new_v4(),
                filename: format!("test_{}.jsonl", i),
                size_bytes: 1024 * (i as i64 + 1),
                uploaded_by: user_id,
                status: FileStatus::Processed,
                error_message: None,
                expires_at: None,
            };
            repo.create(&create_request).await.unwrap();
        }

        let filter = FileFilter::new(0, 10).with_uploaded_by(user_id).with_status(FileStatus::Processed);
        let files = repo.list(&filter).await.unwrap();
        assert_eq!(files.len(), 5);

        for i in 1..files.len() {
            assert!(files[i - 1].created_at >= files[i].created_at);
        }
    }

    #[sqlx::test]
    async fn test_list_files_with_pagination(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Files::new(&mut conn);

        for i in 0..5 {
            let create_request = FileCreateDBRequest {
                id: Uuid::new_v4(),
                filename: format!("test_{}.jsonl", i),
                size_bytes: 1024,
                uploaded_by: user_id,
                status: FileStatus::Processed,
                error_message: None,
                expires_at: None,
            };
            repo.create(&create_request).await.unwrap();
        }

        let filter = FileFilter::new(0, 2).with_uploaded_by(user_id);
        let first_page = repo.list(&filter).await.unwrap();
        assert_eq!(first_page.len(), 2);

        let filter = FileFilter::new(2, 2).with_uploaded_by(user_id);
        let second_page = repo.list(&filter).await.unwrap();
        assert_eq!(second_page.len(), 2);

        let first_ids: Vec<_> = first_page.iter().map(|f| f.id).collect();
        let second_ids: Vec<_> = second_page.iter().map(|f| f.id).collect();
        assert!(first_ids.iter().all(|id| !second_ids.contains(id)));
    }

    #[sqlx::test]
    async fn test_mark_expired_files(pool: PgPool) {
        let user_id = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let mut repo = Files::new(&mut conn);

        let expired_request = FileCreateDBRequest {
            id: Uuid::new_v4(),
            filename: "expired.jsonl".to_string(),
            size_bytes: 1024,
            uploaded_by: user_id,
            status: FileStatus::Processed,
            error_message: None,
            expires_at: Some(Utc::now() - chrono::Duration::days(1)),
        };
        repo.create(&expired_request).await.unwrap();

        let active_request = FileCreateDBRequest {
            id: Uuid::new_v4(),
            filename: "active.jsonl".to_string(),
            size_bytes: 1024,
            uploaded_by: user_id,
            status: FileStatus::Processed,
            error_message: None,
            expires_at: Some(Utc::now() + chrono::Duration::days(1)),
        };
        repo.create(&active_request).await.unwrap();

        let expired_ids = repo.mark_expired().await.unwrap();
        assert_eq!(expired_ids.len(), 1);

        let filter = FileFilter::new(0, 10).with_uploaded_by(user_id);
        let files = repo.list(&filter).await.unwrap();

        let expired_count = files.iter().filter(|f| f.status == FileStatus::Expired).count();
        let active_count = files.iter().filter(|f| f.status == FileStatus::Processed).count();

        assert_eq!(expired_count, 1);
        assert_eq!(active_count, 1);
    }
}
