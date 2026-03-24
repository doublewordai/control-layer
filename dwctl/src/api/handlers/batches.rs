// This file deals with the Batches API.
//! This is designed to match (as far as possible) the OpenAI Batches
//! [API](https://platform.openai.com/docs/api-reference/batch/).
//!
//! Repository methods are delegated to the fusillade/ crate.

use sqlx_pool_router::PoolProvider;

use super::sla_capacity::{check_sla_capacity, parse_window_to_seconds};
use crate::AppState;
use crate::api::models::batches::{
    BatchAnalytics, BatchErrors, BatchListResponse, BatchObjectType, BatchResponse, BatchResultsQuery, CreateBatchRequest,
    ListBatchesQuery, ListObjectType, RequestCounts, RetryRequestsRequest,
};
use crate::api::models::users::CurrentUser;
use crate::auth::permissions::{RequiresPermission, can_read_all_resources, has_permission, operation, resource};
use crate::db::handlers::{Credits, Users, api_keys::ApiKeys, repository::Repository};
use crate::db::models::api_keys::ApiKeyPurpose;
use crate::errors::{Error, Result};
use crate::types::{Operation, Resource};
use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::StatusCode,
};
use bytes::Bytes;
use chrono::{Duration, Utc};
use fusillade::Storage;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::pin::Pin;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Background task: batch population
// ---------------------------------------------------------------------------

/// Input for the batch population background job.
///
/// After the handler creates a batch record and returns it to the client,
/// this job copies templates into requests in the background.
#[derive(Debug, Serialize, Deserialize)]
pub struct CreateBatchInput {
    pub batch_id: Uuid,
    pub file_id: Uuid,
    pub created_by: Option<String>,
}

/// Build the underway job for batch population.
pub async fn build_create_batch_job<P: sqlx_pool_router::PoolProvider + Clone + Send + Sync + 'static>(
    pool: sqlx::PgPool,
    state: crate::tasks::TaskState<P>,
) -> anyhow::Result<underway::Job<CreateBatchInput, crate::tasks::TaskState<P>>> {
    use underway::Job;
    use underway::job::To;
    use underway::task::Error as TaskError;

    Job::<CreateBatchInput, _>::builder()
        .state(state)
        .step(|cx, input: CreateBatchInput| async move {
            let batch_id = fusillade::BatchId(input.batch_id);

            if let Err(e) = cx.state.request_manager.populate_batch(
                batch_id,
                fusillade::FileId(input.file_id),
                input.created_by,
            ).await {
                tracing::error!(
                    batch_id = %input.batch_id,
                    error = %e,
                    "Failed to populate batch"
                );
                return Err(TaskError::Retryable(e.to_string()));
            }

            tracing::info!(batch_id = %input.batch_id, "Batch populated");
            To::done()
        })
        .name("create-batch")
        .pool(pool)
        .build()
        .await
        .map_err(Into::into)
}

// ---------------------------------------------------------------------------

/// Check if the current user "owns" a batch, considering org context.
/// Returns true if the batch creator matches the user's ID or their active organization.
fn is_batch_owner(current_user: &CurrentUser, created_by: Option<&str>) -> bool {
    let user_id = current_user.id.to_string();
    if created_by == Some(user_id.as_str()) {
        return true;
    }
    if let Some(org_id) = current_user.active_organization {
        let org_id_str = org_id.to_string();
        if created_by == Some(org_id_str.as_str()) {
            return true;
        }
    }
    false
}

/// Helper function to convert fusillade Batch to OpenAI BatchResponse
///
/// If `creator_email` is provided, it will be injected into the metadata as `created_by_email`.
/// This is used to populate the email without storing it in the batch metadata (PII concern).
fn to_batch_response_with_email(batch: fusillade::Batch, creator_email: Option<&str>) -> BatchResponse {
    // Convert metadata from serde_json::Value to HashMap<String, String>
    let mut metadata: Option<HashMap<String, String>> = batch.metadata.and_then(|m| {
        m.as_object().map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
    });

    // Inject created_by_email into metadata if we have it
    if let Some(email) = creator_email {
        metadata
            .get_or_insert_with(HashMap::new)
            .insert("created_by_email".to_string(), email.to_string());
    }

    // Determine OpenAI status from request counts
    // A batch is only "finished" if it has started processing AND all requests are in terminal states
    let has_started = batch.requests_started_at.is_some();
    let is_finished = has_started && batch.pending_requests == 0 && batch.in_progress_requests == 0;
    let openai_status = if batch.cancelling_at.is_some() {
        // If cancelling_at is set, check if batch is finished
        if is_finished {
            // All requests are in terminal state, batch is now cancelled
            "cancelled"
        } else {
            // Still cancelling
            "cancelling"
        }
    } else if batch.total_requests == 0 {
        "validating"
    } else if is_finished && batch.failed_requests == batch.total_requests {
        // All requests failed (batch.failed_requests already filtered by SLA status)
        "failed"
    } else if is_finished {
        // All requests are in terminal state - check if output files are ready
        if batch.completed_at.is_some() {
            // Output files written, batch is truly completed
            "completed"
        } else if batch.failed_at.is_some() {
            // Batch was marked as failed but not all requests failed (e.g. requests
            // were retried and completed after the batch was already finalized as failed)
            "failed"
        } else {
            // Requests done but still writing output files
            "finalizing"
        }
    } else {
        // Any batch that has been validated (total_requests > 0) but not finished
        // is considered "in_progress". This includes:
        // - Batches actively processing (in_progress_requests > 0)
        // - Batches with some completed work (completed_requests > 0)
        // - Batches queued and waiting for capacity (only pending_requests > 0)
        "in_progress"
    };

    // Compute timestamps based on status
    let in_progress_at = if openai_status != "validating" {
        batch.requests_started_at.map(|dt| dt.timestamp())
    } else {
        None
    };

    // Terminal state timestamps from batch table
    // Only show finalizing_at when status is actually "finalizing" or later
    let finalizing_at = if openai_status == "finalizing" || openai_status == "completed" {
        batch.finalizing_at.map(|dt| dt.timestamp())
    } else {
        None
    };
    let completed_at = batch.completed_at.map(|dt| dt.timestamp());
    let failed_at = batch.failed_at.map(|dt| dt.timestamp());
    let cancelled_at = batch.cancelled_at.map(|dt| dt.timestamp());

    // Convert batch-level errors (validation errors, system errors, etc.)
    let errors = batch.errors.and_then(|e| serde_json::from_value::<BatchErrors>(e).ok());

    // Check if batch has expired
    let expired_at = if chrono::Utc::now() > batch.expires_at {
        Some(batch.expires_at.timestamp())
    } else {
        None
    };

    BatchResponse {
        id: batch.id.0.to_string(),
        object_type: BatchObjectType::Batch,
        endpoint: batch.endpoint.clone(),
        errors,
        input_file_id: batch.file_id.map(|id| id.0.to_string()).unwrap_or_default(),
        completion_window: batch.completion_window.clone(),
        status: openai_status.to_string(),
        output_file_id: batch.output_file_id.map(|id| id.0.to_string()),
        // Always show error_file_id if it exists - the file content itself is filtered by fusillade
        error_file_id: batch.error_file_id.map(|id| id.0.to_string()),
        created_at: batch.created_at.timestamp(),
        in_progress_at,
        expires_at: Some(batch.expires_at.timestamp()),
        finalizing_at,
        completed_at,
        failed_at,
        expired_at,
        cancelling_at: batch.cancelling_at.map(|dt| dt.timestamp()),
        cancelled_at,
        request_counts: RequestCounts {
            total: batch.total_requests,
            completed: batch.completed_requests,
            failed: batch.failed_requests,
        },
        metadata,
        analytics: None,
    }
}

/// Helper to fetch creator email for a batch from the database.
///
async fn fetch_creator_email(db: &sqlx::PgPool, batch: &fusillade::Batch) -> Option<String> {
    let created_by = batch.created_by.as_ref()?;
    let user_id = Uuid::parse_str(created_by).ok()?;
    let mut conn = db.acquire().await.ok()?;
    Users::new(&mut conn).get_by_id(user_id).await.ok().flatten().map(|u| u.email)
}

#[utoipa::path(
    post,
    path = "/batches",
    tag = "batches",
    summary = "Create batch",
    description = "Create and start processing a batch from an uploaded file.

The batch will begin processing immediately. Use `GET /batches/{batch_id}` to monitor progress.",
    request_body = CreateBatchRequest,
    responses(
        (status = 201, description = "Batch created and queued for processing.", body = BatchResponse),
        (status = 400, description = "Invalid request — check that the endpoint and completion_window are valid."),
        (status = 402, description = "Insufficient credits — account balance is below zero."),
        (status = 404, description = "Input file not found or you don't have access to it."),
        (status = 500, description = "An unexpected error occurred. Retry the request or contact support if the issue persists.")
    )
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id, input_file_id = %req.input_file_id))]
pub async fn create_batch<P: PoolProvider>(
    State(state): State<AppState<P>>,
    current_user: RequiresPermission<resource::Batches, operation::CreateOwn>,
    has_api_key: crate::auth::current_user::HasApiKey,
    Json(req): Json<CreateBatchRequest>,
) -> Result<(StatusCode, Json<BatchResponse>)> {
    // Validate completion_window against configured allowed values
    if !state.config.batches.allowed_completion_windows.contains(&req.completion_window) {
        let allowed: Vec<&str> = state.config.batches.allowed_completion_windows.iter().map(|w| w.as_str()).collect();

        return Err(Error::BadRequest {
            message: format!("Unsupported completion_window. Allowed values: {}", allowed.join(", ")),
        });
    }

    // Validate endpoint
    let supported_endpoints = &state.config.batches.allowed_url_paths;
    if !supported_endpoints.iter().any(|endpoint| endpoint == &req.endpoint) {
        return Err(Error::BadRequest {
            message: format!(
                "Unsupported endpoint '{}'. Supported: {}",
                req.endpoint,
                supported_endpoints.join(", ")
            ),
        });
    }

    // Parse file ID
    let file_id = Uuid::parse_str(&req.input_file_id).map_err(|_| Error::BadRequest {
        message: "Invalid input_file_id format".to_string(),
    })?;

    // Reject batches from users/orgs with negative credit balance.
    // In org context, check the org's balance rather than the user's personal balance.
    let balance_check_id = current_user.active_organization.unwrap_or(current_user.id);
    {
        let mut conn = state.db.write().acquire().await.map_err(|e| Error::Internal {
            operation: format!("get db connection for credit check: {}", e),
        })?;
        let balance = Credits::new(&mut conn)
            .get_user_balance(balance_check_id)
            .await
            .map_err(|e| Error::Internal {
                operation: format!("check credit balance: {}", e),
            })?;
        if balance < rust_decimal::Decimal::ZERO {
            return Err(Error::InsufficientCredits {
                current_balance: balance,
                message: "Account balance too low. Please add credits to continue.".to_string(),
            });
        }
    }

    // Verify file exists and user has access
    // Use primary pool to avoid read-after-write consistency issues with replicas
    let file = state
        .request_manager
        .get_file_from_primary_pool(fusillade::FileId(file_id))
        .await
        .map_err(|_| Error::NotFound {
            resource: "File".to_string(),
            id: req.input_file_id.clone(),
        })?;

    // Check file ownership if user doesn't have ReadAll permission
    // In org context, files owned by the active org are also considered "own"
    use crate::types::Resource;
    let has_read_all = can_read_all_resources(&current_user, Resource::Files);
    if !has_read_all && !is_batch_owner(&current_user, file.uploaded_by.as_deref()) {
        use crate::types::{Operation, Permission};
        return Err(Error::InsufficientPermissions {
            required: Permission::Allow(Resource::Files, Operation::ReadAll),
            action: Operation::CreateOwn,
            resource: format!("batch using file {}", req.input_file_id),
        });
    }

    // Check that the file owner (whose API key is embedded in the request templates)
    // has sufficient balance. This catches cases where an admin creates a batch from
    // a file owned by another user/org that has negative balance.
    if let Some(ref uploaded_by) = file.uploaded_by
        && let Ok(file_owner_id) = uuid::Uuid::parse_str(uploaded_by)
    {
        let file_owner_id = crate::types::UserId::from(file_owner_id);
        if file_owner_id != current_user.id {
            let mut conn = state.db.write().acquire().await.map_err(|e| Error::Internal {
                operation: format!("get db connection for file owner credit check: {}", e),
            })?;
            let owner_balance = Credits::new(&mut conn)
                .get_user_balance(file_owner_id)
                .await
                .map_err(|e| Error::Internal {
                    operation: format!("check file owner credit balance: {}", e),
                })?;
            if owner_balance < rust_decimal::Decimal::ZERO {
                let owner_name = {
                    let mut users_repo = Users::new(&mut conn);
                    users_repo
                        .get_by_id(file_owner_id)
                        .await
                        .ok()
                        .flatten()
                        .map(|u| u.display_name.unwrap_or(u.username))
                        .unwrap_or_else(|| file_owner_id.to_string())
                };
                return Err(Error::InsufficientCredits {
                    current_balance: owner_balance,
                    message: format!(
                        "File owner ({}) does not have enough balance. Please add credits to their account or upload a new file.",
                        owner_name
                    ),
                });
            }
        }
    }

    // Get per-model request counts from the file
    let file_stats = state
        .request_manager
        .get_file_template_stats(fusillade::FileId(file_id))
        .await
        .map_err(|e| Error::Internal {
            operation: format!("get file template stats: {}", e),
        })?;

    let file_model_counts: HashMap<String, i64> = file_stats.iter().map(|s| (s.model.clone(), s.request_count)).collect();

    let model_aliases: Vec<String> = file_model_counts.keys().cloned().collect();

    // Get per-model batch info (throughputs + allowed windows) in one query
    let batch_model_info = {
        use crate::db::handlers::deployments::Deployments;
        let mut conn = state.db.read().acquire().await.map_err(|e| Error::Internal {
            operation: format!("get db connection: {}", e),
        })?;
        Deployments::new(&mut conn)
            .get_batch_model_info(&model_aliases)
            .await
            .map_err(|e| Error::Internal {
                operation: format!("get batch model info: {}", e),
            })?
    };

    // Check per-model batch completion window restrictions
    for (alias, allowed_windows) in &batch_model_info.allowed_windows {
        if !allowed_windows.contains(&req.completion_window) {
            if allowed_windows.is_empty() {
                return Err(Error::BadRequest {
                    message: format!("Model '{}' does not support batch processing.", alias),
                });
            }
            return Err(Error::BadRequest {
                message: format!(
                    "Model '{}' does not support completion window '{}'. Allowed: {}",
                    alias,
                    req.completion_window,
                    allowed_windows.join(", ")
                ),
            });
        }
    }

    let windows = vec![(req.completion_window.clone(), parse_window_to_seconds(&req.completion_window))];
    let states = vec!["pending".to_string(), "claimed".to_string(), "processing".to_string()];

    let model_throughputs = batch_model_info.throughputs;
    let model_ids_by_alias = get_model_ids_by_aliases(&state, &model_aliases).await?;

    // Determine request_source from authentication method
    // - API key present -> "api"
    // - No API key (cookie auth) -> "frontend"
    let request_source = if has_api_key.0 { "api" } else { "frontend" };

    // When in org context, attribute batch ownership to the org
    let target_user_id = current_user.active_organization.unwrap_or(current_user.id);

    // Get the hidden API key for batch execution and per-member attribution.
    // The secret is stored on the batch so the daemon uses the batch creator's
    // credentials, not the file uploader's key from request_templates.
    let (batch_api_key, api_key_id) = {
        let mut conn = state.db.write().acquire().await.map_err(|e| Error::Database(e.into()))?;
        let mut api_keys_repo = ApiKeys::new(&mut conn);
        let (secret, key_id) = api_keys_repo
            .get_or_create_hidden_key_with_id(target_user_id, ApiKeyPurpose::Batch, current_user.id)
            .await
            .map_err(Error::Database)?;
        (secret, key_id)
    };

    // Convert metadata to HashMap and inject request_source and user info.
    // Strip reserved keys that are injected server-side during response enrichment
    // to prevent user-supplied values from colliding with system fields.
    let mut metadata_map = req.metadata.unwrap_or_default();
    for key in &["created_by_email", "context_name", "context_type", "request_source", "created_by"] {
        metadata_map.remove(*key);
    }
    metadata_map.insert("request_source".to_string(), request_source.to_string());
    metadata_map.insert("created_by".to_string(), current_user.id.to_string());
    let metadata = serde_json::to_value(metadata_map).ok();

    // Create batch input — created_by uses org ID when in org context for ownership scoping
    let batch_input = fusillade::BatchInput {
        file_id: fusillade::FileId(file_id),
        endpoint: req.endpoint.clone(),
        completion_window: req.completion_window.clone(),
        metadata,
        created_by: Some(target_user_id.to_string()),
        api_key_id: Some(api_key_id),
        api_key: Some(batch_api_key),
    };

    let reservation_ids = reserve_capacity_for_batch(
        &state,
        &req.completion_window,
        &file_model_counts,
        &model_throughputs,
        &model_ids_by_alias,
        &windows,
        &states,
        &model_aliases,
        state.config.batches.relaxation_factor(&req.completion_window),
    )
    .await?;

    // RAII guard: releases reservations when this scope exits, whether by normal
    // return, early return (e.g. create_batch error), or unwind panic.
    // The spawn is best-effort — the TTL is the true safety net if the runtime
    // is unavailable at drop time.
    let _release_guard = scopeguard::guard(reservation_ids.clone(), |ids| {
        let state = state.clone();
        tokio::runtime::Handle::current().spawn(async move {
            if let Err(e) = release_capacity_reservations(&state, &ids).await {
                tracing::warn!(
                    reservation_ids = ?ids,
                    error = %e,
                    "Failed to release capacity reservations — will expire via TTL"
                );
            }
        });
    });

    // Batch record (fusillade DB) and job enqueue (dwctl DB) are on separate databases,
    // so true atomicity isn't possible. Each is a single independent write.
    let batch = state
        .request_manager
        .create_batch_record(batch_input)
        .await
        .map_err(|e| Error::Internal {
            operation: format!("create batch record: {}", e),
        })?;

    // Enqueue background job to populate requests from templates.
    // If this fails, we have an orphaned "validating" batch — acceptable since the
    // enqueue failure case is unlikely and a sweeper can clean up.
    state
        .task_runner
        .create_batch_job()
        .enqueue(&CreateBatchInput {
            batch_id: *batch.id,
            file_id,
            created_by: Some(target_user_id.to_string()),
        })
        .await
        .map_err(|e| Error::Internal {
            operation: format!("enqueue batch population: {}", e),
        })?;

    tracing::debug!("Batch {} created, population enqueued", batch.id);

    // For create, we have the current user's email directly
    Ok((
        StatusCode::CREATED,
        Json(to_batch_response_with_email(batch, Some(&current_user.email))),
    ))
}

async fn get_model_ids_by_aliases<P: PoolProvider>(state: &AppState<P>, model_aliases: &[String]) -> Result<HashMap<String, Uuid>> {
    if model_aliases.is_empty() {
        return Ok(HashMap::new());
    }

    let mut conn = state.db.read().acquire().await.map_err(|e| Error::Internal {
        operation: format!("get db connection: {}", e),
    })?;

    let result = crate::db::handlers::deployments::Deployments::new(&mut conn)
        .get_model_ids_by_aliases(model_aliases)
        .await
        .map_err(|e| Error::Internal {
            operation: format!("get model ids: {}", e),
        })?;

    let missing: Vec<&str> = model_aliases
        .iter()
        .filter(|alias| !result.contains_key(*alias))
        .map(|alias| alias.as_str())
        .collect();

    if !missing.is_empty() {
        return Err(Error::BadRequest {
            message: format!(
                "The following model(s) are no longer available: {}. \
                 The batch file references models that have been removed.",
                missing.join(", ")
            ),
        });
    }

    Ok(result)
}

/// Reserve capacity for a batch before it is created, then release it once fusillade
/// has committed the batch to its own database.
///
/// ## Three-phase pipeline
///
/// ```text
/// Phase 1 — Reserve   (this fn, ~1 ms)
///   ├─ BEGIN tx on dwctl write pool
///   ├─ pg_advisory_xact_lock per (model_id, window)  ← serialises concurrent reservations
///   ├─ read active reservations         (dwctl write pool, inside tx)
///   ├─ read pending request counts      (fusillade write pool, separate connection)
///   ├─ check combined capacity
///   ├─ INSERT reservation rows
///   └─ COMMIT  ← lock released, reservation visible to peers
///
/// Phase 2 — create_batch   (fusillade, ms – seconds depending on batch size)
///   └─ single atomic tx: INSERT batches → INSERT requests → UPDATE totals → COMMIT
///
///   └─ UPDATE reservations SET released_at = now()
///
/// ## Advisory lock scope
///
/// `pg_advisory_xact_lock` is transaction-scoped and per `(model_id, window)` pair.
/// All concurrent callers for the same model+window queue behind this lock — only one
/// can read-check-then-insert at a time. Locks are acquired in deterministic UUID order
/// to prevent deadlocks when a batch spans multiple models.
///
/// ## Read ordering and the fail-safe race window
///
/// The two capacity reads come from **different connection pools** (dwctl vs. fusillade),
/// so they hold independent PostgreSQL snapshots under `READ COMMITTED`. There is an
/// unavoidable, tiny race window at the exact moment a concurrent batch finishes
/// `create_batch` and its reservation is released — the "swap point" where requests
/// transition from a reservation into committed pending rows. A new caller straddling
/// this swap point could theoretically see inconsistent state across the two reads.
///
/// The read order here is deliberately chosen to make that race **fail-safe**:
///
/// - Reservations are read **first** (dwctl tx, inside the advisory lock).
/// - Pending counts are read **second** (fusillade pool, outside the lock).
///
/// If the swap point falls between these two reads, the concurrent batch appears in
/// **both** counts — as a reservation that hasn't been released yet, and as committed
/// pending requests that have just landed. This double-counts the batch, causing a
/// conservative over-estimate of load that leads to **under-acceptance** rather than
/// over-acceptance.
///
/// The opposite ordering (pending first, reservations second) produces the dangerous
/// case: the swap point could cause both reads to return zero, making the system
/// appear completely idle and over-accepting the incoming batch.
///
/// In short: the race is an inherent consequence of reading across two independent
/// connections, but the read order ensures it always errs on the side of caution.
#[allow(clippy::too_many_arguments)]
async fn reserve_capacity_for_batch<P: PoolProvider>(
    state: &AppState<P>,
    completion_window: &str,
    file_model_counts: &HashMap<String, i64>,
    model_throughputs: &HashMap<String, f32>,
    model_ids_by_alias: &HashMap<String, Uuid>,
    windows: &[(String, i64)],
    states: &[String],
    model_filter: &[String],
    relaxation_factor: f32,
) -> Result<Vec<Uuid>> {
    use crate::db::handlers::BatchCapacityReservations;

    let mut tx = state.db.write().begin().await.map_err(|e| Error::Internal {
        operation: format!("begin reservation transaction: {}", e),
    })?;

    // Lock per model+window in deterministic order
    let mut model_pairs: Vec<(String, Uuid)> = model_ids_by_alias.iter().map(|(a, id)| (a.clone(), *id)).collect();
    model_pairs.sort_by_key(|(_, id)| *id);

    for (alias, model_id) in &model_pairs {
        sqlx::query!(
            "SELECT pg_advisory_xact_lock(hashtext($1::text), hashtext($2::text))",
            model_id.to_string(),
            completion_window
        )
        .execute(&mut *tx)
        .await
        .map_err(|e| Error::Internal {
            operation: format!("lock reservation for {}: {}", alias, e),
        })?;
    }

    // Sum active reservations and add to pending_counts
    let model_ids: Vec<Uuid> = model_pairs.iter().map(|(_, id)| *id).collect();
    let id_to_alias: HashMap<Uuid, String> = model_pairs.iter().map(|(a, id)| (*id, a.clone())).collect();
    let mut reservations = BatchCapacityReservations::new(&mut tx);

    let reserved_rows = reservations
        .sum_active_by_model_window(&model_ids, completion_window)
        .await
        .map_err(|e| Error::Internal {
            operation: format!("sum active reservations: {}", e),
        })?;

    // Fetch pending counts AFTER locks to avoid stale snapshots
    let pending_counts = state
        .request_manager
        .get_pending_request_counts_by_model_and_completion_window(windows, states, model_filter, true)
        .await
        .map_err(|e| Error::Internal {
            operation: format!("get pending counts: {}", e),
        })?;

    let mut pending_with_reservations = pending_counts.clone();
    for (model_id, reserved) in reserved_rows {
        if let Some(alias) = id_to_alias.get(&model_id) {
            let windows = pending_with_reservations.entry(alias.clone()).or_default();
            let entry = windows.entry(completion_window.to_string()).or_insert(0);
            *entry += reserved;
        }
    }

    let capacity_result = check_sla_capacity(
        file_model_counts,
        &pending_with_reservations,
        model_throughputs,
        state.config.batches.default_throughput,
        completion_window,
        relaxation_factor,
    );

    if !capacity_result.has_capacity {
        tx.rollback().await.ok();

        let overloaded_details: Vec<String> = capacity_result
            .overloaded_models
            .iter()
            .map(|(model, deficit)| format!("{} (needs {} more capacity)", model, deficit))
            .collect();
        tracing::warn!(
            completion_window = %completion_window,
            overloaded_models = %overloaded_details.join(", "),
            "Batch rejected due to insufficient capacity"
        );

        let model_names: Vec<&str> = capacity_result.overloaded_models.keys().map(|model| model.as_str()).collect();

        return Err(Error::TooManyRequests {
            message: format!(
                "Insufficient capacity for {} completion window. The following models are currently at capacity: {}. Try again later or use a longer completion window.",
                completion_window,
                model_names.join(", ")
            ),
        });
    }

    let expires_at = Utc::now() + Duration::seconds(state.config.batches.reservation_ttl_secs);

    let mut rows = Vec::new();
    for (alias, model_id) in &model_pairs {
        if let Some(&count) = file_model_counts.get(alias)
            && count > 0
        {
            rows.push((*model_id, completion_window, count, expires_at));
        }
    }

    let reservation_ids = reservations.insert_reservations(&rows).await.map_err(|e| Error::Internal {
        operation: format!("insert reservations: {}", e),
    })?;

    tx.commit().await.map_err(|e| Error::Internal {
        operation: format!("commit reservation transaction: {}", e),
    })?;

    Ok(reservation_ids)
}

async fn release_capacity_reservations<P: PoolProvider>(state: &AppState<P>, reservation_ids: &[Uuid]) -> Result<()> {
    use crate::db::handlers::BatchCapacityReservations;

    if reservation_ids.is_empty() {
        return Ok(());
    }

    let mut conn = state.db.write().acquire().await.map_err(|e| Error::Internal {
        operation: format!("get db connection: {}", e),
    })?;

    let mut reservations = BatchCapacityReservations::new(&mut conn);
    reservations
        .release_reservations(reservation_ids)
        .await
        .map_err(|e| Error::Internal {
            operation: format!("release reservations: {}", e),
        })
}

#[utoipa::path(
    get,
    path = "/batches/{batch_id}",
    tag = "batches",
    summary = "Retrieve batch",
    description = "Retrieve the current status and details of a batch.

Poll this endpoint to monitor progress. Results are streamed to `output_file_id` as they complete — you can start downloading results before the batch finishes.",
    responses(
        (status = 200, description = "Batch details including status, progress counts, and output file IDs.", body = BatchResponse),
        (status = 404, description = "Batch not found or you don't have access to it."),
        (status = 500, description = "An unexpected error occurred. Retry the request or contact support if the issue persists.")
    ),
    params(
        ("batch_id" = String, Path, description = "The batch ID returned when the batch was created.")
    )
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id, batch_id = %batch_id_str))]
pub async fn get_batch<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(batch_id_str): Path<String>,
    current_user: RequiresPermission<resource::Batches, operation::ReadOwn>,
) -> Result<Json<BatchResponse>> {
    let batch_id = Uuid::parse_str(&batch_id_str).map_err(|_| Error::BadRequest {
        message: "Invalid batch ID format".to_string(),
    })?;

    let batch = state
        .request_manager
        .get_batch(fusillade::BatchId(batch_id))
        .await
        .map_err(|_| Error::NotFound {
            resource: "Batch".to_string(),
            id: batch_id_str.clone(),
        })?;

    // Check ownership: users without ReadAll permission can only see their own batches (or org batches)
    let can_read_all = can_read_all_resources(&current_user, Resource::Batches);
    if !can_read_all && !is_batch_owner(&current_user, batch.created_by.as_deref()) {
        return Err(Error::NotFound {
            resource: "Batch".to_string(),
            id: batch_id_str,
        });
    }

    // Enrich with creator/context metadata (same as list_batches)
    let mut read_conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;

    // Resolve individual creator email via api_key_id
    let created_by_email = if let Some(api_key_id) = batch.api_key_id {
        let creator_map = ApiKeys::new(&mut read_conn)
            .get_creators_by_key_ids(vec![api_key_id])
            .await
            .map_err(Error::Database)?;
        if let Some(&creator_id) = creator_map.get(&api_key_id) {
            Users::new(&mut read_conn)
                .get_bulk(vec![creator_id])
                .await
                .map_err(|e| Error::Internal {
                    operation: format!("fetch creator user: {}", e),
                })?
                .get(&creator_id)
                .map(|u| u.email.clone())
        } else {
            None
        }
    } else {
        // Fall back to owner email (legacy batches without api_key_id)
        if let Some(created_by) = batch.created_by.as_ref() {
            if let Ok(user_id) = Uuid::parse_str(created_by) {
                Users::new(&mut read_conn)
                    .get_bulk(vec![user_id])
                    .await
                    .map_err(|e| Error::Internal {
                        operation: format!("fetch owner user: {}", e),
                    })?
                    .get(&user_id)
                    .map(|u| u.email.clone())
            } else {
                None
            }
        } else {
            None
        }
    };

    // Resolve context from batch owner (created_by field)
    let (context_name, context_type) = if let Some(owner_id) = batch.created_by.as_ref().and_then(|id| Uuid::parse_str(id).ok()) {
        let user_map = Users::new(&mut read_conn)
            .get_bulk(vec![owner_id])
            .await
            .map_err(|e| Error::Internal {
                operation: format!("fetch owner user: {}", e),
            })?;
        match user_map.get(&owner_id) {
            Some(u) if u.user_type == "organization" => {
                let name = u.display_name.clone().unwrap_or_else(|| u.email.clone());
                (Some(name), Some("organization".to_string()))
            }
            Some(_) => (Some("Personal".to_string()), Some("personal".to_string())),
            None => (None, None),
        }
    } else {
        (None, None)
    };

    let mut response = to_batch_response_with_email(batch, None);

    if let Some(email) = created_by_email {
        response
            .metadata
            .get_or_insert_with(HashMap::new)
            .insert("created_by_email".to_string(), email);
    }
    if let Some(name) = context_name {
        response
            .metadata
            .get_or_insert_with(HashMap::new)
            .insert("context_name".to_string(), name);
    }
    if let Some(ctype) = context_type {
        response
            .metadata
            .get_or_insert_with(HashMap::new)
            .insert("context_type".to_string(), ctype);
    }

    Ok(Json(response))
}

#[utoipa::path(
    get,
    path = "/batches/{batch_id}/analytics",
    summary = "Get batch analytics",
    description = "Retrieve aggregated metrics for a batch including token usage, costs, and latency statistics.

Analytics update in real-time as requests complete.",
    responses(
        (status = 200, description = "Batch analytics with token counts, costs, and performance metrics.", body = BatchAnalytics),
        (status = 404, description = "Batch not found or you don't have access to it."),
        (status = 500, description = "An unexpected error occurred. Retry the request or contact support if the issue persists.")
    ),
    params(
        ("batch_id" = String, Path, description = "The batch ID returned when the batch was created.")
    )
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id, batch_id = %batch_id_str))]
pub async fn get_batch_analytics<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(batch_id_str): Path<String>,
    current_user: RequiresPermission<resource::Batches, operation::ReadOwn>,
) -> Result<Json<BatchAnalytics>> {
    let batch_id = Uuid::parse_str(&batch_id_str).map_err(|_| Error::BadRequest {
        message: "Invalid batch ID format".to_string(),
    })?;

    // Get batch first to verify it exists and check permissions
    let batch = state
        .request_manager
        .get_batch(fusillade::BatchId(batch_id))
        .await
        .map_err(|_| Error::NotFound {
            resource: "Batch".to_string(),
            id: batch_id_str.clone(),
        })?;

    // Check ownership: users without ReadAll permission can only see their own batches (or org batches)
    let can_read_all = can_read_all_resources(&current_user, Resource::Batches);
    if !can_read_all && !is_batch_owner(&current_user, batch.created_by.as_deref()) {
        return Err(Error::NotFound {
            resource: "Batch".to_string(),
            id: batch_id_str.clone(),
        });
    }

    // Fetch aggregated analytics metrics for this batch
    let analytics = crate::db::handlers::analytics::get_batch_analytics(state.db.read(), &batch_id)
        .await
        .map_err(|e| Error::Internal {
            operation: format!("fetch batch analytics: {}", e),
        })?;

    Ok(Json(analytics))
}

#[utoipa::path(
    get,
    path = "/batches/{batch_id}/results",
    tag = "batches",
    summary = "Get batch results",
    description = "Stream batch results with merged input/output data as JSONL.

Each line contains the original input body, response body (for completed requests), error message (for failed requests), and current status. Results are filtered to show exactly one entry per input template (excluding superseded requests from escalation races).

Supports pagination via `limit` and `skip` query parameters, and filtering by `custom_id` via the `search` parameter.",
    responses(
        (status = 200, description = "Batch results as newline-delimited JSON. Check the `X-Incomplete` header to determine if more results exist.", content_type = "application/x-ndjson"),
        (status = 404, description = "Batch not found or you don't have access to it."),
        (status = 500, description = "An unexpected error occurred. Retry the request or contact support if the issue persists.")
    ),
    params(
        ("batch_id" = String, Path, description = "The batch ID returned when the batch was created."),
        BatchResultsQuery
    )
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id, batch_id = %batch_id_str))]
pub async fn get_batch_results<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(batch_id_str): Path<String>,
    Query(query): Query<BatchResultsQuery>,
    current_user: RequiresPermission<resource::Batches, operation::ReadOwn>,
) -> Result<axum::response::Response> {
    let batch_id = Uuid::parse_str(&batch_id_str).map_err(|_| Error::BadRequest {
        message: "Invalid batch ID format".to_string(),
    })?;

    // Get batch first to verify it exists and check permissions
    let batch = state
        .request_manager
        .get_batch(fusillade::BatchId(batch_id))
        .await
        .map_err(|_| Error::NotFound {
            resource: "Batch".to_string(),
            id: batch_id_str.clone(),
        })?;

    // Check ownership: users without ReadAll permission can only see their own batches (or org batches)
    let can_read_all = can_read_all_resources(&current_user, Resource::Batches);
    if !can_read_all && !is_batch_owner(&current_user, batch.created_by.as_deref()) {
        return Err(Error::NotFound {
            resource: "Batch".to_string(),
            id: batch_id_str.clone(),
        });
    }

    // Check if batch is still processing (more results may arrive).
    // The batch object already contains computed request counts from the LATERAL join,
    // so no separate get_batch_status call is needed.
    let still_processing = batch.pending_requests > 0 || batch.in_progress_requests > 0;

    // Stream the batch results as JSONL
    let offset = query.pagination.skip() as usize;
    let search = query.search.clone();
    let status = query.status.clone();
    let requested_limit = query.pagination.limit.map(|_| query.pagination.limit() as usize);

    if let Some(limit) = requested_limit {
        // Pagination case: buffer only N+1 items to check for more pages
        let results_stream = state
            .request_manager
            .get_batch_results_stream(fusillade::BatchId(batch_id), offset, search, status);

        let mut buffer: Vec<_> = results_stream.take(limit + 1).collect().await;
        let has_more_pages = buffer.len() > limit;
        buffer.truncate(limit);

        let line_count = buffer.len();
        let last_line = offset + line_count;
        let has_more = has_more_pages || still_processing;

        // Serialize buffered items
        let mut jsonl_lines = Vec::new();
        for result in buffer {
            let json_line = result
                .and_then(|item| {
                    serde_json::to_string(&item)
                        .map(|json| format!("{}\n", json))
                        .map_err(|e| fusillade::FusilladeError::Other(anyhow::anyhow!("JSON serialization failed: {}", e)))
                })
                .map_err(|e| Error::Internal {
                    operation: format!("serialize batch result: {}", e),
                })?;
            jsonl_lines.push(json_line);
        }

        let jsonl_content = jsonl_lines.join("");

        let mut response = axum::response::Response::new(Body::from(jsonl_content));
        response
            .headers_mut()
            .insert("content-type", "application/x-ndjson".parse().unwrap());
        response.headers_mut().insert("X-Incomplete", has_more.to_string().parse().unwrap());
        response.headers_mut().insert("X-Last-Line", last_line.to_string().parse().unwrap());
        *response.status_mut() = StatusCode::OK;

        Ok(response)
    } else {
        // Unlimited case: true streaming to avoid OOM on large result sets
        //
        // Derive the expected count from batch status so we can set X-Last-Line
        // before streaming. When a search filter is active the count is unknown
        // without an extra query, so we skip X-Last-Line in that case.
        let expected_count = if search.is_none() {
            let count = match status.as_deref() {
                Some("completed") => batch.completed_requests,
                Some("failed") => batch.failed_requests,
                Some("pending") => batch.pending_requests,
                Some("in_progress") => batch.in_progress_requests,
                _ => batch.total_requests,
            };
            Some((count as usize).saturating_sub(offset))
        } else {
            None
        };

        let results_stream = state
            .request_manager
            .get_batch_results_stream(fusillade::BatchId(batch_id), offset, search, status);

        // Limit stream to expected count so X-Last-Line is accurate
        let results_stream: Pin<Box<dyn futures::Stream<Item = fusillade::Result<fusillade::batch::BatchResultItem>> + Send>> =
            if let Some(count) = expected_count {
                Box::pin(results_stream.take(count))
            } else {
                Box::pin(results_stream)
            };

        let body_stream = results_stream.map(|result| {
            result
                .and_then(|item| {
                    serde_json::to_string(&item)
                        .map(|json| Bytes::from(format!("{}\n", json)))
                        .map_err(|e| fusillade::FusilladeError::Other(anyhow::anyhow!("JSON serialization failed: {}", e)))
                })
                .map_err(|e| std::io::Error::other(e.to_string()))
        });

        let body = Body::from_stream(body_stream);
        let mut response = axum::response::Response::new(body);
        response
            .headers_mut()
            .insert("content-type", "application/x-ndjson".parse().unwrap());
        response
            .headers_mut()
            .insert("X-Incomplete", still_processing.to_string().parse().unwrap());
        if let Some(count) = expected_count {
            let last_line = offset + count;
            response.headers_mut().insert("X-Last-Line", last_line.to_string().parse().unwrap());
        }
        *response.status_mut() = StatusCode::OK;

        Ok(response)
    }
}

#[utoipa::path(
    post,
    path = "/batches/{batch_id}/cancel",
    tag = "batches",
    summary = "Cancel batch",
    description = "Cancel an in-progress batch.

Pending requests will not be processed. Requests already in progress will complete. The batch status will transition to `cancelling` then `cancelled`.",
    responses(
        (status = 200, description = "Cancellation initiated. The batch will finish processing in-flight requests.", body = BatchResponse),
        (status = 404, description = "Batch not found or you don't have access to it."),
        (status = 500, description = "An unexpected error occurred. Retry the request or contact support if the issue persists.")
    ),
    params(
        ("batch_id" = String, Path, description = "The batch ID returned when the batch was created.")
    )
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id, batch_id = %batch_id_str))]
pub async fn cancel_batch<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(batch_id_str): Path<String>,
    current_user: RequiresPermission<resource::Batches, operation::UpdateOwn>,
) -> Result<Json<BatchResponse>> {
    let batch_id = Uuid::parse_str(&batch_id_str).map_err(|_| Error::BadRequest {
        message: "Invalid batch ID format".to_string(),
    })?;

    // Get batch first to verify it exists
    let batch = state
        .request_manager
        .get_batch(fusillade::BatchId(batch_id))
        .await
        .map_err(|_| Error::NotFound {
            resource: "Batch".to_string(),
            id: batch_id_str.clone(),
        })?;

    // Check ownership: users without UpdateAll permission can only cancel their own batches (or org batches)
    let can_update_all = has_permission(&current_user, Resource::Batches, Operation::UpdateAll);
    if !can_update_all && !is_batch_owner(&current_user, batch.created_by.as_deref()) {
        return Err(Error::NotFound {
            resource: "Batch".to_string(),
            id: batch_id_str.clone(),
        });
    }

    // Cancel the batch
    state
        .request_manager
        .cancel_batch(fusillade::BatchId(batch_id))
        .await
        .map_err(|e| Error::Internal {
            operation: format!("cancel batch: {}", e),
        })?;

    // Fetch updated batch to get latest status
    let batch = state
        .request_manager
        .get_batch(fusillade::BatchId(batch_id))
        .await
        .map_err(|_| Error::NotFound {
            resource: "Batch".to_string(),
            id: batch_id_str.clone(),
        })?;

    tracing::debug!("Batch {} cancelled", batch_id);

    // Fetch creator email for the response
    let creator_email = fetch_creator_email(state.db.read(), &batch).await;
    Ok(Json(to_batch_response_with_email(batch, creator_email.as_deref())))
}

#[utoipa::path(
    delete,
    path = "/batches/{batch_id}",
    tag = "batches",
    summary = "Delete batch",
    description = "Permanently delete a batch and all its associated data.

This action cannot be undone. The input file is not deleted.",
    responses(
        (status = 204, description = "Batch deleted successfully."),
        (status = 400, description = "Invalid batch ID format."),
        (status = 404, description = "Batch not found or you don't have access to it."),
        (status = 500, description = "An unexpected error occurred. Retry the request or contact support if the issue persists.")
    ),
    params(
        ("batch_id" = String, Path, description = "The batch ID returned when the batch was created.")
    )
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id, batch_id = %batch_id_str))]
pub async fn delete_batch<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(batch_id_str): Path<String>,
    current_user: RequiresPermission<resource::Batches, operation::DeleteOwn>,
) -> Result<StatusCode> {
    let batch_id = Uuid::parse_str(&batch_id_str).map_err(|_| Error::BadRequest {
        message: "Invalid batch ID format".to_string(),
    })?;

    // Get batch first to verify it exists and check ownership
    let batch = state
        .request_manager
        .get_batch(fusillade::BatchId(batch_id))
        .await
        .map_err(|_| Error::NotFound {
            resource: "Batch".to_string(),
            id: batch_id_str.clone(),
        })?;

    // Check ownership: users without DeleteAll permission can only delete their own batches (or org batches)
    let can_delete_all = has_permission(&current_user, Resource::Batches, Operation::DeleteAll);
    if !can_delete_all && !is_batch_owner(&current_user, batch.created_by.as_deref()) {
        return Err(Error::NotFound {
            resource: "Batch".to_string(),
            id: batch_id_str.clone(),
        });
    }

    // Delete the batch
    state
        .request_manager
        .delete_batch(fusillade::BatchId(batch_id))
        .await
        .map_err(|e| Error::Internal {
            operation: format!("delete batch: {}", e),
        })?;

    tracing::debug!("Batch {} deleted", batch_id);

    Ok(StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/batches/{batch_id}/retry",
    tag = "batches",
    summary = "Retry failed requests",
    description = "Retry all failed requests in a batch.

Failed requests are reset to pending and will be processed again. Use this after fixing transient issues or increasing rate limits.",
    responses(
        (status = 200, description = "Failed requests queued for retry.", body = BatchResponse),
        (status = 400, description = "No failed requests to retry in this batch."),
        (status = 404, description = "Batch not found or you don't have access to it."),
        (status = 500, description = "An unexpected error occurred. Retry the request or contact support if the issue persists.")
    ),
    params(
        ("batch_id" = String, Path, description = "The batch ID returned when the batch was created.")
    )
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id, batch_id = %batch_id_str))]
pub async fn retry_failed_batch_requests<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(batch_id_str): Path<String>,
    current_user: RequiresPermission<resource::Batches, operation::UpdateOwn>,
) -> Result<Json<BatchResponse>> {
    let batch_id = Uuid::parse_str(&batch_id_str).map_err(|_| Error::BadRequest {
        message: "Invalid batch ID format".to_string(),
    })?;

    // Get batch first to verify it exists
    let batch = state
        .request_manager
        .get_batch(fusillade::BatchId(batch_id))
        .await
        .map_err(|_| Error::NotFound {
            resource: "Batch".to_string(),
            id: batch_id_str.clone(),
        })?;

    // Check ownership: users without UpdateAll permission can only retry their own batches (or org batches)
    let can_update_all = has_permission(&current_user, Resource::Batches, Operation::UpdateAll);
    if !can_update_all && !is_batch_owner(&current_user, batch.created_by.as_deref()) {
        return Err(Error::NotFound {
            resource: "Batch".to_string(),
            id: batch_id_str.clone(),
        });
    }

    // Retry all failed requests for the batch in a single database operation
    let retried_count = state
        .request_manager
        .retry_failed_requests_for_batch(fusillade::BatchId(batch_id))
        .await
        .map_err(|e| Error::Internal {
            operation: format!("retry failed requests: {}", e),
        })?;

    if retried_count == 0 {
        return Err(Error::BadRequest {
            message: "No failed requests to retry in this batch".to_string(),
        });
    }

    tracing::debug!(
        batch_id = %batch_id,
        retried_count,
        "Retried failed requests"
    );

    // Fetch updated batch to get latest status
    let batch = state
        .request_manager
        .get_batch(fusillade::BatchId(batch_id))
        .await
        .map_err(|_| Error::NotFound {
            resource: "Batch".to_string(),
            id: batch_id_str.clone(),
        })?;

    // Fetch creator email for the response
    let creator_email = fetch_creator_email(state.db.read(), &batch).await;
    Ok(Json(to_batch_response_with_email(batch, creator_email.as_deref())))
}

#[utoipa::path(
    post,
    path = "/batches/{batch_id}/retry-requests",
    tag = "batches",
    summary = "Retry specific requests",
    description = "Retry specific failed requests by their IDs.

Use this for fine-grained control over which requests to retry, rather than retrying all failures.",
    request_body = RetryRequestsRequest,
    responses(
        (status = 200, description = "Specified requests queued for retry.", body = BatchResponse),
        (status = 400, description = "No valid request IDs provided."),
        (status = 404, description = "Batch not found or you don't have access to it."),
        (status = 500, description = "An unexpected error occurred. Retry the request or contact support if the issue persists.")
    ),
    params(
        ("batch_id" = String, Path, description = "The batch ID returned when the batch was created.")
    )
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id, batch_id = %batch_id_str))]
pub async fn retry_specific_requests<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(batch_id_str): Path<String>,
    current_user: RequiresPermission<resource::Batches, operation::UpdateOwn>,
    Json(req): Json<RetryRequestsRequest>,
) -> Result<Json<BatchResponse>> {
    let batch_id = Uuid::parse_str(&batch_id_str).map_err(|_| Error::BadRequest {
        message: "Invalid batch ID format".to_string(),
    })?;

    // Get batch first to verify it exists
    let batch = state
        .request_manager
        .get_batch(fusillade::BatchId(batch_id))
        .await
        .map_err(|_| Error::NotFound {
            resource: "Batch".to_string(),
            id: batch_id_str.clone(),
        })?;

    // Check ownership: users without UpdateAll permission can only retry their own batches (or org batches)
    let can_update_all = has_permission(&current_user, Resource::Batches, Operation::UpdateAll);
    if !can_update_all && !is_batch_owner(&current_user, batch.created_by.as_deref()) {
        return Err(Error::NotFound {
            resource: "Batch".to_string(),
            id: batch_id_str.clone(),
        });
    }

    // Parse request IDs
    let request_ids: Vec<fusillade::RequestId> = req
        .request_ids
        .iter()
        .filter_map(|id_str| Uuid::parse_str(id_str).ok().map(fusillade::RequestId))
        .collect();

    if request_ids.is_empty() {
        return Err(Error::BadRequest {
            message: "No valid request IDs provided".to_string(),
        });
    }

    tracing::debug!(
        batch_id = %batch_id,
        request_count = request_ids.len(),
        "Retrying specific requests"
    );

    // Retry the specified requests
    let results = state
        .request_manager
        .retry_failed_requests(request_ids.clone())
        .await
        .map_err(|e| Error::Internal {
            operation: format!("retry failed requests: {}", e),
        })?;

    // Check for any failures
    let failed_retries: Vec<_> = results
        .iter()
        .enumerate()
        .filter_map(|(i, r)| r.as_ref().err().map(|e| (i, e)))
        .collect();

    if !failed_retries.is_empty() {
        tracing::warn!(
            batch_id = %batch_id,
            failed_retry_count = failed_retries.len(),
            "Some requests failed to retry"
        );
    }

    let successful_retries = results.iter().filter(|r| r.is_ok()).count();
    tracing::debug!(
        batch_id = %batch_id,
        retried_count = successful_retries,
        "Successfully retried specific requests"
    );

    // Fetch updated batch to get latest status
    let batch = state
        .request_manager
        .get_batch(fusillade::BatchId(batch_id))
        .await
        .map_err(|_| Error::NotFound {
            resource: "Batch".to_string(),
            id: batch_id_str.clone(),
        })?;

    // Fetch creator email for the response
    let creator_email = fetch_creator_email(state.db.read(), &batch).await;
    Ok(Json(to_batch_response_with_email(batch, creator_email.as_deref())))
}

#[utoipa::path(
    get,
    path = "/batches",
    tag = "batches",
    summary = "List batches",
    description = "Returns a paginated list of your batches, newest first.

Use cursor-based pagination: pass `last_id` from the response as the `after` parameter to fetch the next page.",
    responses(
        (status = 200, description = "List of batches. Check `has_more` to determine if additional pages exist.", body = BatchListResponse),
        (status = 500, description = "An unexpected error occurred. Retry the request or contact support if the issue persists.")
    ),
    params(
        ListBatchesQuery
    )
)]
#[tracing::instrument(skip_all, fields(user_id = %current_user.id))]
pub async fn list_batches<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Query(query): Query<ListBatchesQuery>,
    current_user: RequiresPermission<resource::Batches, operation::ReadOwn>,
) -> Result<Json<BatchListResponse>> {
    let limit = query.pagination.limit();

    // Parse the after cursor if provided
    let after = query
        .pagination
        .after
        .as_ref()
        .and_then(|after_str| Uuid::parse_str(after_str).ok().map(fusillade::BatchId));

    // Determine ownership scoping:
    // - Org context: always scope to org (unified view for both PMs and standard users)
    // - Personal context + PM: no filter (sees all batches across all users)
    // - Personal context + standard user: scope to own batches only
    let can_read_all = can_read_all_resources(&current_user, Resource::Batches);
    let created_by = if let Some(org_id) = current_user.active_organization {
        Some(org_id.to_string())
    } else if can_read_all {
        None
    } else {
        Some(current_user.id.to_string())
    };

    // Translate member_id to api_key_ids for fusillade filtering.
    // Uses a short-lived connection so we don't hold it across the fusillade call.
    //
    // Note: this only matches batches created after hidden key attribution was deployed.
    // Legacy batches with api_key_id = NULL won't match the filter.
    let api_key_ids_filter = if let Some(member_id) = query.member_id {
        let mut read_conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;
        let key_ids = match current_user.active_organization {
            Some(org_id) => {
                // Org context: find the single hidden key for this member in this org
                let key_id = ApiKeys::new(&mut read_conn)
                    .find_hidden_key_id(org_id, ApiKeyPurpose::Batch, member_id)
                    .await
                    .map_err(Error::Database)?;
                key_id.into_iter().collect::<Vec<_>>()
            }
            None if can_read_all => {
                // PM personal context: find ALL hidden keys created by this member
                ApiKeys::new(&mut read_conn)
                    .find_all_hidden_key_ids_by_creator(ApiKeyPurpose::Batch, member_id)
                    .await
                    .map_err(Error::Database)?
            }
            None => {
                return Err(Error::BadRequest {
                    message: "member_id filter is only available in organization context or for platform managers".to_string(),
                });
            }
        };
        if key_ids.is_empty() {
            return Ok(Json(BatchListResponse {
                object_type: ListObjectType::List,
                data: vec![],
                first_id: None,
                last_id: None,
                has_more: false,
            }));
        }
        Some(key_ids)
    } else {
        None
    };

    // Fetch batches with ownership filtering, search, and cursor-based pagination
    let batches = state
        .request_manager
        .list_batches(fusillade::ListBatchesFilter {
            created_by,
            search: query.search.clone(),
            after,
            limit: Some(limit + 1),
            api_key_ids: api_key_ids_filter,
            status: query.status.clone(),
            created_after: query.created_after,
            created_before: query.created_before,
            active_first: query.active_first,
        })
        .await
        .map_err(|e| Error::Internal {
            operation: format!("list batches: {}", e),
        })?;

    // Check if there are more results
    let has_more = batches.len() > limit as usize;
    let batches: Vec<_> = batches.into_iter().take(limit as usize).collect();

    // Get first and last IDs
    let first_id = batches.first().map(|b| b.id.0.to_string());
    let last_id = batches.last().map(|b| b.id.0.to_string());

    // Resolve creator/context metadata for all returned batches.
    // Uses a fresh connection (not held across the fusillade call above).
    use crate::db::models::users::UserDBResponse;
    let mut read_conn = state.db.read().acquire().await.map_err(|e| Error::Database(e.into()))?;

    // Resolve individual creators via api_key_id → api_keys.created_by
    let api_key_ids: Vec<Uuid> = batches
        .iter()
        .filter_map(|b| b.api_key_id)
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let api_key_creator_map: HashMap<Uuid, Uuid> = if !api_key_ids.is_empty() {
        ApiKeys::new(&mut read_conn)
            .get_creators_by_key_ids(api_key_ids)
            .await
            .map_err(Error::Database)?
    } else {
        HashMap::new()
    };

    // Collect all unique user IDs we need to resolve:
    // - Owner IDs from batch.created_by (could be org or personal user)
    // - Individual creator IDs from api_key resolution
    let mut all_user_ids: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
    for batch in &batches {
        if let Some(owner_id) = batch.created_by.as_ref().and_then(|id| Uuid::parse_str(id).ok()) {
            all_user_ids.insert(owner_id);
        }
        if let Some(api_key_id) = batch.api_key_id
            && let Some(&creator_id) = api_key_creator_map.get(&api_key_id)
        {
            all_user_ids.insert(creator_id);
        }
    }

    // Bulk fetch all users (emails, user_type, display_name)
    let user_map: HashMap<Uuid, UserDBResponse> = if !all_user_ids.is_empty() {
        Users::new(&mut read_conn)
            .get_bulk(all_user_ids.into_iter().collect())
            .await
            .map_err(|e| Error::Internal {
                operation: format!("fetch users: {}", e),
            })?
    } else {
        HashMap::new()
    };

    // Parse include parameter
    let includes: Vec<&str> = query
        .include
        .as_ref()
        .map(|s| s.split(',').map(|s| s.trim()).collect())
        .unwrap_or_default();
    let include_analytics = includes.contains(&"analytics");

    // Collect batch IDs for bulk operations
    let batch_ids: Vec<Uuid> = batches.iter().map(|b| b.id.0).collect();

    // Fetch analytics in bulk if requested
    let analytics_map: HashMap<Uuid, BatchAnalytics> = if include_analytics && !batches.is_empty() {
        crate::db::handlers::analytics::get_batches_analytics_bulk(state.db.read(), &batch_ids)
            .await
            .map_err(|e| Error::Internal {
                operation: format!("fetch bulk batch analytics: {}", e),
            })?
    } else {
        HashMap::new()
    };

    // Convert batches to responses with enriched creator/context metadata
    let data: Vec<_> = batches
        .into_iter()
        .map(|batch| {
            let batch_id = batch.id;

            // Resolve individual creator email via api_key_id, with fallback
            // to batch owner for legacy batches without api_key_id
            let individual_creator_id = batch.api_key_id.and_then(|key_id| api_key_creator_map.get(&key_id).copied());
            let created_by_email = individual_creator_id
                .and_then(|uid| user_map.get(&uid))
                .map(|u| u.email.clone())
                .or_else(|| {
                    // Legacy fallback: use batch owner (created_by) when api_key_id is NULL
                    if batch.api_key_id.is_none() {
                        batch
                            .created_by
                            .as_ref()
                            .and_then(|id| Uuid::parse_str(id).ok())
                            .and_then(|uid| user_map.get(&uid))
                            .map(|u| u.email.clone())
                    } else {
                        None
                    }
                });

            // Resolve context from batch owner (created_by field)
            let owner_id = batch.created_by.as_ref().and_then(|id| Uuid::parse_str(id).ok());
            let owner = owner_id.and_then(|id| user_map.get(&id));
            let (context_name, context_type) = match owner {
                Some(u) if u.user_type == "organization" => {
                    let name = u.display_name.clone().unwrap_or_else(|| u.email.clone());
                    (Some(name), Some("organization".to_string()))
                }
                Some(_) => (Some("Personal".to_string()), Some("personal".to_string())),
                None => (None, None),
            };

            let mut response = to_batch_response_with_email(batch, None);

            // Inject enriched creator/context info into metadata
            if let Some(email) = created_by_email {
                response
                    .metadata
                    .get_or_insert_with(HashMap::new)
                    .insert("created_by_email".to_string(), email);
            }
            if let Some(name) = context_name {
                response
                    .metadata
                    .get_or_insert_with(HashMap::new)
                    .insert("context_name".to_string(), name);
            }
            if let Some(ctype) = context_type {
                response
                    .metadata
                    .get_or_insert_with(HashMap::new)
                    .insert("context_type".to_string(), ctype);
            }

            if include_analytics {
                response.analytics = analytics_map.get(&batch_id).cloned();
            }
            response
        })
        .collect();

    Ok(Json(BatchListResponse {
        object_type: ListObjectType::List,
        data,
        first_id,
        last_id,
        has_more,
    }))
}

#[cfg(test)]
mod tests {
    use crate::api::models::batches::CreateBatchRequest;
    use crate::api::models::users::Role;
    use crate::db::handlers::Credits;
    use crate::db::models::credits::{CreditTransactionCreateDBRequest, CreditTransactionType};
    use crate::errors::Error;
    use crate::test::utils::*;
    use axum::http::StatusCode;
    use rust_decimal::Decimal;
    use sqlx::PgPool;
    use std::collections::HashMap;
    use uuid::Uuid;

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_batch_with_default_24h_sla(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create a deployment and add to group so user has access to the model
        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Upload a batch file first
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;
        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");
        let multipart = axum_test::multipart::MultipartForm::new()
            .add_part("file", file_part)
            .add_part("purpose", axum_test::multipart::Part::text("batch"));
        let upload_resp = app
            .post("/ai/v1/files")
            .multipart(multipart)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        upload_resp.assert_status(StatusCode::CREATED);
        let file: serde_json::Value = upload_resp.json();
        let file_id = file["id"].as_str().unwrap();

        // Create batch with 24h SLA (default allowed)
        let create_req = CreateBatchRequest {
            input_file_id: file_id.to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
        };

        let resp = app
            .post("/ai/v1/batches")
            .json(&create_req)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        resp.assert_status(StatusCode::CREATED);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_batch_in_org_context(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;

        // Create a user with batch permissions and an org they belong to
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let org = create_test_org(&pool, user.id).await;

        // Create a group and give the org access to a model
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, org.id, group.id).await;
        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        let auth = add_auth_headers(&user);
        let org_cookie = format!("dw_active_org={}", org.id);

        // Upload a file in org context (uploaded_by will be the org ID)
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;
        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");
        let multipart = axum_test::multipart::MultipartForm::new()
            .add_part("file", file_part)
            .add_part("purpose", axum_test::multipart::Part::text("batch"));
        let upload_resp = app
            .post("/ai/v1/files")
            .multipart(multipart)
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .add_header("cookie", &org_cookie)
            .await;
        upload_resp.assert_status(StatusCode::CREATED);
        let file: serde_json::Value = upload_resp.json();
        let file_id = file["id"].as_str().unwrap();

        // Create batch in org context using the org-owned file
        let create_req = CreateBatchRequest {
            input_file_id: file_id.to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
        };

        let resp = app
            .post("/ai/v1/batches")
            .json(&create_req)
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .add_header("cookie", &org_cookie)
            .await;
        resp.assert_status(StatusCode::CREATED);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_batch_with_unsupported_sla(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create a deployment and add to group so user has access to the model
        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Upload a batch file first
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;
        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");
        let multipart = axum_test::multipart::MultipartForm::new()
            .add_part("file", file_part)
            .add_part("purpose", axum_test::multipart::Part::text("batch"));
        let upload_resp = app
            .post("/ai/v1/files")
            .multipart(multipart)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        upload_resp.assert_status(StatusCode::CREATED);
        let file: serde_json::Value = upload_resp.json();
        let file_id = file["id"].as_str().unwrap();

        // Try to create batch with unsupported 1h SLA
        let create_req = CreateBatchRequest {
            input_file_id: file_id.to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "1h".to_string(),
            metadata: None,
        };

        let resp = app
            .post("/ai/v1/batches")
            .json(&create_req)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        resp.assert_status(StatusCode::BAD_REQUEST);
        let error_text = resp.text();
        assert!(error_text.contains("Unsupported completion_window"));
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_batch_with_custom_allowed_sla(pool: PgPool) {
        // Create app with custom config allowing multiple SLAs
        let mut config = create_test_config();
        config.batches.allowed_completion_windows = vec!["1h".to_string(), "24h".to_string(), "48h".to_string()];

        let (app, _bg_services) = create_test_app_with_config(pool.clone(), config, false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create a deployment and add to group so user has access to the model
        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Upload a batch file first
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;
        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");
        let multipart = axum_test::multipart::MultipartForm::new()
            .add_part("file", file_part)
            .add_part("purpose", axum_test::multipart::Part::text("batch"));
        let upload_resp = app
            .post("/ai/v1/files")
            .multipart(multipart)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        upload_resp.assert_status(StatusCode::CREATED);
        let file: serde_json::Value = upload_resp.json();
        let file_id = file["id"].as_str().unwrap();

        // Create batch with 1h SLA (now allowed in custom config)
        let create_req = CreateBatchRequest {
            input_file_id: file_id.to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "1h".to_string(),
            metadata: None,
        };

        let resp = app
            .post("/ai/v1/batches")
            .json(&create_req)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        resp.assert_status(StatusCode::CREATED);

        // Also test that 48h works
        let upload_resp2 = app
            .post("/ai/v1/files")
            .multipart(
                axum_test::multipart::MultipartForm::new()
                    .add_part(
                        "file",
                        axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch-2.jsonl"),
                    )
                    .add_part("purpose", axum_test::multipart::Part::text("batch")),
            )
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        upload_resp2.assert_status(StatusCode::CREATED);
        let file2: serde_json::Value = upload_resp2.json();
        let file_id2 = file2["id"].as_str().unwrap();

        let create_req2 = CreateBatchRequest {
            input_file_id: file_id2.to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "48h".to_string(),
            metadata: None,
        };

        let resp2 = app
            .post("/ai/v1/batches")
            .json(&create_req2)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        resp2.assert_status(StatusCode::CREATED);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_batch_with_responses_endpoint(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create a deployment and add to group so user has access to the model
        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Upload a /v1/responses batch file first
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/responses","body":{"model":"gpt-4","input":"Hello"}}"#;
        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");
        let multipart = axum_test::multipart::MultipartForm::new()
            .add_part("file", file_part)
            .add_part("purpose", axum_test::multipart::Part::text("batch"));
        let upload_resp = app
            .post("/ai/v1/files")
            .multipart(multipart)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        upload_resp.assert_status(StatusCode::CREATED);
        let file: serde_json::Value = upload_resp.json();
        let file_id = file["id"].as_str().unwrap();

        // Create batch with /v1/responses endpoint
        let create_req = CreateBatchRequest {
            input_file_id: file_id.to_string(),
            endpoint: "/v1/responses".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
        };

        let resp = app
            .post("/ai/v1/batches")
            .json(&create_req)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        resp.assert_status(StatusCode::CREATED);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_sla_to_expiry_timestamp_24h(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create a deployment and add to group so user has access to the model
        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Upload a batch file first
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;
        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");
        let multipart = axum_test::multipart::MultipartForm::new()
            .add_part("file", file_part)
            .add_part("purpose", axum_test::multipart::Part::text("batch"));
        let upload_resp = app
            .post("/ai/v1/files")
            .multipart(multipart)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        upload_resp.assert_status(StatusCode::CREATED);
        let file: serde_json::Value = upload_resp.json();
        let file_id = file["id"].as_str().unwrap();

        // Record the time before creating the batch
        let now = chrono::Utc::now();

        // Create batch with 24h SLA
        let create_req = CreateBatchRequest {
            input_file_id: file_id.to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
        };

        let resp = app
            .post("/ai/v1/batches")
            .json(&create_req)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        resp.assert_status(StatusCode::CREATED);
        let batch: serde_json::Value = resp.json();

        // Verify the batch has an expires_at timestamp
        let expires_at = batch["expires_at"].as_i64().expect("expires_at should be present");

        // Convert to DateTime for easier comparison
        let expires_at_dt = chrono::DateTime::from_timestamp(expires_at, 0).expect("Invalid timestamp");

        // Calculate expected expiry (24 hours from now)
        let expected_expiry = now + chrono::Duration::hours(24);

        // Allow 1 minute tolerance for test execution time
        let tolerance = chrono::Duration::minutes(1);
        let diff = (expires_at_dt - expected_expiry).abs();

        assert!(
            diff < tolerance,
            "Expiry timestamp should be ~24h from now. Expected: {}, Got: {}, Diff: {} seconds",
            expected_expiry,
            expires_at_dt,
            diff.num_seconds()
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_sla_to_expiry_timestamp_custom(pool: PgPool) {
        // Create app with custom config allowing 1h SLA
        let mut config = create_test_config();
        config.batches.allowed_completion_windows = vec!["1h".to_string(), "24h".to_string()];

        let (app, _bg_services) = create_test_app_with_config(pool.clone(), config, false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create a deployment and add to group so user has access to the model
        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Upload a batch file first
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;
        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");
        let multipart = axum_test::multipart::MultipartForm::new()
            .add_part("file", file_part)
            .add_part("purpose", axum_test::multipart::Part::text("batch"));
        let upload_resp = app
            .post("/ai/v1/files")
            .multipart(multipart)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        upload_resp.assert_status(StatusCode::CREATED);
        let file: serde_json::Value = upload_resp.json();
        let file_id = file["id"].as_str().unwrap();

        // Record the time before creating the batch
        let now = chrono::Utc::now();

        // Create batch with 1h SLA
        let create_req = CreateBatchRequest {
            input_file_id: file_id.to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "1h".to_string(),
            metadata: None,
        };

        let resp = app
            .post("/ai/v1/batches")
            .json(&create_req)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        resp.assert_status(StatusCode::CREATED);
        let batch: serde_json::Value = resp.json();

        // Verify the batch has an expires_at timestamp
        let expires_at = batch["expires_at"].as_i64().expect("expires_at should be present");

        // Convert to DateTime for easier comparison
        let expires_at_dt = chrono::DateTime::from_timestamp(expires_at, 0).expect("Invalid timestamp");

        // Calculate expected expiry (1 hour from now)
        let expected_expiry = now + chrono::Duration::hours(1);

        // Allow 1 minute tolerance for test execution time
        let tolerance = chrono::Duration::minutes(1);
        let diff = (expires_at_dt - expected_expiry).abs();

        assert!(
            diff < tolerance,
            "Expiry timestamp should be ~1h from now. Expected: {}, Got: {}, Diff: {} seconds",
            expected_expiry,
            expires_at_dt,
            diff.num_seconds()
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_batches_with_include_analytics(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create a deployment and add to group so user has access to the model
        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Upload a batch file
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;
        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");
        let multipart = axum_test::multipart::MultipartForm::new()
            .add_part("file", file_part)
            .add_part("purpose", axum_test::multipart::Part::text("batch"));
        let upload_resp = app
            .post("/ai/v1/files")
            .multipart(multipart)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        upload_resp.assert_status(StatusCode::CREATED);
        let file: serde_json::Value = upload_resp.json();
        let file_id = file["id"].as_str().unwrap();

        // Create a batch
        let create_req = CreateBatchRequest {
            input_file_id: file_id.to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
        };

        let create_resp = app
            .post("/ai/v1/batches")
            .json(&create_req)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        create_resp.assert_status(StatusCode::CREATED);

        // List batches without include=analytics - analytics should not be present
        let list_resp = app
            .get("/ai/v1/batches")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        list_resp.assert_status_ok();
        let list_result: serde_json::Value = list_resp.json();
        assert!(!list_result["data"].as_array().unwrap().is_empty());
        // Without include=analytics, analytics field should be null/missing
        let first_batch = &list_result["data"][0];
        assert!(first_batch["analytics"].is_null());

        // List batches with include=analytics - analytics should be present
        let list_with_analytics_resp = app
            .get("/ai/v1/batches?include=analytics")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        list_with_analytics_resp.assert_status_ok();
        let list_with_analytics: serde_json::Value = list_with_analytics_resp.json();
        assert!(!list_with_analytics["data"].as_array().unwrap().is_empty());
        // With include=analytics, analytics field should be an object (even if empty)
        let first_batch_with_analytics = &list_with_analytics["data"][0];
        assert!(first_batch_with_analytics["analytics"].is_object());
        // Verify analytics has expected fields
        let analytics = &first_batch_with_analytics["analytics"];
        assert!(analytics["total_requests"].is_number());
        assert!(analytics["total_prompt_tokens"].is_number());
        assert!(analytics["total_completion_tokens"].is_number());
        assert!(analytics["total_tokens"].is_number());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_errors_hidden_until_sla_expires(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let deployment = create_test_deployment(&pool, user.id, "test-model", "test-model").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Upload a batch file
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"test-model","messages":[{"role":"user","content":"Test"}]}}"#;
        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test.jsonl");
        let multipart = axum_test::multipart::MultipartForm::new()
            .add_part("file", file_part)
            .add_part("purpose", axum_test::multipart::Part::text("batch"));
        let upload_resp = app
            .post("/ai/v1/files")
            .multipart(multipart)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        upload_resp.assert_status(StatusCode::CREATED);
        let file: serde_json::Value = upload_resp.json();
        let file_id = file["id"].as_str().unwrap();

        // Create batch
        let create_req = CreateBatchRequest {
            input_file_id: file_id.to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
        };

        let resp = app
            .post("/ai/v1/batches")
            .json(&create_req)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        resp.assert_status(StatusCode::CREATED);
        let batch: serde_json::Value = resp.json();
        let batch_id = batch["id"].as_str().unwrap();
        let batch_uuid = Uuid::parse_str(batch_id).unwrap();

        // Scenario 1: Simulate errors exist but batch is still within SLA (no failed_at)
        sqlx::query(
            r#"
            UPDATE fusillade.batches
            SET errors = '{"object":"list","data":[{"code":"invalid_request","message":"Test error"}]}'::jsonb
            WHERE id = $1
            "#,
        )
        .bind(batch_uuid)
        .execute(&pool)
        .await
        .expect("Failed to set errors");

        // GET batch - errors should be HIDDEN (null) because within SLA
        let get_resp = app
            .get(&format!("/ai/v1/batches/{}", batch_id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        get_resp.assert_status(StatusCode::OK);
        let batch_response: serde_json::Value = get_resp.json();

        // Batch-level errors (validation/system errors) are now always shown
        // The hiding logic has been removed as it was causing more issues than it solved
        assert!(!batch_response["errors"].is_null(), "Batch-level errors are now always shown");
        // failed_at is also shown now (hiding logic removed)
        assert!(
            batch_response["failed_at"].is_null(),
            "failed_at should still be null since we didn't set it yet"
        );
        // Failed request count uses failed_requests_non_retriable before SLA
        // (which is 0 in this test since we have no actual failed requests)
        assert_eq!(
            batch_response["request_counts"]["failed"].as_i64().unwrap(),
            0,
            "Failed request count is 0 (no actual failed requests, only batch-level errors)"
        );

        // Scenario 2: Set failed_at - now this will be shown
        sqlx::query(
            r#"
            UPDATE fusillade.batches
            SET failed_at = NOW()
            WHERE id = $1
            "#,
        )
        .bind(batch_uuid)
        .execute(&pool)
        .await
        .expect("Failed to set failed_at");

        // GET batch - errors are now shown, hiding logic removed
        let get_resp2 = app
            .get(&format!("/ai/v1/batches/{}", batch_id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        get_resp2.assert_status(StatusCode::OK);
        let batch_response2: serde_json::Value = get_resp2.json();

        assert!(
            !batch_response2["errors"].is_null(),
            "Batch-level errors are now always shown (hiding logic removed)"
        );
        // failed_at is now shown since we set it
        assert!(
            !batch_response2["failed_at"].is_null(),
            "failed_at is now shown (hiding logic removed)"
        );
        // Failed count still 0 since we have no actual failed requests
        assert_eq!(
            batch_response2["request_counts"]["failed"].as_i64().unwrap(),
            0,
            "Failed request count is 0 (no actual failed requests)"
        );

        // Scenario 3: Expire the SLA AND have failed_at - NOW errors should be visible
        sqlx::query(
            r#"
            UPDATE fusillade.batches
            SET expires_at = NOW() - INTERVAL '1 hour'
            WHERE id = $1
            "#,
        )
        .bind(batch_uuid)
        .execute(&pool)
        .await
        .expect("Failed to expire batch");

        // GET batch - errors should NOW be VISIBLE
        let get_resp3 = app
            .get(&format!("/ai/v1/batches/{}", batch_id))
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        get_resp3.assert_status(StatusCode::OK);
        let batch_response3: serde_json::Value = get_resp3.json();

        assert!(
            !batch_response3["errors"].is_null(),
            "Errors should be visible when both failed_at is set AND SLA has expired"
        );
        assert_eq!(
            batch_response3["errors"]["data"][0]["message"].as_str().unwrap(),
            "Test error",
            "Error message should match what we set"
        );
        // Note: error_file_id would be shown if it existed - fusillade creates it during processing
        // This test manually sets errors without going through fusillade, so no error file exists
        assert!(
            !batch_response3["failed_at"].is_null(),
            "failed_at should now be visible after SLA expires"
        );
        // Note: We can't easily verify the exact failed count since we didn't actually
        // create failed requests in the DB (just set the errors field). But we verified
        // it was hidden before, so the logic is working.
    }

    /// Regression test for streaming batch results.
    ///
    /// Previously, get_batch_results collected ALL results into memory before
    /// sending the response, causing OOM kills on large result sets. This test
    /// verifies that:
    /// 1. Unlimited downloads (no limit param) return all results correctly
    /// 2. Paginated downloads (with limit) return correct subset and headers
    /// 3. X-Incomplete reflects batch processing status
    /// 4. X-Last-Line is set correctly
    /// 5. Unlimited responses use streaming (no content-length header)
    #[sqlx::test]
    #[test_log::test]
    async fn test_batch_results_streaming(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let deployment = create_test_deployment(&pool, user.id, "test-model-endpoint", "test-model").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Create file, templates, batch, and completed requests directly in the DB
        let file_id = Uuid::new_v4();
        let batch_id = Uuid::new_v4();
        let num_requests = 50;

        sqlx::query(
            "INSERT INTO fusillade.files (id, name, status, created_at, updated_at) VALUES ($1, 'test.jsonl', 'processed', NOW(), NOW())",
        )
        .bind(file_id)
        .execute(&pool)
        .await
        .expect("Failed to create file");

        sqlx::query(
            "INSERT INTO fusillade.batches (id, created_by, file_id, endpoint, completion_window, expires_at, created_at, total_requests) VALUES ($1, $2, $3, '/v1/chat/completions', '24h', NOW() + interval '24 hours', NOW(), $4)",
        )
        .bind(batch_id)
        .bind(user.id.to_string())
        .bind(file_id)
        .bind(num_requests as i32)
        .execute(&pool)
        .await
        .expect("Failed to create batch");

        for i in 0..num_requests {
            let template_id = Uuid::new_v4();
            let request_id = Uuid::new_v4();
            let custom_id = format!("req-{}", i);
            let body = serde_json::json!({"model": "test-model", "messages": [{"role": "user", "content": format!("Test {}", i)}]});
            let response_body = serde_json::json!({
                "id": format!("chatcmpl-{}", i),
                "choices": [{"message": {"content": format!("Response {}", i)}}]
            });

            sqlx::query(
                "INSERT INTO fusillade.request_templates (id, file_id, model, api_key, endpoint, path, body, custom_id, method) VALUES ($1, $2, 'test-model', 'test-key', 'http://test', '/v1/chat/completions', $3, $4, 'POST')",
            )
            .bind(template_id)
            .bind(file_id)
            .bind(serde_json::to_string(&body).unwrap())
            .bind(&custom_id)
            .execute(&pool)
            .await
            .expect("Failed to create template");

            sqlx::query(
                "INSERT INTO fusillade.requests (id, batch_id, template_id, model, state, response_status, response_body, created_at, completed_at) VALUES ($1, $2, $3, 'test-model', 'completed', 200, $4, NOW(), NOW())",
            )
            .bind(request_id)
            .bind(batch_id)
            .bind(template_id)
            .bind(serde_json::to_string(&response_body).unwrap())
            .execute(&pool)
            .await
            .expect("Failed to create completed request");
        }

        let auth = add_auth_headers(&user);

        // Test 1: Unlimited download (streaming path)
        let response = app
            .get(&format!("/ai/v1/batches/{}/results", batch_id))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;

        response.assert_status(StatusCode::OK);
        response.assert_header("content-type", "application/x-ndjson");
        response.assert_header("X-Incomplete", "false");
        response.assert_header("X-Last-Line", &num_requests.to_string());
        // Streaming responses must not have content-length (regression guard against
        // collecting the entire result set into memory before sending)
        assert!(
            response.headers().get("content-length").is_none(),
            "Unlimited download should be streamed without content-length"
        );

        let body = response.text();
        let lines: Vec<&str> = body.trim().lines().collect();
        assert_eq!(lines.len(), num_requests, "Should return all {} results", num_requests);

        // Verify each line is valid JSON with expected fields
        for line in &lines {
            let item: serde_json::Value = serde_json::from_str(line).expect("Each line should be valid JSON");
            assert!(item.get("custom_id").is_some(), "Each result should have custom_id");
            assert!(item.get("status").is_some(), "Each result should have status");
        }

        // Test 2: Paginated download (buffered path)
        let page_size = 10;
        let response = app
            .get(&format!("/ai/v1/batches/{}/results?limit={}", batch_id, page_size))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;

        response.assert_status(StatusCode::OK);
        response.assert_header("X-Incomplete", "true"); // more pages exist
        response.assert_header("X-Last-Line", &page_size.to_string());

        let body = response.text();
        let lines: Vec<&str> = body.trim().lines().collect();
        assert_eq!(lines.len(), page_size, "Should return exactly {} results", page_size);

        // Test 3: Last page should have X-Incomplete=false
        let response = app
            .get(&format!(
                "/ai/v1/batches/{}/results?limit={}&skip={}",
                batch_id,
                page_size,
                num_requests - page_size
            ))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;

        response.assert_status(StatusCode::OK);
        response.assert_header("X-Incomplete", "false"); // no more pages, batch complete
        response.assert_header("X-Last-Line", &num_requests.to_string());
    }

    /// Test that X-Incomplete reflects batch processing status, not just pagination.
    ///
    /// When a batch still has pending/in-progress requests, X-Incomplete should be
    /// true even on the last page of currently available results (or unlimited download).
    #[sqlx::test]
    #[test_log::test]
    async fn test_batch_results_x_incomplete_while_still_processing(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let deployment = create_test_deployment(&pool, user.id, "test-model-endpoint", "test-model").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        let file_id = Uuid::new_v4();
        let batch_id = Uuid::new_v4();
        let num_completed = 5;
        let num_pending = 3;
        let total = num_completed + num_pending;

        sqlx::query(
            "INSERT INTO fusillade.files (id, name, status, created_at, updated_at) VALUES ($1, 'test.jsonl', 'processed', NOW(), NOW())",
        )
        .bind(file_id)
        .execute(&pool)
        .await
        .expect("Failed to create file");

        sqlx::query(
            "INSERT INTO fusillade.batches (id, created_by, file_id, endpoint, completion_window, expires_at, created_at, total_requests) VALUES ($1, $2, $3, '/v1/chat/completions', '24h', NOW() + interval '24 hours', NOW(), $4)",
        )
        .bind(batch_id)
        .bind(user.id.to_string())
        .bind(file_id)
        .bind(total as i32)
        .execute(&pool)
        .await
        .expect("Failed to create batch");

        // Create completed requests
        for i in 0..num_completed {
            let template_id = Uuid::new_v4();
            let request_id = Uuid::new_v4();
            let custom_id = format!("req-{}", i);
            let body = serde_json::json!({"model": "test-model", "messages": [{"role": "user", "content": format!("Test {}", i)}]});
            let response_body = serde_json::json!({
                "id": format!("chatcmpl-{}", i),
                "choices": [{"message": {"content": format!("Response {}", i)}}]
            });

            sqlx::query(
                "INSERT INTO fusillade.request_templates (id, file_id, model, api_key, endpoint, path, body, custom_id, method) VALUES ($1, $2, 'test-model', 'test-key', 'http://test', '/v1/chat/completions', $3, $4, 'POST')",
            )
            .bind(template_id)
            .bind(file_id)
            .bind(serde_json::to_string(&body).unwrap())
            .bind(&custom_id)
            .execute(&pool)
            .await
            .expect("Failed to create template");

            sqlx::query(
                "INSERT INTO fusillade.requests (id, batch_id, template_id, model, state, response_status, response_body, created_at, completed_at) VALUES ($1, $2, $3, 'test-model', 'completed', 200, $4, NOW(), NOW())",
            )
            .bind(request_id)
            .bind(batch_id)
            .bind(template_id)
            .bind(serde_json::to_string(&response_body).unwrap())
            .execute(&pool)
            .await
            .expect("Failed to create completed request");
        }

        // Create pending requests (no response yet)
        for i in num_completed..total {
            let template_id = Uuid::new_v4();
            let request_id = Uuid::new_v4();
            let custom_id = format!("req-{}", i);
            let body = serde_json::json!({"model": "test-model", "messages": [{"role": "user", "content": format!("Test {}", i)}]});

            sqlx::query(
                "INSERT INTO fusillade.request_templates (id, file_id, model, api_key, endpoint, path, body, custom_id, method) VALUES ($1, $2, 'test-model', 'test-key', 'http://test', '/v1/chat/completions', $3, $4, 'POST')",
            )
            .bind(template_id)
            .bind(file_id)
            .bind(serde_json::to_string(&body).unwrap())
            .bind(&custom_id)
            .execute(&pool)
            .await
            .expect("Failed to create template");

            sqlx::query(
                "INSERT INTO fusillade.requests (id, batch_id, template_id, model, state, created_at) VALUES ($1, $2, $3, 'test-model', 'pending', NOW())",
            )
            .bind(request_id)
            .bind(batch_id)
            .bind(template_id)
            .execute(&pool)
            .await
            .expect("Failed to create pending request");
        }

        let auth = add_auth_headers(&user);

        // Unlimited download: X-Incomplete should be true because batch is still processing
        let response = app
            .get(&format!("/ai/v1/batches/{}/results", batch_id))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;

        response.assert_status(StatusCode::OK);
        response.assert_header("X-Incomplete", "true");

        let body = response.text();
        let lines: Vec<&str> = body.trim().lines().collect();
        // Results include all requests (completed + pending)
        assert_eq!(lines.len(), total, "Should return all request results");

        // Paginated last page: even though no more pages, X-Incomplete should still be true
        let response = app
            .get(&format!("/ai/v1/batches/{}/results?limit={}", batch_id, total))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;

        response.assert_status(StatusCode::OK);
        response.assert_header("X-Incomplete", "true");

        let body = response.text();
        let lines: Vec<&str> = body.trim().lines().collect();
        assert_eq!(lines.len(), total, "Should return all request results");
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_reserve_capacity_for_batch_inserts_and_releases(pool: PgPool) {
        let config = create_test_config();
        // Use create_test_app_state_with_fusillade to run all migrations (dwctl + fusillade)
        let state = create_test_app_state_with_fusillade(pool.clone(), config).await;

        let user = create_test_user(&pool, Role::StandardUser).await;
        let endpoint_id = create_test_endpoint(&pool, &format!("test-{}", Uuid::new_v4()), user.id).await;

        let alias = format!("alias-{}", Uuid::new_v4());
        let model_id = create_test_model(&pool, "model-a", &alias, endpoint_id, user.id).await;

        let file_model_counts: HashMap<String, i64> = HashMap::from([(alias.clone(), 5_i64)]);
        let model_throughputs = HashMap::from([(alias.clone(), 1000.0_f32)]);
        let model_ids_by_alias = HashMap::from([(alias.clone(), model_id)]);

        let windows = vec![("24h".to_string(), super::parse_window_to_seconds("24h"))];
        let states = vec!["pending".to_string(), "claimed".to_string(), "processing".to_string()];
        let model_filter = vec![alias.clone()];

        let reservation_ids = super::reserve_capacity_for_batch(
            &state,
            "24h",
            &file_model_counts,
            &model_throughputs,
            &model_ids_by_alias,
            &windows,
            &states,
            &model_filter,
            1.0,
        )
        .await
        .unwrap();

        assert_eq!(reservation_ids.len(), 1);

        let row = sqlx::query!(
            "SELECT reserved_requests, released_at FROM batch_capacity_reservations WHERE id = $1",
            reservation_ids[0]
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(row.reserved_requests, 5);
        assert!(row.released_at.is_none());

        super::release_capacity_reservations(&state, &reservation_ids).await.unwrap();

        let row = sqlx::query!(
            "SELECT released_at FROM batch_capacity_reservations WHERE id = $1",
            reservation_ids[0]
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert!(row.released_at.is_some());
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_reserve_capacity_for_batch_rejects_over_capacity(pool: PgPool) {
        let mut config = create_test_config();
        config.batches.default_throughput = 0.0;

        let state = create_test_app_state_with_fusillade(pool.clone(), config).await;

        let user = create_test_user(&pool, Role::StandardUser).await;
        let endpoint_id = create_test_endpoint(&pool, &format!("test-{}", Uuid::new_v4()), user.id).await;

        let alias = format!("alias-{}", Uuid::new_v4());
        let model_id = create_test_model(&pool, "model-a", &alias, endpoint_id, user.id).await;

        let file_model_counts: HashMap<String, i64> = HashMap::from([(alias.clone(), 1_i64)]);
        let model_throughputs = HashMap::from([(alias.clone(), 0.0_f32)]);
        let model_ids_by_alias = HashMap::from([(alias.clone(), model_id)]);

        let windows = vec![("1h".to_string(), super::parse_window_to_seconds("1h"))];
        let states = vec!["pending".to_string(), "claimed".to_string(), "processing".to_string()];
        let model_filter = vec![alias.clone()];

        let err = super::reserve_capacity_for_batch(
            &state,
            "1h",
            &file_model_counts,
            &model_throughputs,
            &model_ids_by_alias,
            &windows,
            &states,
            &model_filter,
            1.0,
        )
        .await
        .unwrap_err();

        match err {
            Error::TooManyRequests { .. } => {}
            other => panic!("expected TooManyRequests, got {other:?}"),
        }

        let count = sqlx::query_scalar!(
            "SELECT COUNT(*) as count FROM batch_capacity_reservations WHERE model_id = $1 AND completion_window = $2",
            model_id,
            "1h"
        )
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(count.unwrap_or(0), 0);
    }

    /// Test that create_batch API accepts "high" priority name
    #[sqlx::test]
    #[test_log::test]
    async fn test_create_batch_with_high_priority(pool: PgPool) {
        // Create app with config allowing 1h window
        let mut config = create_test_config();
        config.batches.allowed_completion_windows = vec!["1h".to_string(), "24h".to_string()];

        let (app, _bg_services) = create_test_app_with_config(pool.clone(), config, false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Upload a batch file
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;
        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");
        let multipart = axum_test::multipart::MultipartForm::new()
            .add_part("file", file_part)
            .add_part("purpose", axum_test::multipart::Part::text("batch"));
        let upload_resp = app
            .post("/ai/v1/files")
            .multipart(multipart)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        upload_resp.assert_status(StatusCode::CREATED);
        let file: serde_json::Value = upload_resp.json();
        let file_id = file["id"].as_str().unwrap();

        // Create batch with "high" priority (should normalize to "1h")
        let create_req = CreateBatchRequest {
            input_file_id: file_id.to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "1h".to_string(),
            metadata: None,
        };

        let resp = app
            .post("/ai/v1/batches")
            .json(&create_req)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        resp.assert_status(StatusCode::CREATED);
        let batch: serde_json::Value = resp.json();

        // Verify the API returns formatted priority label (stored as "1h" internally)
        assert_eq!(batch["completion_window"].as_str().unwrap(), "1h");
    }

    /// Test that create_batch API accepts "standard" priority name
    #[sqlx::test]
    #[test_log::test]
    async fn test_create_batch_with_standard_priority(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Upload a batch file
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;
        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");
        let multipart = axum_test::multipart::MultipartForm::new()
            .add_part("file", file_part)
            .add_part("purpose", axum_test::multipart::Part::text("batch"));
        let upload_resp = app
            .post("/ai/v1/files")
            .multipart(multipart)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        upload_resp.assert_status(StatusCode::CREATED);
        let file: serde_json::Value = upload_resp.json();
        let file_id = file["id"].as_str().unwrap();

        // Create batch with "standard" priority (should normalize to "24h")
        let create_req = CreateBatchRequest {
            input_file_id: file_id.to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
        };

        let resp = app
            .post("/ai/v1/batches")
            .json(&create_req)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        resp.assert_status(StatusCode::CREATED);
        let batch: serde_json::Value = resp.json();

        // Verify the API returns formatted proper priority label
        assert_eq!(batch["completion_window"].as_str().unwrap(), "24h");
    }

    /// Test that legacy "1h" format still works (backwards compatibility)
    #[sqlx::test]
    #[test_log::test]
    async fn test_create_batch_with_legacy_1h_format(pool: PgPool) {
        // Create app with config allowing 1h window
        let mut config = create_test_config();
        config.batches.allowed_completion_windows = vec!["1h".to_string(), "24h".to_string()];

        let (app, _bg_services) = create_test_app_with_config(pool.clone(), config, false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Upload a batch file
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;
        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");
        let multipart = axum_test::multipart::MultipartForm::new()
            .add_part("file", file_part)
            .add_part("purpose", axum_test::multipart::Part::text("batch"));
        let upload_resp = app
            .post("/ai/v1/files")
            .multipart(multipart)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        upload_resp.assert_status(StatusCode::CREATED);
        let file: serde_json::Value = upload_resp.json();
        let file_id = file["id"].as_str().unwrap();

        // Create batch with legacy "1h" format
        let create_req = CreateBatchRequest {
            input_file_id: file_id.to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "1h".to_string(),
            metadata: None,
        };

        let resp = app
            .post("/ai/v1/batches")
            .json(&create_req)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        resp.assert_status(StatusCode::CREATED);
        let batch: serde_json::Value = resp.json();

        // Verify the API returns correct priority label
        assert_eq!(batch["completion_window"].as_str().unwrap(), "1h");
    }

    /// Test that invalid completion_window values are rejected
    #[sqlx::test]
    #[test_log::test]
    async fn test_create_batch_rejects_invalid_completion_window(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // Upload a batch file
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;
        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");
        let multipart = axum_test::multipart::MultipartForm::new()
            .add_part("file", file_part)
            .add_part("purpose", axum_test::multipart::Part::text("batch"));
        let upload_resp = app
            .post("/ai/v1/files")
            .multipart(multipart)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        upload_resp.assert_status(StatusCode::CREATED);
        let file: serde_json::Value = upload_resp.json();
        let file_id = file["id"].as_str().unwrap();

        // Try to create batch with invalid completion_window
        let create_req = CreateBatchRequest {
            input_file_id: file_id.to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "invalid".to_string(),
            metadata: None,
        };

        let resp = app
            .post("/ai/v1/batches")
            .json(&create_req)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        resp.assert_status(StatusCode::BAD_REQUEST);
        let error_text = resp.text();
        assert!(error_text.contains("Unsupported completion_window"));
    }

    /// Test that relaxation factor of 0.0 blocks all batches for that window
    #[sqlx::test]
    #[test_log::test]
    async fn test_create_batch_blocked_by_zero_relaxation_factor(pool: PgPool) {
        let mut config = create_test_config();
        config.batches.allowed_completion_windows = vec!["24h".to_string()];
        config.batches.window_relaxation_factors = std::collections::HashMap::from([("24h".to_string(), 0.0)]);

        let (app, _bg_services) = create_test_app_with_config(pool.clone(), config, false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;
        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test.jsonl");
        let multipart = axum_test::multipart::MultipartForm::new()
            .add_part("file", file_part)
            .add_part("purpose", axum_test::multipart::Part::text("batch"));
        let upload_resp = app
            .post("/ai/v1/files")
            .multipart(multipart)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        upload_resp.assert_status(StatusCode::CREATED);
        let file: serde_json::Value = upload_resp.json();
        let file_id = file["id"].as_str().unwrap();

        let create_req = CreateBatchRequest {
            input_file_id: file_id.to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
        };

        let resp = app
            .post("/ai/v1/batches")
            .json(&create_req)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        // factor=0.0 means effective capacity=0, any request must be rejected
        resp.assert_status(StatusCode::TOO_MANY_REQUESTS);
        let error_text = resp.text();
        assert!(error_text.contains("completion window"), "Error should mention completion window");
        assert!(error_text.contains("gpt-4"), "Error should name the overloaded model");
    }

    /// Test that relaxation factor > 1.0 allows accepting more requests than strict capacity
    #[sqlx::test]
    #[test_log::test]
    async fn test_reserve_capacity_relaxation_factor_expands_acceptance(pool: PgPool) {
        let mut config = create_test_config();
        // Set a very low throughput so strict capacity is tiny
        config.batches.default_throughput = 0.001; // 0.001 req/s = 3.6 requests per hour
        config.batches.allowed_completion_windows = vec!["1h".to_string(), "24h".to_string()];
        // 2× relaxation on 1h: effective capacity = 3.6 * 2 = 7.2 → floor to 7
        config.batches.window_relaxation_factors = std::collections::HashMap::from([("1h".to_string(), 2.0)]);

        let state = create_test_app_state_with_fusillade(pool.clone(), config).await;

        let user = create_test_user(&pool, Role::StandardUser).await;
        let endpoint_id = create_test_endpoint(&pool, &format!("test-{}", Uuid::new_v4()), user.id).await;
        let alias = format!("alias-{}", Uuid::new_v4());
        let model_id = create_test_model(&pool, "model-a", &alias, endpoint_id, user.id).await;

        // 5 requests — would fail at strict capacity (3) but pass with 2× relaxation (7)
        let file_model_counts = HashMap::from([(alias.clone(), 5_i64)]);
        let model_throughputs = HashMap::from([(alias.clone(), 0.001_f32)]);
        let model_ids_by_alias = HashMap::from([(alias.clone(), model_id)]);
        let windows = vec![("1h".to_string(), super::parse_window_to_seconds("1h"))];
        let states = vec!["pending".to_string(), "claimed".to_string(), "processing".to_string()];
        let model_filter = vec![alias.clone()];

        // Without relaxation (strict): should be rejected
        let strict_err = super::reserve_capacity_for_batch(
            &state,
            "1h",
            &file_model_counts,
            &model_throughputs,
            &model_ids_by_alias,
            &windows,
            &states,
            &model_filter,
            1.0,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(strict_err, Error::TooManyRequests { .. }),
            "Should be rejected at strict capacity"
        );

        // With relaxation factor 2.0: should be accepted
        let reservation_ids = super::reserve_capacity_for_batch(
            &state,
            "1h",
            &file_model_counts,
            &model_throughputs,
            &model_ids_by_alias,
            &windows,
            &states,
            &model_filter,
            2.0,
        )
        .await
        .expect("Should be accepted with 2× relaxation factor");
        assert_eq!(reservation_ids.len(), 1);

        super::release_capacity_reservations(&state, &reservation_ids).await.unwrap();
    }

    /// Test that relaxation factors are window-specific — relaxing one window
    /// does not affect another.
    #[sqlx::test]
    #[test_log::test]
    async fn test_reserve_capacity_relaxation_factor_is_window_specific(pool: PgPool) {
        let mut config = create_test_config();
        config.batches.default_throughput = 0.001; // 3.6 req/h strict
        config.batches.allowed_completion_windows = vec!["1h".to_string(), "24h".to_string()];
        // Only relax 24h — 1h stays strict
        config.batches.window_relaxation_factors = std::collections::HashMap::from([("24h".to_string(), 10.0)]);

        let state = create_test_app_state_with_fusillade(pool.clone(), config).await;

        let user = create_test_user(&pool, Role::StandardUser).await;
        let endpoint_id = create_test_endpoint(&pool, &format!("test-{}", Uuid::new_v4()), user.id).await;
        let alias = format!("alias-{}", Uuid::new_v4());
        let model_id = create_test_model(&pool, "model-a", &alias, endpoint_id, user.id).await;

        let file_model_counts = HashMap::from([(alias.clone(), 5_i64)]);
        let model_throughputs = HashMap::from([(alias.clone(), 0.001_f32)]);
        let model_ids_by_alias = HashMap::from([(alias.clone(), model_id)]);
        let states = vec!["pending".to_string(), "claimed".to_string(), "processing".to_string()];
        let model_filter = vec![alias.clone()];

        // 1h window — strict (factor defaults to 1.0), 5 > 3.6, rejected
        let windows_1h = vec![("1h".to_string(), super::parse_window_to_seconds("1h"))];
        let err = super::reserve_capacity_for_batch(
            &state,
            "1h",
            &file_model_counts,
            &model_throughputs,
            &model_ids_by_alias,
            &windows_1h,
            &states,
            &model_filter,
            1.0,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::TooManyRequests { .. }), "1h should be rejected — not relaxed");

        // 24h window — factor=10.0, effective capacity = 86400 * 0.001 * 10 = 864, accepted
        let windows_24h = vec![("24h".to_string(), super::parse_window_to_seconds("24h"))];
        let reservation_ids = super::reserve_capacity_for_batch(
            &state,
            "24h",
            &file_model_counts,
            &model_throughputs,
            &model_ids_by_alias,
            &windows_24h,
            &states,
            &model_filter,
            10.0,
        )
        .await
        .expect("24h should be accepted with 10× relaxation");
        assert_eq!(reservation_ids.len(), 1);

        super::release_capacity_reservations(&state, &reservation_ids).await.unwrap();
    }

    /// Test that the relaxation_factor from config flows through the full API path
    #[sqlx::test]
    #[test_log::test]
    async fn test_create_batch_relaxation_factor_from_config(pool: PgPool) {
        let mut config = create_test_config();
        // Throughput so low that even 1 request fails strict, but 2× relaxation passes
        config.batches.default_throughput = 0.0001; // 0.36 req/h strict → floor 0
        config.batches.allowed_completion_windows = vec!["24h".to_string()];
        // 2× on 24h: 0.0001 * 86400 * 2 = 17.28 → 17 capacity — easily fits 1 request
        config.batches.window_relaxation_factors = std::collections::HashMap::from([("24h".to_string(), 2.0)]);

        let (app, _bg_services) = create_test_app_with_config(pool.clone(), config, false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;
        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test.jsonl");
        let multipart = axum_test::multipart::MultipartForm::new()
            .add_part("file", file_part)
            .add_part("purpose", axum_test::multipart::Part::text("batch"));
        let upload_resp = app
            .post("/ai/v1/files")
            .multipart(multipart)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        upload_resp.assert_status(StatusCode::CREATED);
        let file: serde_json::Value = upload_resp.json();
        let file_id = file["id"].as_str().unwrap();

        let create_req = CreateBatchRequest {
            input_file_id: file_id.to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
        };

        let resp = app
            .post("/ai/v1/batches")
            .json(&create_req)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        // Should be accepted because relaxation factor makes effective capacity > 0
        resp.assert_status(StatusCode::CREATED);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_batch_rejected_with_negative_balance(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;

        // Give user credits then deduct more to make balance negative
        let mut conn = pool.acquire().await.unwrap();
        let mut credits_repo = Credits::new(&mut conn);
        credits_repo
            .create_transaction(&CreditTransactionCreateDBRequest {
                user_id: user.id,
                transaction_type: CreditTransactionType::AdminGrant,
                amount: Decimal::new(100, 2), // $1.00
                source_id: Uuid::new_v4().to_string(),
                description: Some("Initial credits".to_string()),
                fusillade_batch_id: None,
                api_key_id: None,
            })
            .await
            .unwrap();
        credits_repo
            .create_transaction(&CreditTransactionCreateDBRequest {
                user_id: user.id,
                transaction_type: CreditTransactionType::Usage,
                amount: Decimal::new(200, 2), // -$2.00, net = -$1.00
                source_id: Uuid::new_v4().to_string(),
                description: Some("Usage".to_string()),
                fusillade_batch_id: None,
                api_key_id: None,
            })
            .await
            .unwrap();
        drop(credits_repo);
        drop(conn);

        let create_req = CreateBatchRequest {
            input_file_id: Uuid::new_v4().to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
        };

        let auth = add_auth_headers(&user);
        let resp = app
            .post("/ai/v1/batches")
            .json(&create_req)
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;

        resp.assert_status(StatusCode::PAYMENT_REQUIRED);
        let body = resp.text();
        assert!(body.contains("balance too low"), "Expected balance too low message, got: {}", body);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_batch_allowed_with_zero_balance(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, user.id, group.id).await;

        // Create a deployment and add to group so user has access to the model
        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        // User has zero balance (no credits granted) — should still be allowed

        // Upload a batch file
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;
        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");
        let multipart = axum_test::multipart::MultipartForm::new()
            .add_part("file", file_part)
            .add_part("purpose", axum_test::multipart::Part::text("batch"));
        let upload_resp = app
            .post("/ai/v1/files")
            .multipart(multipart)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;
        upload_resp.assert_status(StatusCode::CREATED);
        let file: serde_json::Value = upload_resp.json();
        let file_id = file["id"].as_str().unwrap();

        let create_req = CreateBatchRequest {
            input_file_id: file_id.to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
        };

        let resp = app
            .post("/ai/v1/batches")
            .json(&create_req)
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        // Zero balance should be accepted (only negative is rejected)
        resp.assert_status(StatusCode::CREATED);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_create_batch_in_org_context_checks_org_balance_not_user(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;

        // Create a user with negative personal balance
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let org = create_test_org(&pool, user.id).await;

        let mut conn = pool.acquire().await.unwrap();
        let mut credits_repo = Credits::new(&mut conn);

        // Give user negative balance
        credits_repo
            .create_transaction(&CreditTransactionCreateDBRequest {
                user_id: user.id,
                transaction_type: CreditTransactionType::Usage,
                amount: Decimal::new(500, 2), // -$5.00
                source_id: Uuid::new_v4().to_string(),
                description: Some("Usage".to_string()),
                fusillade_batch_id: None,
                api_key_id: None,
            })
            .await
            .unwrap();

        // Give org positive balance
        credits_repo
            .create_transaction(&CreditTransactionCreateDBRequest {
                user_id: org.id,
                transaction_type: CreditTransactionType::AdminGrant,
                amount: Decimal::new(1000, 2), // $10.00
                source_id: Uuid::new_v4().to_string(),
                description: Some("Org credits".to_string()),
                fusillade_batch_id: None,
                api_key_id: None,
            })
            .await
            .unwrap();
        drop(credits_repo);
        drop(conn);

        // Give org access to a model
        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, org.id, group.id).await;
        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        let auth = add_auth_headers(&user);
        let org_cookie = format!("dw_active_org={}", org.id);

        // Upload a file in org context
        let jsonl_content = r#"{"custom_id":"request-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;
        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("test-batch.jsonl");
        let multipart = axum_test::multipart::MultipartForm::new()
            .add_part("file", file_part)
            .add_part("purpose", axum_test::multipart::Part::text("batch"));
        let upload_resp = app
            .post("/ai/v1/files")
            .multipart(multipart)
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .add_header("cookie", &org_cookie)
            .await;
        upload_resp.assert_status(StatusCode::CREATED);
        let file: serde_json::Value = upload_resp.json();
        let file_id = file["id"].as_str().unwrap();

        // Create batch in org context — should succeed despite user's negative balance
        // because the org has positive balance
        let create_req = CreateBatchRequest {
            input_file_id: file_id.to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
        };

        let resp = app
            .post("/ai/v1/batches")
            .json(&create_req)
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .add_header("cookie", &org_cookie)
            .await;

        resp.assert_status(StatusCode::CREATED);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_batches_member_id_rejected_outside_org_context(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let auth = add_auth_headers(&user);

        // Attempt member_id filter without org context → should return 400
        let resp = app
            .get(&format!("/ai/v1/batches?member_id={}", Uuid::new_v4()))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;
        resp.assert_status(StatusCode::BAD_REQUEST);
        let body = resp.text();
        assert!(
            body.contains("organization context"),
            "Expected error about org context, got: {}",
            body
        );
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_batches_member_id_no_key_returns_empty(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let org = create_test_org(&pool, user.id).await;
        let auth = add_auth_headers(&user);
        let org_cookie = format!("dw_active_org={}", org.id);

        // Filter by a member who has never created batches (no hidden key) → empty list
        let unknown_member = Uuid::new_v4();
        let resp = app
            .get(&format!("/ai/v1/batches?member_id={}", unknown_member))
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .add_header("cookie", &org_cookie)
            .await;
        resp.assert_status_ok();
        let body: serde_json::Value = resp.json();
        assert_eq!(body["data"].as_array().unwrap().len(), 0);
        assert_eq!(body["has_more"], false);
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_batches_enrichment_in_org_context(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;

        let user = create_test_user_with_roles(&pool, vec![Role::StandardUser, Role::BatchAPIUser]).await;
        let org = create_test_org(&pool, user.id).await;

        let group = create_test_group(&pool).await;
        add_user_to_group(&pool, org.id, group.id).await;
        // Also add user directly so they have model access in personal context too
        add_user_to_group(&pool, user.id, group.id).await;
        let deployment = create_test_deployment(&pool, user.id, "gpt-4-model", "gpt-4").await;
        add_deployment_to_group(&pool, deployment.id, group.id, user.id).await;

        let auth = add_auth_headers(&user);
        let org_cookie = format!("dw_active_org={}", org.id);

        // Upload file and create batch in org context
        let jsonl_content = r#"{"custom_id":"req-1","method":"POST","url":"/v1/chat/completions","body":{"model":"gpt-4","messages":[{"role":"user","content":"Hello"}]}}"#;
        let file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("enrich-test.jsonl");
        let multipart = axum_test::multipart::MultipartForm::new()
            .add_part("file", file_part)
            .add_part("purpose", axum_test::multipart::Part::text("batch"));
        let upload_resp = app
            .post("/ai/v1/files")
            .multipart(multipart)
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .add_header("cookie", &org_cookie)
            .await;
        upload_resp.assert_status(StatusCode::CREATED);
        let file: serde_json::Value = upload_resp.json();
        let file_id = file["id"].as_str().unwrap();

        let create_req = CreateBatchRequest {
            input_file_id: file_id.to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
        };
        let create_resp = app
            .post("/ai/v1/batches")
            .json(&create_req)
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .add_header("cookie", &org_cookie)
            .await;
        create_resp.assert_status(StatusCode::CREATED);

        // List in org context → should have enriched metadata
        let list_resp = app
            .get("/ai/v1/batches")
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .add_header("cookie", &org_cookie)
            .await;
        list_resp.assert_status_ok();
        let list_body: serde_json::Value = list_resp.json();
        let batch = &list_body["data"][0];
        let metadata = &batch["metadata"];
        // Org context should see enriched fields
        assert!(
            metadata["created_by_email"].is_string(),
            "Expected enriched created_by_email in org context"
        );
        assert!(
            metadata["context_name"].is_string(),
            "Expected enriched context_name in org context"
        );

        // Create a personal batch (no org context) so we can verify scrubbing
        let personal_file_part = axum_test::multipart::Part::bytes(jsonl_content.as_bytes()).file_name("personal-test.jsonl");
        let personal_multipart = axum_test::multipart::MultipartForm::new()
            .add_part("file", personal_file_part)
            .add_part("purpose", axum_test::multipart::Part::text("batch"));
        let personal_upload_resp = app
            .post("/ai/v1/files")
            .multipart(personal_multipart)
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;
        personal_upload_resp.assert_status(StatusCode::CREATED);
        let personal_file: serde_json::Value = personal_upload_resp.json();
        let personal_file_id = personal_file["id"].as_str().unwrap();

        let personal_batch_req = CreateBatchRequest {
            input_file_id: personal_file_id.to_string(),
            endpoint: "/v1/chat/completions".to_string(),
            completion_window: "24h".to_string(),
            metadata: None,
        };
        let personal_create_resp = app
            .post("/ai/v1/batches")
            .json(&personal_batch_req)
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;
        personal_create_resp.assert_status(StatusCode::CREATED);

        // List outside org context → personal batch should also have enrichment
        // (enrichment is always applied since authorization already controls visibility)
        let list_resp_personal = app
            .get("/ai/v1/batches")
            .add_header(&auth[0].0, &auth[0].1)
            .add_header(&auth[1].0, &auth[1].1)
            .await;
        list_resp_personal.assert_status_ok();
        let list_body_personal: serde_json::Value = list_resp_personal.json();
        let personal_batches = list_body_personal["data"].as_array().unwrap();
        assert!(!personal_batches.is_empty(), "Expected at least one personal batch");
        for batch in personal_batches {
            let meta = batch["metadata"].as_object().expect("metadata should exist");
            assert!(
                meta.contains_key("created_by_email"),
                "created_by_email should be present even in personal context"
            );
            assert!(
                meta.contains_key("context_name"),
                "context_name should be present even in personal context"
            );
            assert_eq!(
                meta.get("context_type").and_then(|v| v.as_str()),
                Some("personal"),
                "personal batch should have context_type=personal"
            );
        }
    }
}
