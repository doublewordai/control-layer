use std::time::Duration;

use aws_config::BehaviorVersion;
use aws_config::meta::region::RegionProviderChain;
use aws_credential_types::{Credentials, provider::SharedCredentialsProvider};
use aws_sdk_s3::Client;
use aws_sdk_s3::config::{Region, timeout::TimeoutConfig};
use aws_sdk_s3::primitives::ByteStream;
use uuid::Uuid;

use crate::config::{ObjectStoreProvider, ObjectStoreConfig};
use crate::errors::{Error, Result};

#[derive(Clone)]
pub struct BlobStorageClient {
    client: Client,
    bucket: String,
    prefix: String,
}

impl BlobStorageClient {
    pub async fn new(config: &ObjectStoreConfig) -> Result<Self> {
        match config.provider {
            ObjectStoreProvider::S3Compatible => {}
        }

        let creds = Credentials::new(
            config.access_key_id.clone(),
            config.secret_access_key.clone(),
            config.session_token.clone(),
            None,
            "dwctl-object-store",
        );

        let timeout_config = TimeoutConfig::builder()
            .connect_timeout(Duration::from_millis(config.connect_timeout_ms))
            .operation_timeout(Duration::from_millis(config.request_timeout_ms))
            .build();

        let sdk_config = aws_config::defaults(BehaviorVersion::latest())
            .region(RegionProviderChain::first_try(Region::new(config.region.clone())))
            .credentials_provider(SharedCredentialsProvider::new(creds))
            .endpoint_url(config.endpoint.clone())
            .timeout_config(timeout_config)
            .load()
            .await;

        let s3_config = aws_sdk_s3::config::Builder::from(&sdk_config)
            .force_path_style(config.path_style)
            .build();

        Ok(Self {
            client: Client::from_conf(s3_config),
            bucket: config.bucket.clone(),
            prefix: config.prefix.clone(),
        })
    }

    pub fn object_key_for_file(&self, file_id: Uuid) -> String {
        format!("{}{file_id}.jsonl", self.prefix)
    }

    pub async fn put_file_from_path(&self, key: &str, path: &str, content_type: &str) -> Result<()> {
        let body = ByteStream::from_path(std::path::Path::new(path))
            .await
            .map_err(|e| Error::Internal {
                operation: format!("open upload file for object storage: {e}"),
            })?;

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_type(content_type)
            .body(body)
            .send()
            .await
            .map_err(|e| Error::Internal {
                operation: format!("put object to blob storage: {e}"),
            })?;
        Ok(())
    }

    pub async fn get_file_bytes(&self, key: &str) -> Result<Vec<u8>> {
        let obj = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| Error::Internal {
                operation: format!("get object from blob storage: {e}"),
            })?;

        let bytes = obj.body.collect().await.map_err(|e| Error::Internal {
            operation: format!("read blob object body: {e}"),
        })?;

        Ok(bytes.into_bytes().to_vec())
    }
}
