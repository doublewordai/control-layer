use std::time::Duration;

use aws_config::BehaviorVersion;
use aws_config::meta::region::RegionProviderChain;
use aws_credential_types::{Credentials, provider::SharedCredentialsProvider};
use aws_sdk_s3::Client;
use aws_sdk_s3::config::{Region, timeout::TimeoutConfig};
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::ByteStream;
use uuid::Uuid;

use crate::config::{ObjectStoreConfig, ObjectStoreProvider};
use crate::errors::{Error, Result};

#[derive(Clone)]
pub struct BlobStorageClient {
    client: Client,
    bucket: String,
    prefix: String,
    provider: ObjectStoreProvider,
}

impl BlobStorageClient {
    pub async fn new(config: &ObjectStoreConfig) -> Result<Self> {
        let timeout_config = TimeoutConfig::builder()
            .connect_timeout(Duration::from_millis(config.connect_timeout_ms))
            .operation_timeout(Duration::from_millis(config.request_timeout_ms))
            .build();

        let mut sdk_config = aws_config::defaults(BehaviorVersion::latest())
            .region(RegionProviderChain::first_try(Region::new(config.region.clone())))
            .timeout_config(timeout_config);

        if let Some(endpoint) = normalized_endpoint(config) {
            sdk_config = sdk_config.endpoint_url(endpoint.to_owned());
        }

        if let Some(creds) = static_credentials(config) {
            sdk_config = sdk_config.credentials_provider(SharedCredentialsProvider::new(creds));
        }

        let sdk_config = sdk_config.load().await;

        let s3_config = aws_sdk_s3::config::Builder::from(&sdk_config)
            .force_path_style(effective_path_style(config))
            .build();

        Ok(Self {
            client: Client::from_conf(s3_config),
            bucket: config.bucket.clone(),
            prefix: config.prefix.clone(),
            provider: config.provider,
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
            .map_err(|e| blob_storage_error("put object to blob storage", self, key, e))?;
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
            .map_err(|e| blob_storage_error("get object from blob storage", self, key, e))?;

        let bytes = obj.body.collect().await.map_err(|e| Error::Internal {
            operation: format!("read blob object body: {e}"),
        })?;

        Ok(bytes.into_bytes().to_vec())
    }

    async fn get_object(&self, key: &str) -> Result<aws_sdk_s3::operation::get_object::GetObjectOutput> {
        self.client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| blob_storage_error("get object from blob storage", self, key, e))
    }

    pub async fn get_file_bytes_if_exists(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let obj = match self.client.get_object().bucket(&self.bucket).key(key).send().await {
            Ok(obj) => obj,
            Err(SdkError::ServiceError(err)) if err.err().is_no_such_key() => return Ok(None),
            Err(e) => return Err(blob_storage_error("get object from blob storage", self, key, e)),
        };

        let bytes = obj.body.collect().await.map_err(|e| Error::Internal {
            operation: format!("read blob object body: {e}"),
        })?;

        Ok(Some(bytes.into_bytes().to_vec()))
    }

    pub async fn put_bytes(&self, key: &str, bytes: Vec<u8>, content_type: &str) -> Result<()> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_type(content_type)
            .body(ByteStream::from(bytes))
            .send()
            .await
            .map_err(|e| blob_storage_error("put object to blob storage", self, key, e))?;
        Ok(())
    }

    pub async fn delete_object(&self, key: &str) -> Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| blob_storage_error("delete object from blob storage", self, key, e))?;
        Ok(())
    }

    pub async fn delete_prefix(&self, prefix: &str) -> Result<()> {
        let mut continuation_token: Option<String> = None;

        loop {
            let response = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix)
                .set_continuation_token(continuation_token.clone())
                .send()
                .await
                .map_err(|e| Error::Internal {
                    operation: format!(
                        "list objects from blob storage (bucket={}, prefix={}): {}",
                        self.bucket,
                        prefix,
                        classify_sdk_error(&e)
                    ),
                })?;

            for object in response.contents() {
                if let Some(key) = object.key() {
                    self.delete_object(key).await?;
                }
            }

            if response.is_truncated().unwrap_or(false) {
                continuation_token = response.next_continuation_token().map(ToOwned::to_owned);
            } else {
                break;
            }
        }

        Ok(())
    }
}

fn static_credentials(config: &ObjectStoreConfig) -> Option<Credentials> {
    let access_key_id = config.access_key_id.as_deref().map(str::trim).filter(|s| !s.is_empty())?;
    let secret_access_key = config.secret_access_key.as_deref().map(str::trim).filter(|s| !s.is_empty())?;
    let session_token = config
        .session_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);

    Some(Credentials::new(
        access_key_id.to_owned(),
        secret_access_key.to_owned(),
        session_token,
        None,
        "dwctl-object-store",
    ))
}

fn normalized_endpoint(config: &ObjectStoreConfig) -> Option<&str> {
    config.endpoint.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

fn effective_path_style(config: &ObjectStoreConfig) -> bool {
    config.path_style.unwrap_or(match config.provider {
        ObjectStoreProvider::AwsS3 => false,
        ObjectStoreProvider::S3Compatible => true,
    })
}

fn provider_name(provider: ObjectStoreProvider) -> &'static str {
    match provider {
        ObjectStoreProvider::AwsS3 => "aws_s3",
        ObjectStoreProvider::S3Compatible => "s3_compatible",
    }
}

fn classify_sdk_error<E>(error: &SdkError<E>) -> String
where
    E: std::fmt::Debug,
{
    match error {
        SdkError::ConstructionFailure(e) => format!("construction failure: {e:?}"),
        SdkError::TimeoutError(e) => format!("timeout: {e:?}"),
        SdkError::DispatchFailure(e) => format!("dispatch failure: {e:?}"),
        SdkError::ResponseError(e) => format!("response error: {e:?}"),
        SdkError::ServiceError(e) => format!("service error: {:?}", e.err()),
        _ => error.to_string(),
    }
}

fn blob_storage_error<E>(operation: &str, client: &BlobStorageClient, key: &str, error: SdkError<E>) -> Error
where
    E: std::fmt::Debug,
{
    Error::Internal {
        operation: format!(
            "{operation} (bucket={}, key={}, provider={}): {}",
            client.bucket,
            key,
            provider_name(client.provider),
            classify_sdk_error(&error)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::ObjectStoreProvider;

    fn object_store_config() -> ObjectStoreConfig {
        ObjectStoreConfig {
            provider: ObjectStoreProvider::S3Compatible,
            endpoint: Some("http://localhost:9000".to_string()),
            bucket: "bucket".to_string(),
            region: "us-east-1".to_string(),
            access_key_id: None,
            secret_access_key: None,
            session_token: None,
            path_style: Some(true),
            prefix: "uploads/".to_string(),
            connect_timeout_ms: 1000,
            request_timeout_ms: 1000,
        }
    }

    #[test]
    fn static_credentials_none_without_static_keys() {
        assert!(static_credentials(&object_store_config()).is_none());
    }

    #[test]
    fn static_credentials_build_when_keys_present() {
        let mut config = object_store_config();
        config.access_key_id = Some("key".to_string());
        config.secret_access_key = Some("secret".to_string());
        config.session_token = Some("token".to_string());

        let creds = static_credentials(&config).expect("static credentials should be built");

        assert_eq!(creds.access_key_id(), "key");
        assert_eq!(creds.secret_access_key(), "secret");
        assert_eq!(creds.session_token(), Some("token"));
    }

    #[test]
    fn static_credentials_ignore_blank_values() {
        let mut config = object_store_config();
        config.access_key_id = Some("   ".to_string());
        config.secret_access_key = Some("secret".to_string());

        assert!(static_credentials(&config).is_none());
    }

    #[test]
    fn effective_path_style_defaults_to_false_for_aws_s3() {
        let config = ObjectStoreConfig {
            provider: ObjectStoreProvider::AwsS3,
            endpoint: None,
            path_style: None,
            ..object_store_config()
        };

        assert!(!effective_path_style(&config));
    }

    #[test]
    fn effective_path_style_defaults_to_true_for_s3_compatible() {
        let config = ObjectStoreConfig {
            provider: ObjectStoreProvider::S3Compatible,
            endpoint: Some("http://localhost:9000".to_string()),
            path_style: None,
            ..object_store_config()
        };

        assert!(effective_path_style(&config));
    }

    #[test]
    fn effective_path_style_respects_explicit_override() {
        let config = ObjectStoreConfig {
            provider: ObjectStoreProvider::AwsS3,
            path_style: Some(true),
            ..object_store_config()
        };

        assert!(effective_path_style(&config));
    }

    #[test]
    fn normalized_endpoint_ignores_blank_values() {
        let config = ObjectStoreConfig {
            endpoint: Some("   ".to_string()),
            ..object_store_config()
        };

        assert!(normalized_endpoint(&config).is_none());
    }
}
