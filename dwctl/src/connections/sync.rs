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
                // TODO: when capacity reservation is implemented, return
                // TaskError::Retryable for capacity errors using a typed error
                // (not string matching) so activation retries with backoff.
                Err(e) => {
                    tracing::error!(
                        sync_entry_id = %input.sync_entry_id,
                        error = %e,
                        "ActivateBatchJob failed"
                    );
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
            .get_ingest_file_job()
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

async fn run_ingest_file<P: PoolProvider + Clone + Send + Sync + 'static>(
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
        SyncEntries::new(&mut conn)
            .update_status(input.sync_entry_id, "ingesting", None)
            .await?;
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
            return 0i32;
        }

        let mut line_buf = String::new();
        let mut template_count: i32 = 0;
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
                    return template_count;
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

                // Parse as OpenAI batch request format
                match serde_json::from_str::<serde_json::Value>(line) {
                    Ok(parsed) => {
                        let custom_id = parsed.get("custom_id").and_then(|v| v.as_str()).map(|s| s.to_string());
                        let method = parsed.get("method").and_then(|v| v.as_str()).unwrap_or("POST").to_string();
                        let url = parsed.get("url").and_then(|v| v.as_str()).unwrap_or(&api_path).to_string();
                        let body = parsed.get("body").map(|v| v.to_string()).unwrap_or_else(|| "{}".to_string());
                        let model = parsed
                            .get("body")
                            .and_then(|b| b.get("model"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();

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
                            return template_count;
                        }
                        template_count += 1;
                    }
                    Err(e) => {
                        tracing::warn!(line_num = line_number, error = %e, "Skipping invalid JSONL line");
                        // Continue — invalid lines will show as missing templates
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

        // Handle any remaining partial line
        let remaining = line_buf.trim().to_string();
        if !remaining.is_empty()
            && let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&remaining)
        {
            let custom_id = parsed.get("custom_id").and_then(|v| v.as_str()).map(|s| s.to_string());
            let method = parsed.get("method").and_then(|v| v.as_str()).unwrap_or("POST").to_string();
            let url = parsed.get("url").and_then(|v| v.as_str()).unwrap_or(&api_path).to_string();
            let body = parsed.get("body").map(|v| v.to_string()).unwrap_or_else(|| "{}".to_string());
            let model = parsed
                .get("body")
                .and_then(|b| b.get("model"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

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

        template_count
    });

    // 6. Feed the stream into fusillade's create_file_stream
    let rx_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let result = state.request_manager.create_file_stream(rx_stream).await;

    let template_count = producer.await.unwrap_or(0);

    match result {
        Ok(fusillade::FileStreamResult::Success(file_id)) => {
            // 7. Update sync entry with internal file_id and template count
            //    (source_connection_id and source_external_key are already set
            //    via FileMetadata during create_file_stream)
            let mut conn = dwctl.acquire().await?;
            SyncEntries::new(&mut conn)
                .set_ingested(input.sync_entry_id, file_id.0, template_count)
                .await?;
            SyncOperations::new(&mut conn)
                .increment_counter(input.sync_id, "files_ingested")
                .await?;

            // 8. Enqueue ActivateBatchJob
            state
                .get_activate_batch_job()
                .enqueue(&ActivateBatchInput {
                    sync_id: input.sync_id,
                    sync_entry_id: input.sync_entry_id,
                    connection_id: input.connection_id,
                    file_id: file_id.0,
                    template_count,
                })
                .await?;

            tracing::info!(
                sync_entry_id = %input.sync_entry_id,
                file_id = %file_id,
                template_count,
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

async fn run_activate_batch<P: PoolProvider + Clone + Send + Sync + 'static>(
    state: &TaskState<P>,
    input: &ActivateBatchInput,
) -> anyhow::Result<()> {
    use crate::db::handlers::connections::{SyncEntries, SyncOperations};
    use fusillade::Storage;

    let dwctl = &state.dwctl_pool;

    // 1. Mark entry as activating
    {
        let mut conn = dwctl.acquire().await?;
        SyncEntries::new(&mut conn)
            .update_status(input.sync_entry_id, "activating", None)
            .await?;
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

        let mut conn = dwctl.acquire().await?;
        let (secret, key_id) = ApiKeys::new(&mut conn)
            .get_or_create_hidden_key_with_id(owner_id, ApiKeyPurpose::Batch, triggered_by)
            .await
            .map_err(|e| anyhow::anyhow!("resolve batch API key: {e}"))?;

        (owner_id, secret, key_id, conn_name)
    };

    // 5. Look up sync entry for external key (for provenance metadata)
    let external_key = {
        let mut conn = dwctl.acquire().await?;
        SyncEntries::new(&mut conn)
            .get_by_id(input.sync_entry_id)
            .await?
            .map(|e| e.external_key)
            .unwrap_or_default()
    };

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

    // 5. Enqueue the existing batch populate job
    state
        .get_create_batch_job()
        .enqueue(&crate::api::handlers::batches::CreateBatchInput {
            batch_id: *batch.id,
            file_id: input.file_id,
        })
        .await?;

    // 6. Update sync entry with batch_id
    {
        let mut conn = dwctl.acquire().await?;
        SyncEntries::new(&mut conn).set_activated(input.sync_entry_id, *batch.id).await?;
        SyncOperations::new(&mut conn)
            .increment_counter(input.sync_id, "batches_created")
            .await?;
    }

    tracing::info!(
        sync_entry_id = %input.sync_entry_id,
        batch_id = %batch.id,
        "Batch activated"
    );

    Ok(())
}
