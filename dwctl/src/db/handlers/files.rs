use crate::db::{
    errors::{DbError, Result},
    handlers::{file_storage::FileStorage, repository::Repository},
    models::file_storage::FileStorageRequest,
    models::files::{FileCreateDBRequest, FileStatus, FileUpdateDBRequest, StorageBackend},
};
use crate::types::{FileId, UserId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use std::collections::HashMap;
use std::sync::Arc;
use utoipa::ToSchema;

/// File purpose for OpenAI-compatible files API
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum FilePurpose {
    Batch,
    BatchOutput,
}

impl std::fmt::Display for FilePurpose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FilePurpose::Batch => write!(f, "batch"),
            FilePurpose::BatchOutput => write!(f, "batch_output"),
        }
    }
}

impl std::str::FromStr for FilePurpose {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "batch" => Ok(FilePurpose::Batch),
            "batch_output" => Ok(FilePurpose::BatchOutput),
            _ => Err(format!("Unknown file purpose: {}", s)),
        }
    }
}

/// Filter for listing files
#[derive(Debug, Clone)]
pub struct FileFilter {
    pub uploaded_by: Option<UserId>,
    pub storage_backend: Option<StorageBackend>,
    pub purpose: Option<FilePurpose>,
    pub status: Option<FileStatus>,
    pub after: Option<FileId>,
    pub limit: i64,
    pub order_desc: bool,
}

impl Default for FileFilter {
    fn default() -> Self {
        Self {
            uploaded_by: None,
            storage_backend: None,
            purpose: None,
            status: None,
            after: None,
            limit: 10000,
            order_desc: true, // Default to descending (newest first)
        }
    }
}

impl FileFilter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn uploaded_by(mut self, user_id: UserId) -> Self {
        self.uploaded_by = Some(user_id);
        self
    }

    pub fn purpose(mut self, purpose: FilePurpose) -> Self {
        self.purpose = Some(purpose);
        self
    }

    pub fn status(mut self, status: FileStatus) -> Self {
        self.status = Some(status);
        self
    }

    pub fn after(mut self, file_id: FileId) -> Self {
        self.after = Some(file_id);
        self
    }

    pub fn limit(mut self, limit: i64) -> Self {
        self.limit = limit;
        self
    }

    pub fn order_desc(mut self, desc: bool) -> Self {
        self.order_desc = desc;
        self
    }
}

/// File domain object
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct File {
    pub id: FileId,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: i64,
    pub storage_backend: String, // Will be parsed to StorageBackend enum when needed
    pub storage_key: String,
    pub status: FileStatus,
    pub expires_at: Option<DateTime<Utc>>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub uploaded_by: UserId,
    pub purpose: String, // Will be parsed to FilePurpose enum when needed
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl File {
    /// Check if file content is available
    pub fn is_content_available(&self) -> bool {
        matches!(self.status, FileStatus::Active)
    }
}

pub struct Files<'c> {
    db: &'c mut PgConnection,
    storage: Arc<dyn FileStorage>,
}

impl<'c> Files<'c> {
    pub fn new(db: &'c mut PgConnection, storage: Arc<dyn FileStorage>) -> Self {
        Self { db, storage }
    }

    /// Create a file record with content - returns File object
    pub async fn create_with_content(&mut self, request: &FileCreateDBRequest, content: Vec<u8>) -> Result<File> {
        // Step 1: Store file content first (fail fast if storage fails)
        // Storage backend generates its own key
        let storage_request = FileStorageRequest {
            content,
            content_type: request.content_type.clone(),
        };

        let storage_response = self.storage.store(storage_request).await?;

        // Step 2: Create database record with storage key - Postgres generates file.id
        let mut enriched_request = request.clone();
        enriched_request.storage_key = storage_response.storage_key;

        // Try to create the database record
        match self.create(&enriched_request).await {
            Ok(file) => Ok(file),
            Err(e) => {
                // If metadata storage fails, clean up the file content
                let _ = self.storage.delete(&enriched_request.storage_key).await;
                Err(e)
            }
        }
    }

    /// Retrieve file content (only if status is active)
    pub async fn get_content(&self, file: &File) -> Result<Vec<u8>> {
        if !file.is_content_available() {
            return Err(DbError::InvalidData {
                message: format!("File content is not available (status: {:?})", file.status),
            });
        }
        self.storage.retrieve(&file.storage_key).await
    }

    /// Soft-delete file - marks as deleted and removes content from storage
    /// Metadata is retained for audit purposes
    pub async fn soft_delete(&mut self, id: FileId) -> Result<bool> {
        // Get file metadata first
        let file = match self.get_by_id(id).await? {
            Some(f) => f,
            None => return Ok(false),
        };

        // Only delete content if file is still active
        if file.status == FileStatus::Active {
            // Delete from storage first (best effort - continue even if this fails)
            let _ = self.storage.delete(&file.storage_key).await;
        }

        // Update database status to deleted
        let result = sqlx::query(
            r#"
            UPDATE files
            SET status = 'deleted', deleted_at = NOW(), updated_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(id)
        .execute(&mut *self.db)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Mark files as expired (called by cron job)
    /// This updates the status but doesn't delete content yet
    #[cfg(test)]
    pub async fn mark_expired(&mut self) -> Result<Vec<FileId>> {
        let expired_ids = sqlx::query_scalar::<_, FileId>(
            r#"
            UPDATE files
            SET status = 'expired', updated_at = NOW()
            WHERE status = 'active'
              AND expires_at IS NOT NULL
              AND expires_at <= NOW()
            RETURNING id
            "#,
        )
        .fetch_all(&mut *self.db)
        .await?;

        Ok(expired_ids)
    }

    /// Delete storage for expired/deleted files (called by cleanup job)
    #[cfg(test)]
    pub async fn cleanup_storage(&mut self, file_ids: Vec<FileId>) -> Result<usize> {
        let files = self.get_bulk(file_ids).await?;

        let mut cleaned = 0;
        for (_, file) in files {
            // Only clean up if status indicates content should be gone
            if matches!(file.status, FileStatus::Deleted | FileStatus::Expired) {
                if let Ok(()) = self.storage.delete(&file.storage_key).await {
                    cleaned += 1;
                }
            }
        }

        Ok(cleaned)
    }

    /// List files that need storage cleanup
    #[cfg(test)]
    pub async fn list_pending_cleanup(&mut self, limit: i64) -> Result<Vec<File>> {
        let files = sqlx::query_as::<_, File>(
            r#"
            SELECT * FROM files
            WHERE status IN ('deleted', 'expired')
            ORDER BY updated_at ASC
            LIMIT $1
            "#,
        )
        .bind(limit)
        .fetch_all(&mut *self.db)
        .await?;

        Ok(files)
    }
}

#[async_trait::async_trait]
impl<'c> Repository for Files<'c> {
    type CreateRequest = FileCreateDBRequest;
    type UpdateRequest = FileUpdateDBRequest;
    type Response = File;
    type Id = FileId;
    type Filter = FileFilter;

    async fn create(&mut self, request: &Self::CreateRequest) -> Result<Self::Response> {
        let storage_backend_str = match request.storage_backend {
            StorageBackend::Postgres => "postgres",
            StorageBackend::Local => "local",
        };

        let purpose_str = request.purpose.to_string();

        let file = sqlx::query_as::<_, File>(
            r#"
            INSERT INTO files (
                filename, content_type, size_bytes, storage_backend, storage_key,
                uploaded_by, purpose, expires_at, status
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'active')
            RETURNING *
            "#,
        )
        .bind(&request.filename)
        .bind(&request.content_type)
        .bind(request.size_bytes)
        .bind(storage_backend_str)
        .bind(&request.storage_key)
        .bind(request.uploaded_by)
        .bind(purpose_str)
        .bind(request.expires_at)
        .fetch_one(&mut *self.db)
        .await?;

        Ok(file)
    }

    async fn get_by_id(&mut self, id: Self::Id) -> Result<Option<Self::Response>> {
        let file = sqlx::query_as::<_, File>("SELECT * FROM files WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *self.db)
            .await?;

        Ok(file)
    }

    async fn get_bulk(&mut self, ids: Vec<Self::Id>) -> Result<HashMap<Self::Id, Self::Response>> {
        let files = sqlx::query_as::<_, File>("SELECT * FROM files WHERE id = ANY($1)")
            .bind(&ids)
            .fetch_all(&mut *self.db)
            .await?;

        Ok(files.into_iter().map(|f| (f.id, f)).collect())
    }

    async fn list(&mut self, filter: &Self::Filter) -> Result<Vec<Self::Response>> {
        // If we have an 'after' cursor, we need to get that file's created_at for comparison
        let after_created_at = if let Some(after_id) = filter.after {
            let after_file = sqlx::query_scalar::<_, DateTime<Utc>>("SELECT created_at FROM files WHERE id = $1")
                .bind(after_id)
                .fetch_optional(&mut *self.db)
                .await?;

            after_file
        } else {
            None
        };

        let mut query = sqlx::QueryBuilder::new("SELECT * FROM files WHERE 1=1");

        if let Some(user_id) = filter.uploaded_by {
            query.push(" AND uploaded_by = ");
            query.push_bind(user_id);
        }

        if let Some(backend) = filter.storage_backend {
            let backend_str = match backend {
                StorageBackend::Postgres => "postgres",
                StorageBackend::Local => "local",
            };
            query.push(" AND storage_backend = ");
            query.push_bind(backend_str);
        }

        if let Some(purpose) = filter.purpose {
            query.push(" AND purpose = ");
            query.push_bind(purpose.to_string());
        }

        if let Some(status) = filter.status {
            query.push(" AND status = ");
            query.push_bind(status);
        }

        // Handle pagination cursor - compare by created_at timestamp
        if let Some(after_created_at) = after_created_at {
            if filter.order_desc {
                query.push(" AND created_at < ");
            } else {
                query.push(" AND created_at > ");
            }
            query.push_bind(after_created_at);
        }

        // Apply ordering
        query.push(" ORDER BY created_at ");
        if filter.order_desc {
            query.push("DESC");
        } else {
            query.push("ASC");
        }

        // Apply limit
        query.push(" LIMIT ");
        query.push_bind(filter.limit);

        let files = query.build_query_as::<File>().fetch_all(&mut *self.db).await?;

        Ok(files)
    }

    async fn delete(&mut self, id: Self::Id) -> Result<bool> {
        // Hard delete from database (use soft_delete for normal deletions)
        let result = sqlx::query("DELETE FROM files WHERE id = $1")
            .bind(id)
            .execute(&mut *self.db)
            .await?;

        Ok(result.rows_affected() > 0)
    }

    async fn update(&mut self, id: Self::Id, request: &Self::UpdateRequest) -> Result<Self::Response> {
        let file = sqlx::query_as::<_, File>(
            r#"
            UPDATE files
            SET
                filename = COALESCE($2, filename),
                updated_at = NOW()
            WHERE id = $1
            RETURNING *
            "#,
        )
        .bind(id)
        .bind(&request.filename)
        .fetch_one(&mut *self.db)
        .await?;

        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api::models::users::{Role, UserCreate, UserResponse},
        db::handlers::{file_storage::PostgresFileStorage, Users},
        db::models::users::UserCreateDBRequest,
    };
    use sqlx::PgPool;
    use std::sync::Arc;

    async fn create_test_user(pool: &PgPool) -> UserResponse {
        let mut tx = pool.begin().await.unwrap();
        let mut user_repo = Users::new(&mut tx);

        let user_create = UserCreateDBRequest::from(UserCreate {
            username: "fileuser".to_string(),
            email: "file@example.com".to_string(),
            display_name: None,
            avatar_url: None,
            roles: vec![Role::StandardUser],
        });

        let user = user_repo.create(&user_create).await.unwrap();
        tx.commit().await.unwrap();
        user.into()
    }

    #[sqlx::test]
    async fn test_create_file_with_content(pool: PgPool) {
        let user = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let storage = Arc::new(PostgresFileStorage::new(pool.clone()));
        let mut repo = Files::new(&mut conn, storage);

        let content = b"test file content";
        let create_request = FileCreateDBRequest {
            filename: "test.txt".to_string(),
            content_type: "text/plain".to_string(),
            size_bytes: content.len() as i64,
            storage_backend: StorageBackend::Postgres,
            uploaded_by: user.id,
            purpose: FilePurpose::Batch,
            expires_at: None,
            storage_key: String::new(),
        };

        let file = repo.create_with_content(&create_request, content.to_vec()).await.unwrap();

        assert_eq!(file.filename, "test.txt");
        assert_eq!(file.storage_backend, "postgres");
        assert_eq!(file.purpose, "batch");
        assert_eq!(file.status, FileStatus::Active);
        assert!(!file.storage_key.is_empty());

        // Verify we can parse it as an OID
        let _oid: i32 = file.storage_key.parse().unwrap();
    }

    #[sqlx::test]
    async fn test_get_file_content(pool: PgPool) {
        let user = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let storage = Arc::new(PostgresFileStorage::new(pool.clone()));

        let content = b"test file content for retrieval";
        let create_request = FileCreateDBRequest {
            filename: "test.txt".to_string(),
            content_type: "text/plain".to_string(),
            size_bytes: content.len() as i64,
            storage_backend: StorageBackend::Postgres,
            uploaded_by: user.id,
            purpose: FilePurpose::Batch,
            expires_at: None,
            storage_key: String::new(),
        };

        let file = {
            let mut create_repo = Files::new(&mut conn, storage.clone());
            create_repo.create_with_content(&create_request, content.to_vec()).await.unwrap()
        };

        let repo = Files::new(&mut conn, storage.clone());
        let retrieved = repo.get_content(&file).await.unwrap();
        assert_eq!(retrieved, content);
    }

    #[sqlx::test]
    async fn test_soft_delete_file(pool: PgPool) {
        let user = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let storage = Arc::new(PostgresFileStorage::new(pool.clone()));
        let mut repo = Files::new(&mut conn, storage);

        let content = b"test file for soft deletion";
        let create_request = FileCreateDBRequest {
            filename: "soft_delete.txt".to_string(),
            content_type: "text/plain".to_string(),
            size_bytes: content.len() as i64,
            storage_backend: StorageBackend::Postgres,
            uploaded_by: user.id,
            purpose: FilePurpose::Batch,
            expires_at: None,
            storage_key: String::new(),
        };

        let file = repo.create_with_content(&create_request, content.to_vec()).await.unwrap();
        assert_eq!(file.status, FileStatus::Active);

        // Soft-delete the file
        let deleted = repo.soft_delete(file.id).await.unwrap();
        assert!(deleted);

        // Verify file metadata still exists but is marked as deleted
        let retrieved = repo.get_by_id(file.id).await.unwrap();
        assert!(retrieved.is_some());
        let deleted_file = retrieved.unwrap();
        assert_eq!(deleted_file.status, FileStatus::Deleted);
        assert!(deleted_file.deleted_at.is_some());
    }

    #[sqlx::test]
    async fn test_get_content_fails_for_deleted_file(pool: PgPool) {
        let user = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let storage = Arc::new(PostgresFileStorage::new(pool.clone()));
        let mut repo = Files::new(&mut conn, storage);

        let content = b"test file to delete";
        let create_request = FileCreateDBRequest {
            filename: "to_delete.txt".to_string(),
            content_type: "text/plain".to_string(),
            size_bytes: content.len() as i64,
            storage_backend: StorageBackend::Postgres,
            uploaded_by: user.id,
            purpose: FilePurpose::Batch,
            expires_at: None,
            storage_key: String::new(),
        };

        let file = repo.create_with_content(&create_request, content.to_vec()).await.unwrap();

        // Soft delete
        repo.soft_delete(file.id).await.unwrap();

        // Try to get content of deleted file
        let deleted_file = repo.get_by_id(file.id).await.unwrap().unwrap();
        let result = repo.get_content(&deleted_file).await;
        assert!(result.is_err());
    }

    #[sqlx::test]
    async fn test_mark_expired_files(pool: PgPool) {
        let user = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let storage = Arc::new(PostgresFileStorage::new(pool.clone()));
        let mut repo = Files::new(&mut conn, storage);

        // Create file that expired yesterday
        let expired_content = b"expired content";
        let expired_request = FileCreateDBRequest {
            filename: "expired.txt".to_string(),
            content_type: "text/plain".to_string(),
            size_bytes: expired_content.len() as i64,
            storage_backend: StorageBackend::Postgres,
            uploaded_by: user.id,
            purpose: FilePurpose::Batch,
            expires_at: Some(Utc::now() - chrono::Duration::days(1)),
            storage_key: String::new(),
        };
        repo.create_with_content(&expired_request, expired_content.to_vec()).await.unwrap();

        // Create file that expires tomorrow
        let active_content = b"active content";
        let active_request = FileCreateDBRequest {
            filename: "active.txt".to_string(),
            content_type: "text/plain".to_string(),
            size_bytes: active_content.len() as i64,
            storage_backend: StorageBackend::Postgres,
            uploaded_by: user.id,
            purpose: FilePurpose::Batch,
            expires_at: Some(Utc::now() + chrono::Duration::days(1)),
            storage_key: String::new(),
        };
        repo.create_with_content(&active_request, active_content.to_vec()).await.unwrap();

        // Mark expired files
        let expired_ids = repo.mark_expired().await.unwrap();
        assert_eq!(expired_ids.len(), 1);

        // Verify statuses
        let files = repo.list(&FileFilter::new().uploaded_by(user.id)).await.unwrap();
        assert_eq!(files.len(), 2);

        let expired_count = files.iter().filter(|f| f.status == FileStatus::Expired).count();
        let active_count = files.iter().filter(|f| f.status == FileStatus::Active).count();

        assert_eq!(expired_count, 1);
        assert_eq!(active_count, 1);
    }

    #[sqlx::test]
    async fn test_list_pending_cleanup(pool: PgPool) {
        let user = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let storage = Arc::new(PostgresFileStorage::new(pool.clone()));
        let mut repo = Files::new(&mut conn, storage);

        // Create and soft-delete a file
        let content1 = b"content to delete";
        let request1 = FileCreateDBRequest {
            filename: "deleted.txt".to_string(),
            content_type: "text/plain".to_string(),
            size_bytes: content1.len() as i64,
            storage_backend: StorageBackend::Postgres,
            uploaded_by: user.id,
            purpose: FilePurpose::Batch,
            expires_at: None,
            storage_key: String::new(),
        };
        let file1 = repo.create_with_content(&request1, content1.to_vec()).await.unwrap();
        repo.soft_delete(file1.id).await.unwrap();

        // Create an expired file
        let content2 = b"expired content";
        let request2 = FileCreateDBRequest {
            filename: "expired.txt".to_string(),
            content_type: "text/plain".to_string(),
            size_bytes: content2.len() as i64,
            storage_backend: StorageBackend::Postgres,
            uploaded_by: user.id,
            purpose: FilePurpose::Batch,
            expires_at: Some(Utc::now() - chrono::Duration::days(1)),
            storage_key: String::new(),
        };
        repo.create_with_content(&request2, content2.to_vec()).await.unwrap();
        repo.mark_expired().await.unwrap();

        // List files pending cleanup
        let pending = repo.list_pending_cleanup(10).await.unwrap();
        assert_eq!(pending.len(), 2);
        assert!(pending
            .iter()
            .all(|f| matches!(f.status, FileStatus::Deleted | FileStatus::Expired)));
    }

    #[sqlx::test]
    async fn test_cleanup_storage(pool: PgPool) {
        let user = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let storage = Arc::new(PostgresFileStorage::new(pool.clone()));
        let mut repo = Files::new(&mut conn, storage);

        // Create and mark as deleted (without actually deleting storage)
        // This simulates a scenario where files are marked deleted but storage cleanup failed
        let mut file_ids = Vec::new();
        for i in 0..3 {
            let content = format!("content {}", i);
            let request = FileCreateDBRequest {
                filename: format!("file_{}.txt", i),
                content_type: "text/plain".to_string(),
                size_bytes: content.len() as i64,
                storage_backend: StorageBackend::Postgres,
                uploaded_by: user.id,
                purpose: FilePurpose::Batch,
                expires_at: None,
                storage_key: String::new(),
            };
            let file = repo.create_with_content(&request, content.into_bytes()).await.unwrap();
            file_ids.push(file.id);

            // Mark as deleted in DB without removing storage (simulating a failed cleanup)
            sqlx::query(
                r#"
                UPDATE files
                SET status = 'deleted', deleted_at = NOW(), updated_at = NOW()
                WHERE id = $1
                "#,
            )
            .bind(file.id)
            .execute(&mut *repo.db)
            .await
            .unwrap();
        }

        // Now cleanup storage - this should actually delete the storage
        let cleaned = repo.cleanup_storage(file_ids.clone()).await.unwrap();
        assert_eq!(cleaned, 3);

        // Verify metadata still exists but status is deleted
        for file_id in file_ids {
            let file = repo.get_by_id(file_id).await.unwrap();
            assert!(file.is_some());
            assert_eq!(file.unwrap().status, FileStatus::Deleted);
        }
    }

    #[sqlx::test]
    async fn test_list_files_with_filter(pool: PgPool) {
        let user = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let storage = Arc::new(PostgresFileStorage::new(pool.clone()));
        let mut repo = Files::new(&mut conn, storage);

        // Create files with different purposes
        for i in 0..3 {
            let content = format!("test content {}", i);
            let create_request = FileCreateDBRequest {
                filename: format!("test_{}.txt", i),
                content_type: "text/plain".to_string(),
                size_bytes: content.len() as i64,
                storage_backend: StorageBackend::Postgres,
                uploaded_by: user.id,
                purpose: if i % 2 == 0 { FilePurpose::Batch } else { FilePurpose::BatchOutput },
                expires_at: None,
                storage_key: String::new(),
            };
            repo.create_with_content(&create_request, content.into_bytes()).await.unwrap();
        }

        // List all files for user
        let filter = FileFilter::new().uploaded_by(user.id).status(FileStatus::Active).limit(100);
        let files = repo.list(&filter).await.unwrap();
        assert_eq!(files.len(), 3);

        // List only batch files
        let filter = FileFilter::new()
            .uploaded_by(user.id)
            .purpose(FilePurpose::Batch)
            .status(FileStatus::Active)
            .limit(100);
        let files = repo.list(&filter).await.unwrap();
        assert_eq!(files.len(), 2);

        // List only batch_output files
        let filter = FileFilter::new()
            .uploaded_by(user.id)
            .purpose(FilePurpose::BatchOutput)
            .status(FileStatus::Active)
            .limit(100);
        let files = repo.list(&filter).await.unwrap();
        assert_eq!(files.len(), 1);
    }

    #[sqlx::test]
    async fn test_list_files_with_pagination(pool: PgPool) {
        let user = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let storage = Arc::new(PostgresFileStorage::new(pool.clone()));
        let mut repo = Files::new(&mut conn, storage);

        // Create 5 files
        let mut file_ids = Vec::new();
        for i in 0..5 {
            let content = format!("test content {}", i);
            let create_request = FileCreateDBRequest {
                filename: format!("test_{}.txt", i),
                content_type: "text/plain".to_string(),
                size_bytes: content.len() as i64,
                storage_backend: StorageBackend::Postgres,
                uploaded_by: user.id,
                purpose: FilePurpose::Batch,
                expires_at: None,
                storage_key: String::new(),
            };
            let file = repo.create_with_content(&create_request, content.into_bytes()).await.unwrap();
            file_ids.push(file.id);
        }

        // Get first 2 files (newest first, DESC order)
        let filter = FileFilter::new()
            .uploaded_by(user.id)
            .status(FileStatus::Active)
            .limit(2)
            .order_desc(true);
        let first_page = repo.list(&filter).await.unwrap();
        assert_eq!(first_page.len(), 2);

        // Get next 2 files using cursor
        let after_id = first_page.last().unwrap().id;
        let filter = FileFilter::new()
            .uploaded_by(user.id)
            .status(FileStatus::Active)
            .after(after_id)
            .limit(2)
            .order_desc(true);
        let second_page = repo.list(&filter).await.unwrap();
        assert_eq!(second_page.len(), 2);

        // Verify no overlap
        let first_page_ids: Vec<_> = first_page.iter().map(|f| f.id).collect();
        let second_page_ids: Vec<_> = second_page.iter().map(|f| f.id).collect();
        assert!(first_page_ids.iter().all(|id| !second_page_ids.contains(id)));

        // Get remaining files
        let after_id = second_page.last().unwrap().id;
        let filter = FileFilter::new()
            .uploaded_by(user.id)
            .status(FileStatus::Active)
            .after(after_id)
            .limit(2)
            .order_desc(true);
        let third_page = repo.list(&filter).await.unwrap();
        assert_eq!(third_page.len(), 1);
    }

    #[sqlx::test]
    async fn test_list_files_ascending_order(pool: PgPool) {
        let user = create_test_user(&pool).await;
        let mut conn = pool.acquire().await.unwrap();
        let storage = Arc::new(PostgresFileStorage::new(pool.clone()));
        let mut repo = Files::new(&mut conn, storage);

        // Create 3 files
        for i in 0..3 {
            let content = format!("test content {}", i);
            let create_request = FileCreateDBRequest {
                filename: format!("test_{}.txt", i),
                content_type: "text/plain".to_string(),
                size_bytes: content.len() as i64,
                storage_backend: StorageBackend::Postgres,
                uploaded_by: user.id,
                purpose: FilePurpose::Batch,
                expires_at: None,
                storage_key: String::new(),
            };
            repo.create_with_content(&create_request, content.into_bytes()).await.unwrap();
        }

        // Get files in ascending order (oldest first)
        let filter = FileFilter::new()
            .uploaded_by(user.id)
            .status(FileStatus::Active)
            .limit(100)
            .order_desc(false);
        let files = repo.list(&filter).await.unwrap();
        assert_eq!(files.len(), 3);

        // Verify ordering - oldest first
        for i in 1..files.len() {
            assert!(files[i - 1].created_at <= files[i].created_at);
        }
    }
}
