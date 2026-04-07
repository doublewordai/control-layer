use crate::blob_storage::BlobStorageClient;
use crate::config::Config;
use crate::errors::{Error, Result};
use axum::{
    body::Body,
    http::{HeaderValue, StatusCode},
    response::Response,
};
use fusillade::{BatchId, FileContentItem, FileId, ReqwestHttpClient, Storage};
use futures::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx_pool_router::PoolProvider;
use uuid::Uuid;

const CACHE_NAMESPACE: &str = "batch-results-cache";

async fn cache_client(config: &Config) -> Result<Option<BlobStorageClient>> {
    let Some(store_config) = config.batches.files.object_store.as_ref() else {
        return Ok(None);
    };

    Ok(Some(BlobStorageClient::new(store_config).await?))
}

fn normalize_filter(value: Option<&str>) -> String {
    value.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("_").to_string()
}

fn object_store_prefix(config: &Config) -> &str {
    config
        .batches
        .files
        .object_store
        .as_ref()
        .map(|cfg| cfg.prefix.as_str())
        .unwrap_or("")
}

fn cache_key_hash(file_id: Uuid, search: Option<&str>, status: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(file_id.to_string().as_bytes());
    hasher.update(b"\n");
    hasher.update(normalize_filter(search).as_bytes());
    hasher.update(b"\n");
    hasher.update(normalize_filter(status).as_bytes());
    format!("{:x}", hasher.finalize())
}

fn cache_prefix(config: &Config, file_id: Uuid) -> String {
    format!("{}{CACHE_NAMESPACE}/{file_id}/", object_store_prefix(config))
}

fn cache_object_key(config: &Config, file_id: Uuid, search: Option<&str>, status: Option<&str>) -> String {
    format!(
        "{}{hash}.jsonl",
        cache_prefix(config, file_id),
        hash = cache_key_hash(file_id, search, status)
    )
}

fn serialize_json_line<T: Serialize>(value: &T, kind: &str) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| Error::Internal {
        operation: format!("serialize {kind} to JSONL: {e}"),
    })?;
    bytes.push(b'\n');
    Ok(bytes)
}

pub fn serialize_file_content_item(item: FileContentItem) -> Result<Vec<u8>> {
    match item {
        FileContentItem::Template(template) => {
            let request = crate::api::handlers::files::OpenAIBatchRequest::from_internal(&template).map_err(|e| Error::Internal {
                operation: format!("transform template to OpenAI request: {e:?}"),
            })?;
            serialize_json_line(&request, "file content")
        }
        FileContentItem::Output(output) => serialize_json_line(&output, "file content"),
        FileContentItem::Error(error) => serialize_json_line(&error, "file content"),
    }
}

async fn collect_stream_bytes<T, S, F>(mut stream: S, mut serialize: F) -> Result<Vec<u8>>
where
    S: futures::Stream<Item = fusillade::Result<T>> + Unpin,
    F: FnMut(T) -> Result<Vec<u8>>,
{
    let mut bytes = Vec::new();

    while let Some(item) = stream.next().await {
        let item = item.map_err(|e| Error::Internal {
            operation: format!("stream batch result data: {e}"),
        })?;
        bytes.extend(serialize(item)?);
    }

    Ok(bytes)
}

async fn read_or_build_cache_entry<F, Fut>(
    config: &Config,
    file_id: Uuid,
    search: Option<&str>,
    status: Option<&str>,
    build: F,
) -> Result<Vec<u8>>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<u8>>>,
{
    let cache_key = cache_object_key(config, file_id, search, status);
    let Some(client) = cache_client(config).await? else {
        return build().await;
    };

    if let Some(cached) = client.get_file_bytes_if_exists(&cache_key).await? {
        return Ok(cached);
    }

    let bytes = build().await?;
    client.put_bytes(&cache_key, bytes.clone(), "application/x-ndjson").await?;
    Ok(bytes)
}

pub async fn get_or_build_file_content_jsonl<P: PoolProvider>(
    config: &Config,
    request_manager: &fusillade::PostgresRequestManager<P, ReqwestHttpClient>,
    file_id: FileId,
    search: Option<String>,
) -> Result<Vec<u8>> {
    let search_key = search.clone();
    read_or_build_cache_entry(config, *file_id, search_key.as_deref(), None, || async move {
        let stream = request_manager.get_file_content_stream(file_id, 0, search);
        collect_stream_bytes(stream, serialize_file_content_item).await
    })
    .await
}

pub async fn get_or_build_batch_results_jsonl<P: PoolProvider>(
    config: &Config,
    request_manager: &fusillade::PostgresRequestManager<P, ReqwestHttpClient>,
    batch_id: BatchId,
    cache_file_id: FileId,
    search: Option<String>,
    status: Option<String>,
) -> Result<Vec<u8>> {
    let search_key = search.clone();
    let status_key = status.clone();
    read_or_build_cache_entry(
        config,
        *cache_file_id,
        search_key.as_deref(),
        status_key.as_deref(),
        || async move {
            let stream = request_manager.get_batch_results_stream(batch_id, 0, search, status);
            collect_stream_bytes(stream, |item| serialize_json_line(&item, "batch result")).await
        },
    )
    .await
}

pub async fn invalidate_cached_file_results(config: &Config, file_id: Uuid) -> Result<()> {
    let Some(client) = cache_client(config).await? else {
        return Ok(());
    };

    client.delete_prefix(&cache_prefix(config, file_id)).await
}

pub struct JsonlSlice {
    pub body: Vec<u8>,
    pub total_lines: usize,
    pub returned_lines: usize,
    pub has_more_pages: bool,
}

pub fn slice_jsonl_bytes(bytes: &[u8], offset: usize, limit: Option<usize>) -> JsonlSlice {
    let newline_positions: Vec<usize> = bytes
        .iter()
        .enumerate()
        .filter_map(|(idx, b)| (*b == b'\n').then_some(idx))
        .collect();

    let total_lines = newline_positions.len();

    if offset >= total_lines {
        return JsonlSlice {
            body: Vec::new(),
            total_lines,
            returned_lines: 0,
            has_more_pages: false,
        };
    }

    let end_line = limit.map(|l| offset.saturating_add(l)).unwrap_or(total_lines).min(total_lines);
    let start_byte = if offset == 0 { 0 } else { newline_positions[offset - 1] + 1 };
    let end_byte = newline_positions[end_line - 1] + 1;

    JsonlSlice {
        body: bytes[start_byte..end_byte].to_vec(),
        total_lines,
        returned_lines: end_line - offset,
        has_more_pages: end_line < total_lines,
    }
}

pub fn jsonl_response_from_slice(slice: JsonlSlice, incomplete: bool) -> Response {
    let mut response = Response::new(Body::from(slice.body));
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("application/x-ndjson"));
    response.headers_mut().insert(
        "X-Incomplete",
        HeaderValue::from_str(if incomplete { "true" } else { "false" }).unwrap(),
    );
    let last_line = slice.returned_lines.min(slice.total_lines);
    response
        .headers_mut()
        .insert("X-Last-Line", HeaderValue::from_str(&last_line.to_string()).unwrap());
    *response.status_mut() = StatusCode::OK;
    response
}

pub fn jsonl_response_from_slice_with_offset(slice: JsonlSlice, offset: usize, incomplete: bool) -> Response {
    let mut response = Response::new(Body::from(slice.body));
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("application/x-ndjson"));
    response.headers_mut().insert(
        "X-Incomplete",
        HeaderValue::from_str(if incomplete { "true" } else { "false" }).unwrap(),
    );
    let last_line = offset + slice.returned_lines;
    response
        .headers_mut()
        .insert("X-Last-Line", HeaderValue::from_str(&last_line.to_string()).unwrap());
    *response.status_mut() = StatusCode::OK;
    response
}

#[cfg(test)]
mod tests {
    use super::{JsonlSlice, cache_key_hash, jsonl_response_from_slice_with_offset, slice_jsonl_bytes};
    use uuid::Uuid;

    #[test]
    fn cache_key_normalizes_empty_filters() {
        let file_id = Uuid::nil();
        let a = cache_key_hash(file_id, None, None);
        let b = cache_key_hash(file_id, Some(""), Some(""));
        let c = cache_key_hash(file_id, Some("   "), Some("  "));
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    #[test]
    fn cache_key_changes_with_search_and_status() {
        let file_id = Uuid::nil();
        let base = cache_key_hash(file_id, Some("req"), Some("completed"));
        let different_search = cache_key_hash(file_id, Some("other"), Some("completed"));
        let different_status = cache_key_hash(file_id, Some("req"), Some("failed"));

        assert_ne!(base, different_search);
        assert_ne!(base, different_status);
    }

    #[test]
    fn slice_jsonl_bytes_returns_expected_page() {
        let payload = b"{\"id\":1}\n{\"id\":2}\n{\"id\":3}\n";
        let slice = slice_jsonl_bytes(payload, 1, Some(1));
        assert_eq!(slice.total_lines, 3);
        assert_eq!(slice.returned_lines, 1);
        assert!(slice.has_more_pages);
        assert_eq!(String::from_utf8(slice.body).unwrap(), "{\"id\":2}\n");
    }

    #[test]
    fn slice_jsonl_bytes_handles_unbounded_tail() {
        let payload = b"a\nb\nc\n";
        let slice = slice_jsonl_bytes(payload, 1, None);
        assert_eq!(slice.total_lines, 3);
        assert_eq!(slice.returned_lines, 2);
        assert!(!slice.has_more_pages);
        assert_eq!(String::from_utf8(slice.body).unwrap(), "b\nc\n");
    }

    #[test]
    fn slice_jsonl_bytes_handles_empty_and_single_line_payloads() {
        let empty = slice_jsonl_bytes(b"", 0, Some(10));
        assert_eq!(empty.total_lines, 0);
        assert_eq!(empty.returned_lines, 0);
        assert_eq!(empty.body, Vec::<u8>::new());
        assert!(!empty.has_more_pages);

        let single = slice_jsonl_bytes(b"{\"id\":1}\n", 0, Some(10));
        assert_eq!(single.total_lines, 1);
        assert_eq!(single.returned_lines, 1);
        assert_eq!(String::from_utf8(single.body).unwrap(), "{\"id\":1}\n");
        assert!(!single.has_more_pages);
    }

    #[test]
    fn slice_jsonl_bytes_returns_empty_page_past_end() {
        let slice = slice_jsonl_bytes(b"a\nb\n", 5, Some(2));
        assert_eq!(slice.total_lines, 2);
        assert_eq!(slice.returned_lines, 0);
        assert_eq!(slice.body, Vec::<u8>::new());
        assert!(!slice.has_more_pages);
    }

    #[test]
    fn response_with_offset_uses_offset_for_last_line() {
        let response = jsonl_response_from_slice_with_offset(
            JsonlSlice {
                body: Vec::new(),
                total_lines: 2,
                returned_lines: 0,
                has_more_pages: false,
            },
            5,
            false,
        );

        assert_eq!(response.headers().get("X-Last-Line").unwrap(), "5");
        assert_eq!(response.headers().get("X-Incomplete").unwrap(), "false");
    }
}
