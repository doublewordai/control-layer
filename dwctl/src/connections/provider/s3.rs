//! Amazon S3 source provider implementation.

use super::{ByteStream, ConnectionTestResult, ExternalFile, FileListPage, ListFilesOptions, ProviderError, SourceProvider};
use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_sdk_s3::Client;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

/// S3-specific connection configuration (stored encrypted).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct S3Config {
    pub bucket: String,
    /// Optional key prefix to scope the listing (e.g. "inputs/batch/").
    #[serde(default)]
    pub prefix: Option<String>,
    pub region: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// Optional custom endpoint URL (for S3-compatible services like MinIO).
    #[serde(default)]
    pub endpoint_url: Option<String>,
}

pub struct S3Provider {
    config: S3Config,
}

impl S3Provider {
    pub fn new(config: S3Config) -> Self {
        Self { config }
    }

    async fn build_client(&self) -> Result<Client, ProviderError> {
        let creds = Credentials::new(
            &self.config.access_key_id,
            &self.config.secret_access_key,
            None, // no session token — production uses long-lived IAM keys
            None, // expiry
            "dwctl-connections",
        );

        // Build S3 client config directly to avoid aws_config picking up
        // environment variables (AWS_REGION, AWS_PROFILE, etc.) that could
        // override the user's connection settings.
        let mut s3_config = aws_sdk_s3::config::Builder::new()
            .region(aws_sdk_s3::config::Region::new(self.config.region.clone()))
            .credentials_provider(creds)
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest());

        if let Some(endpoint) = &self.config.endpoint_url {
            s3_config = s3_config.endpoint_url(endpoint).force_path_style(true);
        }

        Ok(Client::from_conf(s3_config.build()))
    }

    fn prefix(&self) -> &str {
        self.config.prefix.as_deref().unwrap_or("")
    }
}

#[async_trait]
impl SourceProvider for S3Provider {
    fn provider_type(&self) -> &str {
        "s3"
    }

    async fn list_files(&self) -> Result<Vec<ExternalFile>, ProviderError> {
        let client = self.build_client().await?;
        let mut files = Vec::new();
        let mut continuation_token: Option<String> = None;

        loop {
            let mut req = client.list_objects_v2().bucket(&self.config.bucket);

            if !self.prefix().is_empty() {
                req = req.prefix(self.prefix());
            }
            if let Some(ref token) = continuation_token {
                req = req.continuation_token(token);
            }

            let resp = req.send().await.map_err(|e| {
                tracing::warn!(error = %format!("{e:#}"), bucket = %self.config.bucket, "S3 list_objects_v2 failed");
                ProviderError::Internal(format!("{e:#}"))
            })?;

            for obj in resp.contents() {
                let key: &str = match obj.key() {
                    Some(k) => k,
                    None => continue,
                };

                // Skip "directory" markers
                if key.ends_with('/') {
                    continue;
                }

                // Only include .jsonl files
                if !key.ends_with(".jsonl") {
                    continue;
                }

                // Make key relative to prefix for cleaner identifiers
                let relative_key = if !self.prefix().is_empty() {
                    key.strip_prefix(self.prefix()).unwrap_or(key)
                } else {
                    key
                };

                let display_name = relative_key.rsplit('/').next().unwrap_or(relative_key).to_string();

                files.push(ExternalFile {
                    key: relative_key.to_string(),
                    size_bytes: obj.size(),
                    last_modified: obj
                        .last_modified()
                        .and_then(|dt| chrono::DateTime::from_timestamp(dt.secs(), dt.subsec_nanos())),
                    display_name: Some(display_name),
                });
            }

            if resp.is_truncated() == Some(true) {
                continuation_token = resp.next_continuation_token().map(|s| s.to_string());
            } else {
                break;
            }
        }

        Ok(files)
    }

    async fn list_files_paged(&self, options: ListFilesOptions) -> Result<FileListPage, ProviderError> {
        let client = self.build_client().await?;
        let limit = options.limit.unwrap_or(100).min(1000);
        let search = options.search.as_deref().map(|s| s.to_lowercase());

        let mut files = Vec::new();
        let mut continuation_token = options.cursor;
        // We may need to fetch multiple S3 pages to fill our limit when search filters out results
        let mut s3_next_token: Option<String> = None;

        loop {
            // When searching, we need to over-fetch because S3 can't filter by content.
            // Without search, request exactly what we need.
            let s3_max_keys = if search.is_some() { 1000 } else { limit as i32 };
            let mut req = client.list_objects_v2().bucket(&self.config.bucket).max_keys(s3_max_keys);

            if !self.prefix().is_empty() {
                req = req.prefix(self.prefix());
            }
            if let Some(ref token) = continuation_token.take().or(s3_next_token.take()) {
                req = req.continuation_token(token);
            }

            let resp = req.send().await.map_err(|e| ProviderError::Internal(format!("{e:#}")))?;

            for obj in resp.contents() {
                let key: &str = match obj.key() {
                    Some(k) => k,
                    None => continue,
                };

                if key.ends_with('/') {
                    continue;
                }
                if !key.ends_with(".jsonl") {
                    continue;
                }

                let relative_key = if !self.prefix().is_empty() {
                    key.strip_prefix(self.prefix()).unwrap_or(key)
                } else {
                    key
                };

                // Apply search filter
                if let Some(ref q) = search
                    && !relative_key.to_lowercase().contains(q)
                {
                    continue;
                }

                let display_name = relative_key.rsplit('/').next().unwrap_or(relative_key).to_string();

                files.push(ExternalFile {
                    key: relative_key.to_string(),
                    size_bytes: obj.size(),
                    last_modified: obj
                        .last_modified()
                        .and_then(|dt| chrono::DateTime::from_timestamp(dt.secs(), dt.subsec_nanos())),
                    display_name: Some(display_name),
                });
            }

            let s3_has_more = resp.is_truncated() == Some(true);
            let s3_next = resp.next_continuation_token().map(|s| s.to_string());

            // Check if we have enough after processing the full S3 response
            if files.len() >= limit {
                files.truncate(limit);
                // Use the S3 continuation token as cursor (safe because we processed
                // the full S3 page before truncating — no results are skipped).
                return Ok(FileListPage {
                    files,
                    has_more: s3_has_more || s3_next.is_some(),
                    next_cursor: s3_next,
                });
            }

            if s3_has_more {
                s3_next_token = s3_next;
            } else {
                break;
            }
        }

        Ok(FileListPage {
            files,
            has_more: false,
            next_cursor: None,
        })
    }

    async fn stream_file(&self, file_key: &str) -> Result<ByteStream, ProviderError> {
        let client = self.build_client().await?;

        // Reconstruct full key from prefix + relative key
        let full_key = if !self.prefix().is_empty() {
            format!("{}{}", self.prefix(), file_key)
        } else {
            file_key.to_string()
        };

        let resp = client
            .get_object()
            .bucket(&self.config.bucket)
            .key(&full_key)
            .send()
            .await
            .map_err(|e| {
                let msg = e.to_string();
                if msg.contains("NoSuchKey") || msg.contains("not found") {
                    ProviderError::NotFound(format!("s3://{}/{}", self.config.bucket, full_key))
                } else if msg.contains("AccessDenied") || msg.contains("Forbidden") {
                    ProviderError::AccessDenied(msg)
                } else {
                    ProviderError::Internal(msg)
                }
            })?;

        // Convert AWS ByteStream → async read → chunked byte stream.
        // `into_async_read()` gives us a tokio AsyncRead we can wrap.
        use tokio::io::AsyncReadExt;
        let reader = resp.body.into_async_read();
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, ProviderError>>(16);
        tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(reader);
            loop {
                let mut buf = vec![0u8; 64 * 1024]; // 64KB chunks
                match reader.read(&mut buf).await {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        buf.truncate(n);
                        if tx.send(Ok(Bytes::from(buf))).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(ProviderError::Internal(e.to_string()))).await;
                        break;
                    }
                }
            }
        });
        let byte_stream = tokio_stream::wrappers::ReceiverStream::new(rx);

        Ok(Box::pin(byte_stream))
    }

    async fn test_connection(&self) -> Result<ConnectionTestResult, ProviderError> {
        let client = self.build_client().await?;

        // Try to list a single object to verify access
        let mut req = client.list_objects_v2().bucket(&self.config.bucket).max_keys(1);

        if !self.prefix().is_empty() {
            req = req.prefix(self.prefix());
        }

        match req.send().await {
            Ok(_) => Ok(ConnectionTestResult {
                ok: true,
                message: None,
                scope: Some(serde_json::json!({
                    "bucket": self.config.bucket,
                    "prefix": self.prefix(),
                    "region": self.config.region,
                })),
            }),
            Err(e) => {
                // Extract the full error chain for diagnostics
                let sdk_err = e.into_service_error();
                let msg = format!("{sdk_err:#}");
                let meta = sdk_err.meta();
                let code = meta.code().unwrap_or("unknown");
                let sdk_message = meta.message().unwrap_or("no message");
                tracing::warn!(
                    error_code = %code,
                    error_message = %sdk_message,
                    full_error = %msg,
                    bucket = %self.config.bucket,
                    "S3 connection test failed"
                );
                let display_msg = format!("{code}: {sdk_message}");
                Ok(ConnectionTestResult {
                    ok: false,
                    message: Some(display_msg),
                    scope: None,
                })
            }
        }
    }
}
