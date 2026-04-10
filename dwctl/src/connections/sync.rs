//! Underway job definitions for the sync/ingest/activate pipeline.
//!
//! Three job types form the pipeline:
//!
//! 1. **SyncConnectionJob** — discovers files (snapshot/select), deduplicates,
//!    creates sync_entries, enqueues IngestFileJob per new file.
//!
//! 2. **IngestFileJob** — streams a single file from the provider, validates
//!    JSONL, writes templates via fusillade's `create_file_stream`, then
//!    enqueues ActivateBatchJob.
//!
//! 3. **ActivateBatchJob** — checks SLA capacity (if configured), creates a
//!    batch record, enqueues the existing populate job, updates sync_entry.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Typed error for classifying activate-batch failures.
/// Fatal errors (e.g. validation) permanently fail the sync entry;
/// transient errors are retried by underway.
#[derive(Debug, thiserror::Error)]
enum ActivateError {
    #[error("{0}")]
    Fatal(String),
    #[error("{0}")]
    Retryable(String),
}

// ---------------------------------------------------------------------------
// Job input types (serialized to Postgres by underway)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
pub struct SyncConnectionInput {
    pub sync_id: Uuid,
    pub connection_id: Uuid,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IngestFileInput {
    pub sync_id: Uuid,
    pub sync_entry_id: Uuid,
    pub connection_id: Uuid,
    pub external_key: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ActivateBatchInput {
    pub sync_id: Uuid,
    pub sync_entry_id: Uuid,
    pub connection_id: Uuid,
    pub file_id: Uuid,
    pub template_count: i32,
    /// 0-based template indices that had tier-2 validation errors.
    /// Passed directly from ingest to avoid re-parsing capped JSON.
    pub validation_error_indices: Vec<i32>,
}

// ---------------------------------------------------------------------------
// Job builders
// ---------------------------------------------------------------------------

use crate::tasks::TaskState;
use sqlx_pool_router::PoolProvider;

/// Build the SyncConnection underway job.
pub async fn build_sync_connection_job<P: PoolProvider + Clone + Send + Sync + 'static>(
    pool: sqlx::PgPool,
    state: TaskState<P>,
) -> anyhow::Result<underway::Job<SyncConnectionInput, TaskState<P>>> {
    use underway::Job;
    use underway::job::To;
    use underway::task::Error as TaskError;

    Job::<SyncConnectionInput, _>::builder()
        .state(state)
        .step(|cx, input: SyncConnectionInput| async move {
            if let Err(e) = run_sync_connection(&cx.state, &input).await {
                tracing::error!(
                    sync_id = %input.sync_id,
                    error = %e,
                    "SyncConnectionJob failed"
                );
                // Mark the sync operation as failed
                if let Ok(mut conn) = cx.state.dwctl_pool.acquire().await {
                    let _ = crate::db::handlers::connections::SyncOperations::new(&mut conn)
                        .update_status(input.sync_id, "failed")
                        .await;
                }
                return Err(TaskError::Fatal(e.to_string()));
            }
            To::done()
        })
        .name("sync-connection")
        .pool(pool)
        .build()
        .await
        .map_err(Into::into)
}

/// Build the IngestFile underway job.
pub async fn build_ingest_file_job<P: PoolProvider + Clone + Send + Sync + 'static>(
    pool: sqlx::PgPool,
    state: TaskState<P>,
) -> anyhow::Result<underway::Job<IngestFileInput, TaskState<P>>> {
    use underway::Job;
    use underway::job::To;
    use underway::task::Error as TaskError;

    Job::<IngestFileInput, _>::builder()
        .state(state)
        .step(|cx, input: IngestFileInput| async move {
            match run_ingest_file(&cx.state, &input).await {
                Ok(()) => To::done(),
                Err(e) => {
                    tracing::error!(
                        sync_entry_id = %input.sync_entry_id,
                        external_key = %input.external_key,
                        error = %e,
                        "IngestFileJob failed"
                    );
                    // Mark entry as failed, then check if sync is complete
                    if let Ok(mut conn) = cx.state.dwctl_pool.acquire().await {
                        let _ = crate::db::handlers::connections::SyncEntries::new(&mut conn)
                            .update_status(input.sync_entry_id, "failed", Some(&e.to_string()))
                            .await;
                        let _ = crate::db::handlers::connections::SyncOperations::new(&mut conn)
                            .increment_counter(input.sync_id, "files_failed")
                            .await;
                        let _ = crate::db::handlers::connections::SyncOperations::new(&mut conn)
                            .try_complete(input.sync_id)
                            .await;
                    }
                    Err(TaskError::Fatal(e.to_string()))
                }
            }
        })
        .name("ingest-file")
        .pool(pool)
        .build()
        .await
        .map_err(Into::into)
}

/// Build the ActivateBatch underway job.
pub async fn build_activate_batch_job<P: PoolProvider + Clone + Send + Sync + 'static>(
    pool: sqlx::PgPool,
    state: TaskState<P>,
) -> anyhow::Result<underway::Job<ActivateBatchInput, TaskState<P>>> {
    use underway::Job;
    use underway::job::To;
    use underway::task::Error as TaskError;

    Job::<ActivateBatchInput, _>::builder()
        .state(state)
        .step(|cx, input: ActivateBatchInput| async move {
            match run_activate_batch(&cx.state, &input).await {
                Ok(()) => {
                    // Check if sync is now complete
                    if let Ok(mut conn) = cx.state.dwctl_pool.acquire().await {
                        let _ = crate::db::handlers::connections::SyncOperations::new(&mut conn)
                            .try_complete(input.sync_id)
                            .await;
                    }
                    To::done()
                }
                Err(e) => {
                    let is_retryable = e
                        .downcast_ref::<ActivateError>()
                        .is_some_and(|ae| matches!(ae, ActivateError::Retryable(_)));

                    tracing::error!(
                        sync_entry_id = %input.sync_entry_id,
                        retryable = is_retryable,
                        error = %e,
                        "ActivateBatchJob failed"
                    );

                    if is_retryable {
                        return Err(TaskError::Retryable(e.to_string()));
                    }

                    // Fatal — mark entry as failed and update sync counters
                    if let Ok(mut conn) = cx.state.dwctl_pool.acquire().await {
                        let _ = crate::db::handlers::connections::SyncEntries::new(&mut conn)
                            .update_status(input.sync_entry_id, "failed", Some(&e.to_string()))
                            .await;
                        let _ = crate::db::handlers::connections::SyncOperations::new(&mut conn)
                            .increment_counter(input.sync_id, "files_failed")
                            .await;
                        let _ = crate::db::handlers::connections::SyncOperations::new(&mut conn)
                            .try_complete(input.sync_id)
                            .await;
                    }
                    Err(TaskError::Fatal(e.to_string()))
                }
            }
        })
        .name("activate-batch")
        .pool(pool)
        .build()
        .await
        .map_err(Into::into)
}

// ---------------------------------------------------------------------------
// Job implementations
// ---------------------------------------------------------------------------

async fn run_sync_connection<P: PoolProvider + Clone + Send + Sync + 'static>(
    state: &TaskState<P>,
    input: &SyncConnectionInput,
) -> anyhow::Result<()> {
    use crate::connections::provider;
    use crate::db::handlers::connections::{Connections, SyncEntries, SyncOperations};

    let dwctl = &state.dwctl_pool;

    // 1. Load connection and decrypt config
    let (connection, config_json) = {
        let mut conn = dwctl.acquire().await?;
        let connection = Connections::new(&mut conn)
            .get_by_id(input.connection_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("connection not found: {}", input.connection_id))?;

        let encryption_key = state
            .encryption_key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("encryption key not configured"))?;
        let config_json = crate::encryption::decrypt_json(encryption_key, &connection.config_encrypted)?;
        (connection, config_json)
    };

    // 2. Update sync status to listing
    {
        let mut conn = dwctl.acquire().await?;
        SyncOperations::new(&mut conn).update_status(input.sync_id, "listing").await?;
    }

    // 3. Load sync operation to get strategy
    let sync_op = {
        let mut conn = dwctl.acquire().await?;
        SyncOperations::new(&mut conn)
            .get_by_id(input.sync_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("sync operation not found"))?
    };

    // 4. Discover files based on strategy
    let prov = provider::create_provider(&connection.provider, config_json)?;
    let files = match sync_op.strategy.as_str() {
        "snapshot" => prov.list_files().await.map_err(|e| anyhow::anyhow!("{e:#}"))?,
        "select" => {
            let keys: Vec<String> = sync_op
                .strategy_config
                .as_ref()
                .and_then(|c| c.get("file_keys"))
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();

            let key_set: std::collections::HashSet<String> = keys.into_iter().collect();

            // List from provider to get real metadata (size, last_modified),
            // then filter to only the selected keys.
            // TODO(perf): For large buckets, this full listing is O(bucket_size).
            // Consider using HeadObject per key or a batched metadata call instead.
            let all_files = prov.list_files().await.map_err(|e| anyhow::anyhow!("{e:#}"))?;
            all_files.into_iter().filter(|f| key_set.contains(&f.key)).collect()
        }
        other => anyhow::bail!("unsupported strategy: {other}"),
    };

    let files_found = files.len() as i32;

    // 5. Dedup against previously synced entries (skip when force=true)
    let force = sync_op
        .strategy_config
        .as_ref()
        .and_then(|c| c.get("force"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let (new_files, files_skipped) = if force {
        // Force mode: skip dedup, re-ingest everything
        let all: Vec<&provider::ExternalFile> = files.iter().collect();
        (all, 0i32)
    } else {
        let keys_and_dates: Vec<(String, Option<chrono::DateTime<chrono::Utc>>)> =
            files.iter().map(|f| (f.key.clone(), f.last_modified)).collect();

        let already_synced = {
            let mut conn = dwctl.acquire().await?;
            SyncEntries::new(&mut conn)
                .find_existing(input.connection_id, &keys_and_dates)
                .await?
        };

        let already_synced_set: std::collections::HashSet<(String, Option<chrono::DateTime<chrono::Utc>>)> =
            already_synced.into_iter().collect();

        let new: Vec<&provider::ExternalFile> = files
            .iter()
            .filter(|f| !already_synced_set.contains(&(f.key.clone(), f.last_modified)))
            .collect();

        let skipped = files_found - new.len() as i32;
        (new, skipped)
    };

    // 6. Create sync_entries for new files
    let entry_data: Vec<(String, Option<chrono::DateTime<chrono::Utc>>, Option<i64>)> =
        new_files.iter().map(|f| (f.key.clone(), f.last_modified, f.size_bytes)).collect();

    let entries = {
        let mut conn = dwctl.acquire().await?;
        SyncEntries::new(&mut conn)
            .bulk_create(input.sync_id, input.connection_id, &entry_data)
            .await?
    };

    // 7. Update sync operation counters
    {
        let mut conn = dwctl.acquire().await?;
        SyncOperations::new(&mut conn)
            .update_counters(input.sync_id, Some(files_found), Some(files_skipped), None, None, None)
            .await?;
        SyncOperations::new(&mut conn).update_status(input.sync_id, "ingesting").await?;
    }

    // 8. Mark skipped entries
    // (entries that were in already_synced_set but we didn't create — no entries to update since we filtered them)
    // The files_skipped counter above captures this.

    // 9. Enqueue IngestFileJob for each new entry
    for entry in &entries {
        state
            .get_ingest_file_job()?
            .enqueue(&IngestFileInput {
                sync_id: input.sync_id,
                sync_entry_id: entry.id,
                connection_id: input.connection_id,
                external_key: entry.external_key.clone(),
            })
            .await?;
    }

    if entries.is_empty() {
        // Nothing to ingest — mark sync as completed
        let mut conn = dwctl.acquire().await?;
        SyncOperations::new(&mut conn).update_status(input.sync_id, "completed").await?;
    }

    tracing::info!(
        sync_id = %input.sync_id,
        files_found,
        files_skipped,
        files_new = entries.len(),
        "Sync discovery complete"
    );

    Ok(())
}

pub(crate) async fn run_ingest_file<P: PoolProvider + Clone + Send + Sync + 'static>(
    state: &TaskState<P>,
    input: &IngestFileInput,
) -> anyhow::Result<()> {
    use crate::connections::provider;
    use crate::db::handlers::connections::{Connections, SyncEntries, SyncOperations};
    use fusillade::{FileStreamItem, Storage};

    let dwctl = &state.dwctl_pool;

    // 1. Mark entry as ingesting
    {
        let mut conn = dwctl.acquire().await?;
        let updated = SyncEntries::new(&mut conn)
            .update_status(input.sync_entry_id, "ingesting", None)
            .await?;
        if !updated {
            tracing::info!(sync_entry_id = %input.sync_entry_id, "Sync entry deleted, skipping ingestion");
            return Ok(());
        }
    }

    // 2. Load connection and build provider
    let (connection, config_json) = {
        let mut conn = dwctl.acquire().await?;
        let connection = Connections::new(&mut conn)
            .get_by_id(input.connection_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("connection not found"))?;
        let encryption_key = state
            .encryption_key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("encryption key not configured"))?;
        let config_json = crate::encryption::decrypt_json(encryption_key, &connection.config_encrypted)?;
        (connection, config_json)
    };

    let prov = provider::create_provider(&connection.provider, config_json)?;

    // 3. Load sync config for this operation
    let sync_op = {
        let mut conn = dwctl.acquire().await?;
        SyncOperations::new(&mut conn)
            .get_by_id(input.sync_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("sync operation not found"))?
    };

    let api_path = sync_op
        .sync_config
        .get("endpoint")
        .and_then(|v| v.as_str())
        .unwrap_or("/v1/chat/completions")
        .to_string();

    // Build the base URL for the AI proxy (same as normal file upload does)
    let ai_base_url = sync_op
        .sync_config
        .get("ai_base_url")
        .and_then(|v| v.as_str())
        .unwrap_or("http://127.0.0.1:3001/ai")
        .to_string();

    // 4. Resolve file ownership — connection.user_id owns the file,
    //    and we use the triggering user's hidden batch key for member attribution.
    //    Reuse the connection loaded in step 2 (no extra DB round trip).
    let (file_owner, file_api_key_id) = {
        use crate::db::handlers::api_keys::ApiKeys;
        use crate::db::models::api_keys::ApiKeyPurpose;

        let owner_id = connection.user_id;
        let triggered_by = sync_op.triggered_by;

        let mut conn = dwctl.acquire().await?;
        let (_secret, key_id) = ApiKeys::new(&mut conn)
            .get_or_create_hidden_key_with_id(owner_id, ApiKeyPurpose::Batch, triggered_by)
            .await
            .map_err(|e| anyhow::anyhow!("resolve file API key: {e}"))?;

        (owner_id, key_id)
    };

    // 5. Stream file from provider
    let byte_stream = prov
        .stream_file(&input.external_key)
        .await
        .map_err(|e| anyhow::anyhow!("stream file: {e}"))?;

    // 6. Convert byte stream → JSONL lines → FileStreamItem stream
    //    and feed into fusillade's create_file_stream
    let (tx, rx) = tokio::sync::mpsc::channel::<FileStreamItem>(64);

    // Spawn producer task: reads from S3, parses JSONL, sends templates
    let external_key = input.external_key.clone();
    let connection_id = input.connection_id;
    let producer = tokio::spawn(async move {
        use futures::StreamExt;

        // Send metadata first
        let display_name = external_key.rsplit('/').next().unwrap_or(&external_key).to_string();

        let metadata = fusillade::FileMetadata {
            filename: Some(display_name),
            description: Some(format!("Synced from external source: {external_key}")),
            purpose: Some("batch".to_string()),
            uploaded_by: Some(file_owner.to_string()),
            api_key_id: Some(file_api_key_id),
            source_connection_id: Some(connection_id),
            source_external_key: Some(external_key.clone()),
            ..Default::default()
        };

        if tx.send(FileStreamItem::Metadata(metadata)).await.is_err() {
            return (0i32, 0i32, Vec::new(), Vec::new());
        }

        let mut line_buf = String::new();
        let mut template_count: i32 = 0;
        let mut skipped_lines: i32 = 0;
        // (template_index, file_line, error) — template_index matches request_templates.line_number
        // Detailed errors are capped for storage/display; template indices are always
        // collected so the activate step can fail the correct requests.
        const MAX_VALIDATION_ERRORS: usize = 1000;
        let mut validation_errors: Vec<(i32, u64, String)> = Vec::new();
        let mut validation_error_indices: Vec<i32> = Vec::new();
        let mut line_number: u64 = 0;
        let mut stream = byte_stream;
        // Buffer for incomplete UTF-8 sequences split across chunk boundaries.
        let mut utf8_buf: Vec<u8> = Vec::new();

        while let Some(chunk_result) = stream.next().await {
            let chunk = match chunk_result {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(error = %e, "Error reading from provider stream");
                    let _ = tx.send(FileStreamItem::Abort).await;
                    return (template_count, skipped_lines, validation_errors, validation_error_indices);
                }
            };

            // Prepend any leftover bytes from the previous chunk
            utf8_buf.extend_from_slice(&chunk);

            // Decode as much valid UTF-8 as possible, keeping incomplete
            // trailing bytes for the next chunk.
            let drain_up_to = match std::str::from_utf8(&utf8_buf) {
                Ok(s) => {
                    line_buf.push_str(s);
                    utf8_buf.len()
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        // Safety: from_utf8 confirmed these bytes are valid
                        let s = unsafe { std::str::from_utf8_unchecked(&utf8_buf[..valid]) };
                        line_buf.push_str(s);
                    }
                    if let Some(error_len) = e.error_len() {
                        // Genuinely invalid bytes (not incomplete) — skip them
                        // to avoid infinite loop. Replace with U+FFFD.
                        line_buf.push(char::REPLACEMENT_CHARACTER);
                        valid + error_len
                    } else {
                        // Incomplete sequence at end — wait for more data
                        valid
                    }
                }
            };
            if drain_up_to > 0 {
                utf8_buf.drain(..drain_up_to);
            }

            // Process complete lines using a cursor to avoid O(n²) drain per line.
            // We scan for newlines by offset, then compact once after the loop.
            let mut cursor = 0;
            while let Some(rel_pos) = line_buf[cursor..].find('\n') {
                let newline_pos = cursor + rel_pos;
                let line = line_buf[cursor..newline_pos].trim();
                cursor = newline_pos + 1;

                if line.is_empty() {
                    continue;
                }
                line_number += 1;

                // Three-tier error handling:
                // Tier 1: non-JSON → skip entirely (garbled line)
                // Tier 2: JSON but invalid → still ingest as template, record error
                // Tier 3: valid → ingest normally
                match serde_json::from_str::<serde_json::Value>(line) {
                    Ok(parsed) => {
                        let custom_id = parsed.get("custom_id").and_then(|v| v.as_str()).map(|s| s.to_string());
                        let method = parsed.get("method").and_then(|v| v.as_str()).unwrap_or("POST").to_string();
                        // Always use the configured endpoint — ignore per-line url to prevent
                        // targeting unsupported/internal paths (consistent with batch-level routing).
                        let url = api_path.clone();
                        let body = parsed.get("body").map(|v| v.to_string()).unwrap_or_else(|| "{}".to_string());
                        let model = parsed
                            .get("body")
                            .and_then(|b| b.get("model"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();

                        // Collect validation errors (tier 2) — still ingest the template
                        let mut line_error: Option<String> = None;

                        if !matches!(method.as_str(), "POST" | "GET" | "PUT" | "PATCH" | "DELETE") {
                            line_error = Some(format!("invalid HTTP method: {method}"));
                        } else if model.is_empty() {
                            line_error = Some("missing model field in body".to_string());
                        } else {
                            const MAX_BODY_SIZE: usize = 10 * 1024 * 1024;
                            if body.len() > MAX_BODY_SIZE {
                                line_error = Some(format!("oversized body: {} bytes", body.len()));
                            }
                        }
                        if line_error.is_none()
                            && let Some(ref cid) = custom_id
                            && cid.chars().any(|c| c.is_control())
                        {
                            line_error = Some("control characters in custom_id".to_string());
                        }

                        if let Some(ref err) = line_error {
                            tracing::warn!(line_num = line_number, error = %err, "Validation error (tier 2), ingesting template with error");
                            validation_error_indices.push(template_count);
                            if validation_errors.len() < MAX_VALIDATION_ERRORS {
                                validation_errors.push((template_count, line_number, err.clone()));
                            }
                        }

                        // Strip `priority` from body if present and re-serialize
                        let body = if let Ok(mut body_val) = serde_json::from_str::<serde_json::Value>(&body) {
                            if body_val.as_object_mut().is_some_and(|o| o.remove("priority").is_some()) {
                                serde_json::to_string(&body_val).unwrap_or(body)
                            } else {
                                body
                            }
                        } else {
                            body
                        };

                        let template = fusillade::RequestTemplateInput {
                            custom_id,
                            endpoint: ai_base_url.clone(),
                            method,
                            path: url,
                            body,
                            model,
                            api_key: String::new(), // Set at batch activation via batch.api_key
                        };

                        if tx.send(FileStreamItem::Template(template)).await.is_err() {
                            return (template_count, skipped_lines, validation_errors, validation_error_indices);
                        }
                        template_count += 1;
                    }
                    Err(e) => {
                        // Tier 1: garbled line — not valid JSON at all
                        tracing::warn!(line_num = line_number, error = %e, "Skipping non-JSON line (tier 1)");
                        skipped_lines += 1;
                    }
                }
            }
            // Compact: remove all processed lines in one operation
            if cursor > 0 {
                line_buf.drain(..cursor);
            }
        }

        // Flush any remaining UTF-8 bytes
        if !utf8_buf.is_empty() {
            if let Ok(s) = std::str::from_utf8(&utf8_buf) {
                line_buf.push_str(s);
            } else {
                tracing::warn!("Discarding {} trailing bytes (invalid UTF-8)", utf8_buf.len());
            }
        }

        // Handle any remaining partial line (same three-tier handling as main loop)
        let remaining = line_buf.trim();
        if !remaining.is_empty() {
            line_number += 1;
            match serde_json::from_str::<serde_json::Value>(remaining) {
                Ok(parsed) => {
                    let custom_id = parsed.get("custom_id").and_then(|v| v.as_str()).map(|s| s.to_string());
                    let method = parsed.get("method").and_then(|v| v.as_str()).unwrap_or("POST").to_string();
                    let url = api_path.clone();
                    let body = parsed.get("body").map(|v| v.to_string()).unwrap_or_else(|| "{}".to_string());
                    let model = parsed
                        .get("body")
                        .and_then(|b| b.get("model"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();

                    let mut line_error: Option<String> = None;

                    if !matches!(method.as_str(), "POST" | "GET" | "PUT" | "PATCH" | "DELETE") {
                        line_error = Some(format!("invalid HTTP method: {method}"));
                    } else if model.is_empty() {
                        line_error = Some("missing model field in body".to_string());
                    } else {
                        const MAX_BODY_SIZE: usize = 10 * 1024 * 1024;
                        if body.len() > MAX_BODY_SIZE {
                            line_error = Some(format!("oversized body: {} bytes", body.len()));
                        }
                    }
                    if line_error.is_none()
                        && let Some(ref cid) = custom_id
                        && cid.chars().any(|c| c.is_control())
                    {
                        line_error = Some("control characters in custom_id".to_string());
                    }

                    if let Some(ref err) = line_error {
                        tracing::warn!(line_num = line_number, error = %err, "Validation error (tier 2), ingesting template with error");
                        validation_error_indices.push(template_count);
                        if validation_errors.len() < MAX_VALIDATION_ERRORS {
                            validation_errors.push((template_count, line_number, err.clone()));
                        }
                    }

                    // Strip `priority` from body if present and re-serialize
                    let body = if let Ok(mut body_val) = serde_json::from_str::<serde_json::Value>(&body) {
                        if body_val.as_object_mut().is_some_and(|o| o.remove("priority").is_some()) {
                            serde_json::to_string(&body_val).unwrap_or(body)
                        } else {
                            body
                        }
                    } else {
                        body
                    };

                    let template = fusillade::RequestTemplateInput {
                        custom_id,
                        endpoint: ai_base_url.clone(),
                        method,
                        path: url,
                        body,
                        model,
                        api_key: String::new(),
                    };
                    let _ = tx.send(FileStreamItem::Template(template)).await;
                    template_count += 1;
                }
                Err(_) => {
                    // Tier 1: garbled trailing line
                    skipped_lines += 1;
                }
            }
        }

        (template_count, skipped_lines, validation_errors, validation_error_indices)
    });

    // 6. Feed the stream into fusillade's create_file_stream
    let rx_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let result = state.request_manager.create_file_stream(rx_stream).await;

    let (template_count, skipped_lines, validation_errors, validation_error_indices) =
        producer.await.map_err(|e| anyhow::anyhow!("producer task panicked: {e}"))?;

    match result {
        Ok(fusillade::FileStreamResult::Success(file_id)) => {
            // 7. Update sync entry with internal file_id, template count,
            //    skipped lines, and validation errors
            let validation_errors_json = if validation_error_indices.is_empty() {
                None
            } else {
                let errors: Vec<serde_json::Value> = validation_errors
                    .iter()
                    .map(|(idx, line, err)| serde_json::json!({"template_index": idx, "line": line, "error": err}))
                    .collect();
                Some(serde_json::json!(errors))
            };

            let mut conn = dwctl.acquire().await?;
            let updated = SyncEntries::new(&mut conn)
                .set_ingested(
                    input.sync_entry_id,
                    file_id.0,
                    template_count,
                    skipped_lines,
                    validation_errors_json.as_ref(),
                )
                .await?;
            if !updated {
                // Entry was soft-deleted mid-sync — abort without creating a batch
                tracing::info!(sync_entry_id = %input.sync_entry_id, "Sync entry deleted during ingestion, skipping activation");
                return Ok(());
            }
            // If no valid templates were created, mark entry as failed — don't create an empty batch
            if template_count == 0 {
                SyncEntries::new(&mut conn)
                    .update_status(
                        input.sync_entry_id,
                        "failed",
                        Some("No valid requests found in file — all lines were invalid or unparseable"),
                    )
                    .await?;
                SyncOperations::new(&mut conn)
                    .increment_counter(input.sync_id, "files_failed")
                    .await?;
                let _ = SyncOperations::new(&mut conn).try_complete(input.sync_id).await;
                tracing::info!(
                    sync_entry_id = %input.sync_entry_id,
                    skipped_lines,
                    validation_errors = validation_errors.len(),
                    "File has no valid requests, marked as failed"
                );
                return Ok(());
            }

            SyncOperations::new(&mut conn)
                .increment_counter(input.sync_id, "files_ingested")
                .await?;

            // 8. Enqueue ActivateBatchJob
            state
                .get_activate_batch_job()?
                .enqueue(&ActivateBatchInput {
                    sync_id: input.sync_id,
                    sync_entry_id: input.sync_entry_id,
                    connection_id: input.connection_id,
                    file_id: file_id.0,
                    template_count,
                    validation_error_indices,
                })
                .await?;

            tracing::info!(
                sync_entry_id = %input.sync_entry_id,
                file_id = %file_id,
                template_count,
                skipped_lines,
                validation_error_count = validation_errors.len(),
                "File ingested"
            );

            Ok(())
        }
        Ok(fusillade::FileStreamResult::Aborted) => {
            anyhow::bail!("file ingestion aborted during streaming")
        }
        Err(e) => {
            anyhow::bail!("file ingestion failed: {e}")
        }
    }
}

pub(crate) async fn run_activate_batch<P: PoolProvider + Clone + Send + Sync + 'static>(
    state: &TaskState<P>,
    input: &ActivateBatchInput,
) -> anyhow::Result<()> {
    use crate::db::handlers::connections::{SyncEntries, SyncOperations};
    use fusillade::Storage;

    let dwctl = &state.dwctl_pool;

    // 1. Mark entry as activating — abort if entry was soft-deleted
    {
        let mut conn = dwctl.acquire().await?;
        let updated = SyncEntries::new(&mut conn)
            .update_status(input.sync_entry_id, "activating", None)
            .await?;
        if !updated {
            tracing::info!(sync_entry_id = %input.sync_entry_id, "Sync entry deleted, skipping batch activation");
            return Ok(());
        }
    }

    // 2. Load sync config
    let sync_op = {
        let mut conn = dwctl.acquire().await?;
        SyncOperations::new(&mut conn)
            .get_by_id(input.sync_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("sync operation not found"))?
    };

    let endpoint = sync_op
        .sync_config
        .get("endpoint")
        .and_then(|v| v.as_str())
        .unwrap_or("/v1/chat/completions")
        .to_string();
    let completion_window = sync_op
        .sync_config
        .get("completion_window")
        .and_then(|v| v.as_str())
        .unwrap_or("24h")
        .to_string();

    // 3. TODO: capacity check (if respect_capacity_reservations is enabled)
    //    For now, we proceed directly. When capacity checking is added,
    //    return Err with "insufficient capacity" message to trigger retryable error.

    // 4. Resolve the connection owner's hidden batch API key.
    //    connection.user_id = the owner (org ID in org context, personal ID otherwise).
    //    sync_op.triggered_by = the individual who triggered the sync.
    //    Same pattern as create_batch handler:
    //      - key owned by target_user_id (org/user) for billing scope
    //      - created_by on key = individual for per-member attribution
    let (batch_owner, batch_api_key, api_key_id, connection_name) = {
        use crate::db::handlers::api_keys::ApiKeys;
        use crate::db::models::api_keys::ApiKeyPurpose;

        let mut conn = dwctl.acquire().await?;
        let connection = crate::db::handlers::connections::Connections::new(&mut conn)
            .get_by_id(input.connection_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("connection not found"))?;

        let owner_id = connection.user_id;
        let conn_name = connection.name.clone();
        let triggered_by = sync_op.triggered_by;

        let (secret, key_id) = ApiKeys::new(&mut conn)
            .get_or_create_hidden_key_with_id(owner_id, ApiKeyPurpose::Batch, triggered_by)
            .await
            .map_err(|e| anyhow::anyhow!("resolve batch API key: {e}"))?;

        (owner_id, secret, key_id, conn_name)
    };

    // 5. Look up sync entry for external key (used in batch provenance metadata below)
    let sync_entry = {
        let mut conn = dwctl.acquire().await?;
        SyncEntries::new(&mut conn)
            .get_by_id(input.sync_entry_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("sync entry not found: {}", input.sync_entry_id))?
    };
    let external_key = sync_entry.external_key.clone();

    // 6. Create batch record
    //    created_by = owner (org or user) for visibility scoping — same as normal batch creation
    let metadata = serde_json::json!({
        "request_source": "sync",
        "dw_source_id": input.connection_id.to_string(),
        "dw_source_name": connection_name,
        "dw_sync_id": input.sync_id.to_string(),
        "dw_external_key": external_key,
    });

    let batch_input = fusillade::BatchInput {
        file_id: fusillade::FileId(input.file_id),
        endpoint,
        completion_window,
        metadata: Some(metadata),
        created_by: Some(batch_owner.to_string()),
        api_key_id: Some(api_key_id),
        api_key: Some(batch_api_key),
        total_requests: Some(input.template_count as i64),
    };

    let batch = state
        .request_manager
        .create_batch_record(batch_input)
        .await
        .map_err(|e| anyhow::anyhow!("create batch record: {e}"))?;

    let batch_id = *batch.id;

    // 7. Populate batch synchronously (instead of enqueuing async job) so we
    //    can immediately fail requests that had validation errors during ingest.
    //    Error handling mirrors build_create_batch_job: validation errors are
    //    fatal (mark batch failed), other errors bubble up as retryable so
    //    the underway job wrapper can retry on transient failures.
    if let Err(e) = state
        .request_manager
        .populate_batch(fusillade::BatchId(batch_id), fusillade::FileId(input.file_id))
        .await
    {
        return Err(match &e {
            fusillade::FusilladeError::ValidationError(_) => {
                if let Err(mark_err) = state
                    .request_manager
                    .mark_batch_failed(fusillade::BatchId(batch_id), &e.to_string())
                    .await
                {
                    tracing::error!(batch_id = %batch_id, error = %mark_err, "Failed to mark batch as failed after validation error");
                    ActivateError::Retryable(format!("mark_batch_failed: {mark_err}")).into()
                } else {
                    ActivateError::Fatal(format!("populate batch: {e}")).into()
                }
            }
            _ => {
                // Don't mark batch as permanently failed — let underway retry
                ActivateError::Retryable(format!("populate batch: {e}")).into()
            }
        });
    }

    // 8. Fail requests whose templates came from invalid lines (tier 2 errors).
    //    Indices are passed directly from ingest (not re-parsed from the capped JSON).
    if !input.validation_error_indices.is_empty() {
        let fusillade_pool = state.request_manager.pool();

        // Find template IDs by line number (0-based template index)
        let template_ids: Vec<Uuid> =
            sqlx::query_scalar("SELECT id FROM fusillade.request_templates WHERE file_id = $1 AND line_number = ANY($2)")
                .bind(input.file_id)
                .bind(&input.validation_error_indices)
                .fetch_all(fusillade_pool)
                .await?;

        if !template_ids.is_empty() {
            let rows = sqlx::query(
                "UPDATE fusillade.requests SET state = 'failed', error = $1, failed_at = NOW() WHERE batch_id = $2 AND template_id = ANY($3) AND state = 'pending'",
            )
            .bind("Request failed validation during ingestion — check sync entry for details")
            .bind(batch_id)
            .bind(&template_ids)
            .execute(fusillade_pool)
            .await?;

            tracing::info!(
                batch_id = %batch_id,
                failed_count = rows.rows_affected(),
                "Failed invalid requests from tier 2 validation errors"
            );
        }
    }

    // 9. Update sync entry with batch_id
    {
        let mut conn = dwctl.acquire().await?;
        SyncEntries::new(&mut conn).set_activated(input.sync_entry_id, batch_id).await?;
        SyncOperations::new(&mut conn)
            .increment_counter(input.sync_id, "batches_created")
            .await?;
    }

    tracing::info!(
        sync_entry_id = %input.sync_entry_id,
        batch_id = %batch_id,
        "Batch activated"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test::utils::{create_test_config, create_test_user};
    use fusillade::{FileMetadata, FileStreamItem, RequestTemplateInput, Storage as _};
    use sqlx::PgPool;

    /// Helper: create a TaskState backed by a real fusillade schema (for create_file_stream, etc.)
    async fn setup_task_state(pool: PgPool) -> crate::tasks::TaskState<sqlx_pool_router::TestDbPools> {
        use sqlx::Executor;
        use sqlx::postgres::PgConnectOptions;

        pool.execute("CREATE SCHEMA IF NOT EXISTS fusillade")
            .await
            .expect("create fusillade schema");

        let base_opts: PgConnectOptions = pool.connect_options().as_ref().clone();
        let fusillade_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .min_connections(0)
            .connect_with(base_opts.options([("search_path", "fusillade")]))
            .await
            .expect("fusillade pool");

        fusillade::migrator().run(&fusillade_pool).await.expect("fusillade migrations");

        let fusillade_test_pools = sqlx_pool_router::TestDbPools::new(fusillade_pool)
            .await
            .expect("fusillade test pools");
        let request_manager = std::sync::Arc::new(fusillade::PostgresRequestManager::new(fusillade_test_pools, Default::default()));

        crate::tasks::TaskState {
            request_manager,
            dwctl_pool: pool,
            encryption_key: None,
            ingest_file_job: std::sync::Arc::new(std::sync::OnceLock::new()),
            activate_batch_job: std::sync::Arc::new(std::sync::OnceLock::new()),
            create_batch_job: std::sync::Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// Helper: insert a minimal connection row (no real provider, just DB presence).
    async fn insert_test_connection(pool: &PgPool, user_id: Uuid) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query!(
            r#"INSERT INTO connections (id, user_id, kind, provider, name, config_encrypted)
               VALUES ($1, $2, 'source', 'test', 'test-conn', '\x00')"#,
            id,
            user_id,
        )
        .execute(pool)
        .await
        .expect("insert connection");
        id
    }

    /// Helper: insert a sync_operation row.
    async fn insert_test_sync_op(pool: &PgPool, connection_id: Uuid, triggered_by: Uuid) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query!(
            r#"INSERT INTO sync_operations (id, connection_id, status, strategy, triggered_by, sync_config)
               VALUES ($1, $2, 'running', 'select', $3, $4)"#,
            id,
            connection_id,
            triggered_by,
            serde_json::json!({"endpoint": "/v1/chat/completions", "completion_window": "24h"}),
        )
        .execute(pool)
        .await
        .expect("insert sync_op");
        id
    }

    /// Helper: insert a sync_entry row.
    async fn insert_test_sync_entry(pool: &PgPool, sync_id: Uuid, connection_id: Uuid, external_key: &str) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query!(
            r#"INSERT INTO sync_entries (id, sync_id, connection_id, external_key, status)
               VALUES ($1, $2, $3, $4, 'ingested')"#,
            id,
            sync_id,
            connection_id,
            external_key,
        )
        .execute(pool)
        .await
        .expect("insert sync_entry");
        id
    }

    /// Feed templates through create_file_stream and return the file ID.
    async fn create_test_file<P: sqlx_pool_router::PoolProvider + Clone + Send + Sync + 'static>(
        state: &crate::tasks::TaskState<P>,
        owner_id: Uuid,
        templates: Vec<RequestTemplateInput>,
    ) -> Uuid {
        let mut items = vec![FileStreamItem::Metadata(FileMetadata {
            filename: Some("test.jsonl".to_string()),
            purpose: Some("batch".to_string()),
            uploaded_by: Some(owner_id.to_string()),
            ..Default::default()
        })];
        for t in templates {
            items.push(FileStreamItem::Template(t));
        }
        match state
            .request_manager
            .create_file_stream(futures::stream::iter(items))
            .await
            .expect("create_file_stream")
        {
            fusillade::FileStreamResult::Success(file_id) => file_id.0,
            other => panic!("unexpected file stream result: {other:?}"),
        }
    }

    fn valid_template(model: &str) -> RequestTemplateInput {
        RequestTemplateInput {
            custom_id: None,
            endpoint: "http://127.0.0.1:3001/ai".to_string(),
            method: "POST".to_string(),
            path: "/v1/chat/completions".to_string(),
            body: serde_json::json!({"model": model, "messages": [{"role": "user", "content": "hi"}]}).to_string(),
            model: model.to_string(),
            api_key: String::new(),
        }
    }

    fn invalid_template_missing_model() -> RequestTemplateInput {
        RequestTemplateInput {
            custom_id: None,
            endpoint: "http://127.0.0.1:3001/ai".to_string(),
            method: "POST".to_string(),
            path: "/v1/chat/completions".to_string(),
            body: "{}".to_string(),
            model: String::new(), // empty — tier 2 error
            api_key: String::new(),
        }
    }

    /// Integration test: simulates a JSONL file with all 3 tiers and verifies that
    /// the activate step correctly marks tier-2 requests as failed.
    ///
    /// Simulated file lines:
    ///   Line 1: valid JSON (tier 3) → template 0 → pending
    ///   Line 2: garbled non-JSON (tier 1) → skipped, no template
    ///   Line 3: valid JSON missing model (tier 2) → template 1 → should be failed
    ///   Line 4: valid JSON (tier 3) → template 2 → pending
    ///   Line 5: valid JSON missing model (tier 2) → template 3 → should be failed
    ///
    /// After activation, templates 1 and 3 should be "failed"; templates 0 and 2 "pending".
    #[sqlx::test]
    #[test_log::test]
    async fn test_three_tier_ingestion_and_activation(pool: PgPool) {
        // -- Setup --
        let state = setup_task_state(pool.clone()).await;
        let config = create_test_config();
        let _app_state = crate::test::utils::create_test_app_state_with_config(pool.clone(), config).await;

        let user = create_test_user(&pool, crate::api::models::users::Role::PlatformManager).await;
        let user_id = user.id;

        let connection_id = insert_test_connection(&pool, user_id).await;
        let sync_id = insert_test_sync_op(&pool, connection_id, user_id).await;
        let entry_id = insert_test_sync_entry(&pool, sync_id, connection_id, "data/test.jsonl").await;

        // -- Simulate the 3-tier producer output --
        // Tier 1 (garbled line) is skipped by the producer and never becomes a template.
        // We simulate the *result* of the producer: 4 templates, 2 of which are tier-2 invalid.
        let templates = vec![
            valid_template("gpt-4"),          // template 0 (file line 1) — tier 3
            invalid_template_missing_model(), // template 1 (file line 3) — tier 2
            valid_template("gpt-4"),          // template 2 (file line 4) — tier 3
            invalid_template_missing_model(), // template 3 (file line 5) — tier 2
        ];

        let file_id = create_test_file(&state, user_id, templates).await;

        // Store validation errors on the sync entry (simulating what run_ingest_file writes).
        let skipped_lines: i32 = 1; // 1 garbled line
        let validation_errors_json = serde_json::json!([
            {"template_index": 1, "line": 3, "error": "missing model field in body"},
            {"template_index": 3, "line": 5, "error": "missing model field in body"},
        ]);
        sqlx::query!(
            r#"UPDATE sync_entries SET file_id = $2, template_count = 4,
               skipped_lines = $3, validation_errors = $4
               WHERE id = $1"#,
            entry_id,
            file_id,
            skipped_lines,
            validation_errors_json,
        )
        .execute(&pool)
        .await
        .expect("update sync_entry");

        // -- Run activate --
        let input = ActivateBatchInput {
            sync_id,
            sync_entry_id: entry_id,
            connection_id,
            file_id,
            template_count: 4,
            validation_error_indices: vec![1, 3], // template indices 1 and 3 are tier-2 errors
        };

        run_activate_batch(&state, &input).await.expect("run_activate_batch");

        // -- Verify results --
        // Find the batch that was created
        let sync_entry = sqlx::query_as::<_, (Uuid, String)>("SELECT batch_id, status FROM sync_entries WHERE id = $1")
            .bind(entry_id)
            .fetch_one(&pool)
            .await
            .expect("fetch sync_entry");
        assert_eq!(sync_entry.1, "activated", "sync entry should be activated");
        let batch_id = fusillade::BatchId(sync_entry.0);

        let requests = state
            .request_manager
            .get_batch_requests(batch_id)
            .await
            .expect("get_batch_requests");

        assert_eq!(requests.len(), 4, "should have 4 requests total");

        let mut pending_count = 0;
        let mut failed_count = 0;
        for req in &requests {
            match req {
                fusillade::AnyRequest::Pending(_) => pending_count += 1,
                fusillade::AnyRequest::Failed(_) => failed_count += 1,
                other => panic!("unexpected request state: {}", other.variant()),
            }
        }

        assert_eq!(pending_count, 2, "2 valid requests should be pending");
        assert_eq!(failed_count, 2, "2 invalid requests should be failed");
    }

    /// Verify that skipped_lines and validation_errors are stored correctly in
    /// the sync_entry after ingestion (simulated producer output → DB).
    #[sqlx::test]
    #[test_log::test]
    async fn test_validation_errors_stored_correctly(pool: PgPool) {
        let state = setup_task_state(pool.clone()).await;

        let user = create_test_user(&pool, crate::api::models::users::Role::PlatformManager).await;
        let user_id = user.id;

        let connection_id = insert_test_connection(&pool, user_id).await;
        let sync_id = insert_test_sync_op(&pool, connection_id, user_id).await;
        let entry_id = insert_test_sync_entry(&pool, sync_id, connection_id, "data/test.jsonl").await;

        let templates = vec![valid_template("gpt-4"), invalid_template_missing_model(), valid_template("gpt-4")];
        let file_id = create_test_file(&state, user_id, templates).await;

        // Simulate what run_ingest_file does: update sync_entry with skipped/errors
        let skipped_lines: i32 = 2;
        let validation_errors_json = serde_json::json!([
            {"template_index": 1, "line": 4, "error": "missing model field in body"},
        ]);

        {
            use crate::db::handlers::connections::SyncEntries;

            let mut conn = pool.acquire().await.expect("acquire conn");
            let updated = SyncEntries::new(&mut conn)
                .set_ingested(
                    entry_id,
                    file_id,
                    3, // template_count
                    skipped_lines,
                    Some(&validation_errors_json),
                )
                .await
                .expect("set_ingested");
            assert!(updated, "set_ingested should return true");
        }

        // Read back and verify
        let row = sqlx::query!(
            "SELECT status, skipped_lines, validation_errors, template_count, file_id FROM sync_entries WHERE id = $1",
            entry_id,
        )
        .fetch_one(&pool)
        .await
        .expect("read sync_entry");

        assert_eq!(row.status, "ingested");
        assert_eq!(row.skipped_lines, 2);
        assert_eq!(row.template_count.unwrap(), 3);
        assert_eq!(row.file_id.unwrap(), file_id);

        let errors: Vec<serde_json::Value> = serde_json::from_value(row.validation_errors.unwrap()).expect("parse validation_errors");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0]["template_index"], 1);
        assert_eq!(errors[0]["line"], 4);
        assert_eq!(errors[0]["error"], "missing model field in body");
    }
}
