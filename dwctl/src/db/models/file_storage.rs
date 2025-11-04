/// Request to store file content
#[derive(Debug, Clone)]
pub struct FileStorageRequest {
    pub content: Vec<u8>,
    pub content_type: String,
}

/// Response from storing file content
#[derive(Debug, Clone)]
pub struct FileStorageResponse {
    /// Storage key to save in database - format depends on backend
    /// - Postgres: OID as string (e.g., "16384")
    /// - Local: relative path (e.g., "2024/11/abc-123.dat")
    pub storage_key: String,
}
