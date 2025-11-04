use crate::db::{
    errors::{DbError, Result},
    models::file_storage::{FileStorageRequest, FileStorageResponse},
};
use async_trait::async_trait;
use sqlx::PgPool;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Trait for file storage backends
#[async_trait]
pub trait FileStorage: Send + Sync {
    /// Store file content and return storage key
    async fn store(&self, request: FileStorageRequest) -> Result<FileStorageResponse>;

    /// Retrieve file content using storage key
    async fn retrieve(&self, storage_key: &str) -> Result<Vec<u8>>;

    /// Delete file content using storage key
    async fn delete(&self, storage_key: &str) -> Result<()>;

    /// Check if file exists using storage key
    async fn exists(&self, storage_key: &str) -> Result<bool>;
}

// ============================================================================
// Local Filesystem Storage Implementation
// ============================================================================

/// Local filesystem storage backend - stores files in a directory
/// Useful for development and testing
pub struct LocalFileStorage {
    base_path: PathBuf,
}

impl LocalFileStorage {
    pub fn new(base_path: PathBuf) -> Self {
        Self { base_path }
    }
}

#[async_trait]
impl FileStorage for LocalFileStorage {
    async fn store(&self, request: FileStorageRequest) -> Result<FileStorageResponse> {
        // Generate a unique path using UUID
        let file_uuid = uuid::Uuid::new_v4();
        let relative_path = format!("{}/{}.dat", file_uuid.to_string().chars().take(2).collect::<String>(), file_uuid);

        let full_path = self.base_path.join(&relative_path);

        // Ensure parent directory exists
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).await?;
        }

        // Write file
        let mut file = fs::File::create(&full_path).await?;
        file.write_all(&request.content).await?;
        file.sync_all().await?;

        Ok(FileStorageResponse {
            storage_key: relative_path,
        })
    }

    async fn retrieve(&self, storage_key: &str) -> Result<Vec<u8>> {
        let full_path = self.base_path.join(storage_key);

        if !full_path.exists() {
            return Err(DbError::NotFound);
        }

        let mut file = fs::File::open(&full_path).await?;
        let mut content = Vec::new();
        file.read_to_end(&mut content).await?;

        Ok(content)
    }

    async fn delete(&self, storage_key: &str) -> Result<()> {
        let full_path = self.base_path.join(storage_key);

        if full_path.exists() {
            fs::remove_file(&full_path).await?;
        }

        Ok(())
    }

    async fn exists(&self, storage_key: &str) -> Result<bool> {
        let full_path = self.base_path.join(storage_key);
        Ok(full_path.exists())
    }
}

// ============================================================================
// PostgreSQL Storage Implementation
// ============================================================================

/// PostgreSQL storage backend using large objects
/// Files are stored in a separate database/schema to isolate heavy file traffic
pub struct PostgresFileStorage {
    /// Dedicated connection pool for file storage operations
    pool: PgPool,
}

impl PostgresFileStorage {
    /// Create a new PostgreSQL file storage backend with its own connection pool
    ///
    /// # Arguments
    /// * `database_url` - Base database URL (e.g., postgres://user@host:5432/control_layer)
    ///
    /// # Behavior
    /// - Extracts host/port/credentials from the URL
    /// - Connects to `{original_db}_files` database (creates if doesn't exist)
    /// - Creates separate connection pool isolated from main app pool
    pub async fn new_with_pool(database_url: &str) -> Result<Self> {
        use sqlx::postgres::PgConnectOptions;

        // Parse the base database URL
        let base_options =
            PgConnectOptions::from_str(database_url).map_err(|e| DbError::Other(anyhow::anyhow!("Invalid database URL: {}", e)))?;

        // Create options for files database (same host/port/creds, different database name)
        let base_db_name = base_options.get_database().unwrap_or("control_layer");
        let files_db_name = format!("{}_files", base_db_name);
        let files_options = base_options.clone().database(&files_db_name);

        // Try to connect - if database doesn't exist, create it
        let pool = match sqlx::postgres::PgPoolOptions::new()
            .max_connections(5)
            .min_connections(1)
            .acquire_timeout(Duration::from_secs(30))
            .idle_timeout(Duration::from_secs(300))
            .connect_with(files_options.clone())
            .await
        {
            Ok(pool) => {
                tracing::debug!("Connected to existing files database: {}", files_db_name);
                pool
            }
            Err(e) if e.to_string().contains("does not exist") => {
                tracing::info!("Files database '{}' doesn't exist, creating it...", files_db_name);

                // Connect to 'postgres' database to create our files database
                let admin_options = base_options.clone().database("postgres");

                let admin_pool = sqlx::PgPool::connect_with(admin_options).await.map_err(|e| {
                    DbError::Other(anyhow::anyhow!(
                        "Failed to connect to postgres database to create files database: {}",
                        e
                    ))
                })?;

                // Create the files database
                let create_db_query = format!(r#"CREATE DATABASE "{}""#, files_db_name);
                sqlx::query(&create_db_query)
                    .execute(&admin_pool)
                    .await
                    .map_err(|e| DbError::Other(anyhow::anyhow!("Failed to create files database '{}': {}", files_db_name, e)))?;

                tracing::info!("Created files database: {}", files_db_name);
                admin_pool.close().await;

                // Now connect to the newly created database
                sqlx::postgres::PgPoolOptions::new()
                    .max_connections(5)
                    .min_connections(1)
                    .acquire_timeout(Duration::from_secs(30))
                    .idle_timeout(Duration::from_secs(300))
                    .connect_with(files_options)
                    .await
                    .map_err(|e| {
                        DbError::Other(anyhow::anyhow!(
                            "Failed to connect to new files database '{}': {}",
                            files_db_name,
                            e
                        ))
                    })?
            }
            Err(e) => {
                return Err(DbError::Other(anyhow::anyhow!(
                    "Failed to connect to files database '{}': {}",
                    files_db_name,
                    e
                )));
            }
        };

        tracing::info!(
            "PostgreSQL file storage initialized (database: {}, pool: {} connections)",
            files_db_name,
            pool.options().get_max_connections()
        );

        Ok(Self { pool })
    }

    #[cfg(test)]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl FileStorage for PostgresFileStorage {
    async fn store(&self, request: FileStorageRequest) -> Result<FileStorageResponse> {
        let mut tx = self.pool.begin().await?;

        // Create large object - Postgres generates the OID, cast to i32
        let oid: i32 = sqlx::query_scalar("SELECT lo_create(0)::int4").fetch_one(&mut *tx).await?;

        // Open for writing (mode 131072 = INV_WRITE)
        let fd: i32 = sqlx::query_scalar("SELECT lo_open($1, 131072)")
            .bind(oid)
            .fetch_one(&mut *tx)
            .await?;

        // Write content in chunks
        const CHUNK_SIZE: usize = 8192;
        for chunk in request.content.chunks(CHUNK_SIZE) {
            sqlx::query("SELECT lowrite($1, $2)").bind(fd).bind(chunk).execute(&mut *tx).await?;
        }

        // Close
        sqlx::query("SELECT lo_close($1)").bind(fd).execute(&mut *tx).await?;

        tx.commit().await?;

        Ok(FileStorageResponse {
            storage_key: oid.to_string(),
        })
    }

    async fn retrieve(&self, storage_key: &str) -> Result<Vec<u8>> {
        let oid: i32 = storage_key
            .parse()
            .map_err(|_| DbError::Other(anyhow::anyhow!("Invalid postgres OID: {}", storage_key)))?;

        let mut tx = self.pool.begin().await?;

        // Open for reading (mode 262144 = INV_READ)
        let fd: i32 = sqlx::query_scalar("SELECT lo_open($1, 262144)")
            .bind(oid)
            .fetch_one(&mut *tx)
            .await
            .map_err(|_| DbError::NotFound)?;

        // Read all content
        let mut content = Vec::new();
        loop {
            let chunk: Vec<u8> = sqlx::query_scalar("SELECT loread($1, 8192)").bind(fd).fetch_one(&mut *tx).await?;

            if chunk.is_empty() {
                break;
            }
            content.extend_from_slice(&chunk);
        }

        // Close
        sqlx::query("SELECT lo_close($1)").bind(fd).execute(&mut *tx).await?;

        tx.commit().await?;

        Ok(content)
    }

    async fn delete(&self, storage_key: &str) -> Result<()> {
        let oid: i32 = storage_key
            .parse()
            .map_err(|_| DbError::Other(anyhow::anyhow!("Invalid postgres OID: {}", storage_key)))?;

        sqlx::query("SELECT lo_unlink($1)").bind(oid).execute(&self.pool).await?;

        Ok(())
    }

    async fn exists(&self, storage_key: &str) -> Result<bool> {
        let oid: i32 = storage_key
            .parse()
            .map_err(|_| DbError::Other(anyhow::anyhow!("Invalid postgres OID: {}", storage_key)))?;

        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_largeobject_metadata WHERE oid = $1)")
            .bind(oid)
            .fetch_one(&self.pool)
            .await?;

        Ok(exists)
    }
}

// ============================================================================
// Noop Storage Implementation (for middleware that doesn't need file storage)
// ============================================================================

/// No-op file storage backend - used as a placeholder where file storage isn't needed
/// This allows us to construct AppState without actually creating a file storage backend
pub struct NoopFileStorage;

#[async_trait]
impl FileStorage for NoopFileStorage {
    async fn store(&self, _request: FileStorageRequest) -> Result<FileStorageResponse> {
        Err(DbError::Other(anyhow::anyhow!(
            "NoopFileStorage: file storage operations are not supported in this context"
        )))
    }

    async fn retrieve(&self, _storage_key: &str) -> Result<Vec<u8>> {
        Err(DbError::Other(anyhow::anyhow!(
            "NoopFileStorage: file storage operations are not supported in this context"
        )))
    }

    async fn delete(&self, _storage_key: &str) -> Result<()> {
        Err(DbError::Other(anyhow::anyhow!(
            "NoopFileStorage: file storage operations are not supported in this context"
        )))
    }

    async fn exists(&self, _storage_key: &str) -> Result<bool> {
        Err(DbError::Other(anyhow::anyhow!(
            "NoopFileStorage: file storage operations are not supported in this context"
        )))
    }
}

// ============================================================================
// Factory
// ============================================================================

/// Create a file storage backend based on configuration
pub async fn create_file_storage(config: &crate::config::FileStorageBackend, default_database_url: &str) -> Result<Arc<dyn FileStorage>> {
    match config {
        crate::config::FileStorageBackend::Postgres { database_url } => {
            // Use provided URL for completely separate instance, or derive from main DB
            let db_url = database_url.as_deref().unwrap_or(default_database_url);

            if database_url.is_some() {
                tracing::info!("Creating PostgreSQL file storage with separate database instance");
            } else {
                tracing::info!("Creating PostgreSQL file storage in same instance as main database");
            }

            let storage = PostgresFileStorage::new_with_pool(db_url).await?;
            Ok(Arc::new(storage))
        }
        crate::config::FileStorageBackend::Local { path } => {
            tracing::info!("Creating local file storage backend (path: {:?})", path);
            // Ensure directory exists
            if let Err(e) = tokio::fs::create_dir_all(path).await {
                return Err(DbError::Other(anyhow::anyhow!(
                    "Failed to create local storage directory {:?}: {}",
                    path,
                    e
                )));
            }
            Ok(Arc::new(LocalFileStorage::new(path.clone())))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test]
    async fn test_postgres_storage_lifecycle(pool: PgPool) {
        let storage = PostgresFileStorage::new(pool);

        let content = b"test content for storage";

        // Test store
        let request = FileStorageRequest {
            content: content.to_vec(),
            content_type: "application/jsonl".to_string(),
        };

        let response = storage.store(request).await.unwrap();
        assert!(!response.storage_key.is_empty());

        // Verify it's a valid OID
        let _oid: u32 = response.storage_key.parse().unwrap();

        // Test exists
        let exists = storage.exists(&response.storage_key).await.unwrap();
        assert!(exists);

        // Test retrieve
        let retrieved = storage.retrieve(&response.storage_key).await.unwrap();
        assert_eq!(retrieved, content);

        // Test delete
        storage.delete(&response.storage_key).await.unwrap();

        // Verify deletion
        let exists_after = storage.exists(&response.storage_key).await.unwrap();
        assert!(!exists_after);
    }

    #[sqlx::test]
    async fn test_postgres_storage_large_file(pool: PgPool) {
        let storage = PostgresFileStorage::new(pool);

        // Create a 1MB test file
        let content = vec![b'x'; 1024 * 1024];

        let request = FileStorageRequest {
            content: content.clone(),
            content_type: "application/jsonl".to_string(),
        };

        let response = storage.store(request).await.unwrap();
        let retrieved = storage.retrieve(&response.storage_key).await.unwrap();

        assert_eq!(retrieved.len(), content.len());
        assert_eq!(retrieved, content);

        storage.delete(&response.storage_key).await.unwrap();
    }

    #[tokio::test]
    async fn test_local_storage_lifecycle() {
        let temp_dir = tempfile::tempdir().unwrap();
        let storage = LocalFileStorage::new(temp_dir.path().to_path_buf());

        let content = b"test content for local storage";

        // Test store
        let request = FileStorageRequest {
            content: content.to_vec(),
            content_type: "application/jsonl".to_string(),
        };

        let response = storage.store(request).await.unwrap();
        assert!(!response.storage_key.is_empty());

        // Test exists
        let exists = storage.exists(&response.storage_key).await.unwrap();
        assert!(exists);

        // Test retrieve
        let retrieved = storage.retrieve(&response.storage_key).await.unwrap();
        assert_eq!(retrieved, content);

        // Test delete
        storage.delete(&response.storage_key).await.unwrap();

        // Verify deletion
        let exists_after = storage.exists(&response.storage_key).await.unwrap();
        assert!(!exists_after);
    }

    #[tokio::test]
    async fn test_local_storage_retrieve_nonexistent() {
        let temp_dir = tempfile::tempdir().unwrap();
        let storage = LocalFileStorage::new(temp_dir.path().to_path_buf());

        let storage_key = "nonexistent/file.dat";

        let result = storage.retrieve(storage_key).await;
        assert!(matches!(result, Err(DbError::NotFound)));
    }
}
