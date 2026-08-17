//! Main traits for the batching system.
//!
//! This module defines the `Storage` and `RequestManager` traits, which provide the interface
//! for persisting requests, creating files, launching batches, and checking execution status.

use crate::batch::{
    BackgroundBatchInput, Batch, BatchId, BatchInput, BatchStatus, File, FileContentItem,
    FileFilter, FileId, FileStreamItem, FileStreamResult, ListBatchesFilter, OutputFileType,
    RequestTemplateInput,
};
use crate::daemon_record::{AnyDaemonRecord, DaemonRecord, DaemonState, DaemonStatus};
use crate::error::Result;
use crate::request::{
    AnyRequest, CascadeTargetState, Claimed, CreateBackgroundInput, CreateFlexInput,
    CreateRealtimeInput, DaemonId, ListRequestsFilter, PersistCompletedRealtimeInput, Request,
    RequestDetail, RequestId, RequestListResult, RequestState, ServiceTierFilter,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::stream::Stream;
use std::collections::HashMap;
use std::pin::Pin;

/// Outcome of [`DaemonStorage::archive_batch`]. Skips are NORMAL sweeper
/// flow, not errors: candidates are selected outside the move transaction,
/// so by the time the batch row is locked the world may have moved on (a
/// retry un-froze it, another sweeper archived it, a partition is missing).
/// Callers log/alert per variant; only `SkippedNoPartition` warrants an
/// alert (partitions-ahead runway failed), the rest are informational.
/// One cell of demand: requests of one model and service tier counted in
/// one window for one outcome. Returned by
/// [`Storage::get_completed_request_counts_by_model_and_window`] (trailing
/// windows, outcomes `"completed"` / `"failed"`) and
/// [`Storage::get_pending_request_counts_by_model_window_and_tier`]
/// (deadline windows, outcome `"pending"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrailingDemandCount {
    pub model: String,
    /// The caller's window label, echoed back for key matching.
    pub window_label: String,
    /// `None` is the batch tier (`service_tier IS NULL`).
    pub service_tier: Option<String>,
    /// `"pending"`, `"completed"`, or `"failed"`.
    pub outcome: String,
    pub count: i64,
}

/// Operator-supplied rules for automated content expiration.
///
/// The storage layer deliberately does not provide retention periods of its
/// own. A deployment can enable only the content classes it intends to age
/// out, while omitted classes remain subject to explicit deletion.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionPolicy {
    /// Mark files whose existing `expires_at` deadline has passed as expired.
    pub expire_files: bool,
    /// Age terminal file-backed batches out this many seconds after their
    /// terminal timestamp. `None` leaves them subject to explicit deletion.
    pub terminal_batch_seconds: Option<u64>,
    /// Age terminal batchless requests out by their exact service-tier label.
    /// Missing tiers remain subject to explicit deletion.
    pub batchless_seconds_by_service_tier: HashMap<String, u64>,
    /// Keep content-free UUID resurrection fences for at most this many
    /// seconds after the latest destructive response lifecycle event. `None`
    /// disables fence creation; the public library deliberately supplies no
    /// retention default for this independent metadata policy.
    pub max_late_writer_seconds: Option<u64>,
}

impl RetentionPolicy {
    /// Whether this policy contains at least one automated expiration rule.
    pub fn is_enabled(&self) -> bool {
        self.expire_files
            || self.terminal_batch_seconds.is_some()
            || !self.batchless_seconds_by_service_tier.is_empty()
    }

    /// Reject labels that cannot match a persisted service tier safely.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.terminal_batch_seconds == Some(0)
            || self.max_late_writer_seconds == Some(0)
            || self
                .batchless_seconds_by_service_tier
                .values()
                .any(|seconds| *seconds == 0)
        {
            return Err("automated retention periods must be greater than zero".to_string());
        }
        let periods = self
            .terminal_batch_seconds
            .into_iter()
            .chain(self.max_late_writer_seconds)
            .chain(self.batchless_seconds_by_service_tier.values().copied());
        if periods.into_iter().any(|seconds| {
            i64::try_from(seconds)
                .ok()
                .and_then(chrono::Duration::try_seconds)
                .is_none()
        }) {
            return Err("automated retention period is too large".to_string());
        }
        if self
            .batchless_seconds_by_service_tier
            .keys()
            .any(|tier| !matches!(tier.as_str(), "priority" | "flex" | "background"))
        {
            return Err("batchless retention contains an unsupported service tier".to_string());
        }
        if !self.batchless_seconds_by_service_tier.is_empty()
            && self.max_late_writer_seconds.is_none()
        {
            return Err(
                "batchless retention requires an explicit late-writer fence period".to_string(),
            );
        }
        Ok(())
    }

    /// Return the first UTC date on which a terminal batchless response may
    /// be deleted. The entire UTC expiry date remains retained.
    pub fn delete_on(
        terminal_at: DateTime<Utc>,
        seconds: u64,
    ) -> std::result::Result<chrono::NaiveDate, String> {
        let seconds = i64::try_from(seconds)
            .map_err(|_| "automated retention period is too large".to_string())?;
        let duration = chrono::Duration::try_seconds(seconds)
            .ok_or_else(|| "automated retention period is too large".to_string())?;
        let expiry = terminal_at
            .checked_add_signed(duration)
            .ok_or_else(|| "automated retention expiry is out of range".to_string())?;
        expiry
            .date_naive()
            .succ_opt()
            .ok_or_else(|| "automated retention deletion date is out of range".to_string())
    }
}

/// Content-free, matchable retained-response maintenance failures carried in
/// [`crate::error::FusilladeError::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RetainedResponseMaintenanceError {
    /// The storage backend has not opted into retained-response maintenance.
    #[error("Retained response maintenance is not supported by this storage backend")]
    Disabled,
    /// A batchless logical response graph is missing a required member.
    #[error("Retained response graph is incomplete")]
    IncompleteGraph,
    /// A destructive retained-response lifecycle action has no configured
    /// period for its durable content-free resurrection fence.
    #[error("Retained response resurrection fence period is not configured")]
    FencePolicyMissing,
    /// Destructive partition DDL has no explicitly installed single-session
    /// connection boundary.
    #[error("Retained response partition maintenance pool is not configured")]
    PartitionMaintenancePoolMissing,
    /// Durable identity and PostgreSQL's catalogs no longer describe the
    /// exact same retained-response partition generation.
    #[error("Retained response partition retirement identity is inconsistent")]
    RetirementIdentityMismatch,
    /// PostgreSQL could not complete retirement for a non-retryable reason.
    /// The database error is deliberately omitted because it can contain
    /// identifiers and bind values.
    #[error("Retained response partition retirement failed")]
    RetirementFailed,
    /// Creator erasure encountered content whose partition lifecycle is still
    /// in progress and must retry after physical retirement completes.
    #[error("Retained response partition retirement is pending")]
    RetirementPending,
}

impl RetainedResponseMaintenanceError {
    /// Recover this contract from the stable outer Fusillade error type.
    pub fn from_fusillade_error(error: &crate::error::FusilladeError) -> Option<Self> {
        match error {
            crate::error::FusilladeError::Other(error) => error.downcast_ref::<Self>().copied(),
            _ => None,
        }
    }

    /// Wrap this maintenance failure in the stable outer Fusillade error type.
    pub fn into_fusillade_error(self) -> crate::error::FusilladeError {
        crate::error::FusilladeError::Other(anyhow::Error::new(self))
    }
}

/// Content-free, matchable outcomes for writers that arrive after a response
/// graph has left the live tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RetainedResponseWriteError {
    /// The immutable retained graph still exists, so the terminal write is an
    /// idempotent replay rather than a missing request.
    #[error("Response is already retained")]
    AlreadyRetained,
    /// The graph was erased or its retained partition is no longer readable.
    #[error("Response content is no longer available")]
    NotFound,
}

impl RetainedResponseWriteError {
    pub fn from_fusillade_error(error: &crate::error::FusilladeError) -> Option<Self> {
        match error {
            crate::error::FusilladeError::Other(error) => error.downcast_ref::<Self>().copied(),
            _ => None,
        }
    }

    pub fn into_fusillade_error(self) -> crate::error::FusilladeError {
        crate::error::FusilladeError::Other(anyhow::Error::new(self))
    }
}

/// Immutable wall-clock boundaries for one retained-response archive pass.
///
/// Callers resolve relative dwell and cancellation-grace configuration once,
/// then every candidate in the pass is evaluated against the same instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetainedResponseArchiveCutoffs {
    observed_at: DateTime<Utc>,
    terminal_before: DateTime<Utc>,
    cancel_grace_before: DateTime<Utc>,
}

impl RetainedResponseArchiveCutoffs {
    pub fn new(
        observed_at: DateTime<Utc>,
        terminal_before: DateTime<Utc>,
        cancel_grace_before: DateTime<Utc>,
    ) -> std::result::Result<Self, String> {
        if terminal_before > observed_at {
            return Err("terminal movement cutoff cannot be after its observation time".to_owned());
        }
        if cancel_grace_before > observed_at {
            return Err(
                "cancellation grace cutoff cannot be after its observation time".to_owned(),
            );
        }
        Ok(Self {
            observed_at,
            terminal_before,
            cancel_grace_before,
        })
    }

    pub fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }

    pub fn terminal_before(&self) -> DateTime<Utc> {
        self.terminal_before
    }

    pub fn cancel_grace_before(&self) -> DateTime<Utc> {
        self.cancel_grace_before
    }
}

/// Aggregate, content-free result of atomically archiving bounded sets of
/// complete retained batchless response graphs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetainedResponseArchiveOutcome {
    /// Whole logical response graphs committed to retained storage.
    pub groups_archived: u64,
    /// Requests belonging only to fully committed graphs.
    pub requests_archived: u64,
    /// Response steps belonging only to fully committed graphs.
    pub steps_archived: u64,
    /// Templates belonging only to fully committed graphs.
    pub templates_archived: u64,
    /// Payload bytes belonging only to fully committed graphs.
    pub bytes_archived: u64,
    pub skipped_locked: bool,
    pub may_have_more: bool,
}

/// Content-free result of ensuring the complete retained-response partition
/// runway required by the configured policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetainedResponsePartitionRunway {
    /// Partitions created by this maintenance pass.
    pub created: i64,
    /// Exact attached partitions in the continuous prefix beginning tomorrow.
    pub contiguous_ahead: i64,
    /// Exact number of partitions required through the retention horizon and
    /// configured operator runway, inclusive.
    pub required: i64,
}

impl RetainedResponsePartitionRunway {
    /// Movement is safe only when every required deletion day is present.
    pub fn is_complete(self) -> bool {
        self.required > 0 && self.contiguous_ahead == self.required
    }
}

/// Content-free result of one retained-response partition retirement tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetainedResponseRetirementOutcome {
    /// No unfinished retirement exists and no new bucket was selected.
    NoCandidate,
    /// The exact retirement remains durable but could not make progress due
    /// to bounded lock or statement timeout contention.
    Retryable,
    /// One exact daily partition was physically retired.
    Retired,
}

#[cfg(test)]
mod retention_policy_tests {
    use super::*;

    fn policy() -> RetentionPolicy {
        RetentionPolicy {
            batchless_seconds_by_service_tier: HashMap::from([("flex".to_string(), 90 * 86_400)]),
            max_late_writer_seconds: Some(3_600),
            ..Default::default()
        }
    }

    #[test]
    fn batchless_delete_on_is_the_utc_day_after_exact_expiry() {
        let terminal_at = DateTime::parse_from_rfc3339("2026-08-14T15:30:00Z")
            .unwrap()
            .to_utc();
        assert_eq!(
            RetentionPolicy::delete_on(terminal_at, 90 * 86_400).unwrap(),
            chrono::NaiveDate::from_ymd_opt(2026, 11, 13).unwrap()
        );
    }

    #[test]
    fn batchless_delete_on_moves_exact_midnight_expiry_to_the_next_utc_day() {
        // Mutation caught: incrementing the date only when expiry has a
        // non-midnight time component permits deletion on the expiry date.
        let terminal_at = DateTime::parse_from_rfc3339("2026-08-14T00:00:00Z")
            .unwrap()
            .to_utc();
        assert_eq!(
            RetentionPolicy::delete_on(terminal_at, 90 * 86_400).unwrap(),
            chrono::NaiveDate::from_ymd_opt(2026, 11, 13).unwrap()
        );
    }

    #[test]
    fn batchless_delete_on_rejects_timestamp_overflow() {
        // Mutation caught: unchecked expiry arithmetic can wrap or silently
        // select a deletion date after the representable timestamp range.
        assert!(RetentionPolicy::delete_on(DateTime::<Utc>::MAX_UTC, 1).is_err());
    }

    #[test]
    fn batchless_delete_on_rejects_deletion_date_overflow() {
        // Mutation caught: omitting `succ_opt` can wrap the first eligible
        // deletion date past the last representable UTC date.
        assert!(RetentionPolicy::delete_on(DateTime::<Utc>::MAX_UTC, 0).is_err());
    }

    struct DefaultDisabledStorage;

    #[async_trait]
    impl DaemonStorage for DefaultDisabledStorage {
        async fn persist_daemon<T: DaemonState + Clone>(
            &self,
            _record: &DaemonRecord<T>,
        ) -> Result<()>
        where
            AnyDaemonRecord: From<DaemonRecord<T>>,
        {
            Ok(())
        }

        async fn get_daemon(&self, _daemon_id: DaemonId) -> Result<AnyDaemonRecord> {
            Err(crate::error::FusilladeError::ValidationError(
                "not used by this test storage".to_string(),
            ))
        }

        async fn list_daemons(
            &self,
            _status_filter: Option<DaemonStatus>,
        ) -> Result<Vec<AnyDaemonRecord>> {
            Ok(Vec::new())
        }

        async fn purge_orphaned_rows(&self, _batch_size: i64) -> Result<u64> {
            Ok(0)
        }

        async fn archive_batch(&self, _batch_id: BatchId) -> Result<ArchiveOutcome> {
            Ok(ArchiveOutcome::SkippedNotFound)
        }

        async fn list_archivable_batches(
            &self,
            _limit: i64,
            _oldest_first: bool,
            _cancel_grace_secs: f64,
            _min_frozen_age_secs: f64,
        ) -> Result<Vec<BatchId>> {
            Ok(Vec::new())
        }

        async fn count_archivable_batches(&self, _cancel_grace_secs: f64) -> Result<i64> {
            Ok(0)
        }

        async fn count_unfrozen_terminal_batches(&self) -> Result<i64> {
            Ok(0)
        }

        async fn finalize_terminal_batches(&self) -> Result<i64> {
            Ok(0)
        }

        async fn finalize_cancelled_batches(&self, _grace_secs: f64, _limit: i64) -> Result<i64> {
            Ok(0)
        }

        async fn ensure_archive_partitions(&self, _weeks_ahead: i32) -> Result<(i64, i64)> {
            Ok((0, 0))
        }

        async fn purge_model_filter_events(
            &self,
            _batch_size: i64,
            _keep_per_model: i64,
            _retention_secs: f64,
        ) -> Result<u64> {
            Ok(0)
        }
    }

    fn assert_maintenance_is_disabled<T>(result: Result<T>) {
        // Mutation caught: returning a successful no-op, a generic validation
        // error, or enabling maintenance without explicit backend opt-in.
        let error = match result {
            Ok(_) => panic!("retained response maintenance unexpectedly succeeded"),
            Err(error) => error,
        };
        assert_eq!(
            RetainedResponseMaintenanceError::from_fusillade_error(&error),
            Some(RetainedResponseMaintenanceError::Disabled)
        );
    }

    #[tokio::test]
    async fn retained_response_maintenance_is_disabled_without_backend_opt_in() {
        let storage = DefaultDisabledStorage;
        let policy = RetentionPolicy::default();
        let now = DateTime::parse_from_rfc3339("2026-08-14T15:30:00Z")
            .unwrap()
            .to_utc();
        let cutoffs = RetainedResponseArchiveCutoffs::new(now, now, now).unwrap();

        assert!(!storage.supports_retained_response_lifecycle());
        assert!(!storage.supports_retained_response_partition_retirement());
        assert!(!storage.supports_retained_response_route_cleanup());
        assert_maintenance_is_disabled(storage.retained_response_archive_index_ready().await);

        assert_maintenance_is_disabled(
            storage
                .archive_terminal_batchless_responses(&policy, &cutoffs, 1, 1)
                .await,
        );
        assert_maintenance_is_disabled(
            storage
                .ensure_retained_response_partitions(&policy, 1)
                .await,
        );
        assert_maintenance_is_disabled(storage.retire_expired_response_partition(false).await);
        assert_maintenance_is_disabled(storage.cleanup_retained_response_routes(1).await);
        assert_maintenance_is_disabled(storage.cleanup_expired_response_fences(1).await);
    }

    #[test]
    fn retained_response_archive_cutoffs_reject_future_boundaries() {
        let observed_at = DateTime::parse_from_rfc3339("2026-08-14T15:30:00Z")
            .unwrap()
            .to_utc();
        let earlier = observed_at - chrono::Duration::hours(1);
        let later = observed_at + chrono::Duration::nanoseconds(1);

        let cutoffs = RetainedResponseArchiveCutoffs::new(observed_at, earlier, earlier).unwrap();
        assert_eq!(cutoffs.observed_at(), observed_at);
        assert_eq!(cutoffs.terminal_before(), earlier);
        assert_eq!(cutoffs.cancel_grace_before(), earlier);
        assert!(RetainedResponseArchiveCutoffs::new(observed_at, later, earlier).is_err());
        assert!(RetainedResponseArchiveCutoffs::new(observed_at, earlier, later).is_err());
    }

    #[test]
    fn retained_response_error_classifies_an_incomplete_graph_without_payload() {
        // Mutation caught: using an untyped string error leaves callers unable
        // to enforce the invariant that incomplete graphs remain wholly live.
        let error = RetainedResponseMaintenanceError::IncompleteGraph.into_fusillade_error();
        assert_eq!(
            RetainedResponseMaintenanceError::from_fusillade_error(&error),
            Some(RetainedResponseMaintenanceError::IncompleteGraph)
        );
    }

    #[test]
    fn retention_is_disabled_by_default() {
        let policy = RetentionPolicy::default();
        assert!(!policy.is_enabled());
        assert_eq!(policy.max_late_writer_seconds, None);
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn late_writer_window_is_independent_from_content_retention() {
        let policy = RetentionPolicy {
            max_late_writer_seconds: Some(600),
            ..Default::default()
        };

        assert!(!policy.is_enabled());
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn batchless_retention_requires_an_explicit_late_writer_window() {
        let policy = RetentionPolicy {
            batchless_seconds_by_service_tier: HashMap::from([("flex".to_owned(), 600)]),
            ..Default::default()
        };
        assert!(policy.validate().is_err());
    }

    #[test]
    fn retention_rejects_ambiguous_or_immediate_rules() {
        for tier in ["", "unknown"] {
            let policy = RetentionPolicy {
                batchless_seconds_by_service_tier: HashMap::from([(tier.to_string(), 1)]),
                ..Default::default()
            };
            assert!(policy.validate().is_err());
        }

        let realtime = RetentionPolicy {
            batchless_seconds_by_service_tier: HashMap::from([("priority".to_string(), 1)]),
            max_late_writer_seconds: Some(1),
            ..Default::default()
        };
        assert!(
            realtime.validate().is_ok(),
            "persisted realtime content must be configurable for retention"
        );

        let batch = RetentionPolicy {
            terminal_batch_seconds: Some(0),
            ..Default::default()
        };
        assert!(batch.validate().is_err());

        let batchless = RetentionPolicy {
            batchless_seconds_by_service_tier: HashMap::from([("flex".to_string(), 0)]),
            max_late_writer_seconds: Some(1),
            ..Default::default()
        };
        assert!(batchless.validate().is_err());

        let oversized = RetentionPolicy {
            terminal_batch_seconds: Some(i64::MAX as u64),
            ..Default::default()
        };
        assert!(oversized.validate().is_err());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveOutcome {
    /// Rows moved and location stamped; carries the row count moved.
    Archived { rows: u64 },
    /// Batch missing or soft-deleted (purge owns its rows, not the archive).
    SkippedNotFound,
    /// The batch is not available to this mover: either `location` is
    /// already `'archive'` (idempotent no-op), or another transaction holds
    /// its row lock right now — a concurrent mover, or a retry/cancel/freeze
    /// updating the batch (the mover locks with `SKIP LOCKED` and bounces
    /// rather than queueing behind a held lock; contention is counted via
    /// the `fusillade_archive_contended_total` metric). Split batches ARE
    /// valid candidates: re-archiving after a retry resumes moves the
    /// remaining live rows into the same bucket.
    SkippedNotLive,
    /// Counts not frozen: the batch is active again (retry) or was never
    /// finalized. It will re-candidate once frozen.
    SkippedNotFrozen,
    /// The weekly partition for this batch's bucket does not exist. The
    /// batch stays live and fully served; fix partition creation and it
    /// archives on a later pass. Alert-worthy.
    SkippedNoPartition,
    /// Some row is referenced by `response_steps`; the batch stays live
    /// until the batchless store re-homes those rows.
    SkippedResponseSteps,
    /// The `retry_version` CAS on the final stamp failed — a retry raced
    /// the move. Transaction rolled back; nothing moved.
    SkippedRetryRaced,
}

/// Liveness state of a model on internal (self-hosted) infrastructure, as
/// published by the controller into the `model_filters` append-only event log.
///
/// `model_filters` is an event log, not a current-state table: the CURRENT
/// state of a model is the latest event for it. An `Absent` event is an
/// explicit tombstone (the controller retracted the model); a model with no events at
/// all is also treated as absent. The daemon treats absence as "claim now,
/// route to OpenRouter".
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelFilterState {
    /// Internal infrastructure is serving this model now.
    Live,
    /// Internal infrastructure will serve this model soon; `expected_ready_at`
    /// carries the ETA.
    Coming,
    /// The controller is draining this model: it has decided to scale the model to
    /// zero, but the workers are still up finishing their in-flight requests (so it
    /// stays listed in the gateway's `/v1/models`). Distinguished from `Absent` so
    /// observers and the controller can tell "scaling down, still serving" from
    /// "gone", but treated identically to `Coming`/`Absent` by the claim gate
    /// (not-live → no new full-capacity claims).
    Leaving,
    /// Explicit tombstone: the controller is no longer deploying this model.
    /// Appended (instead of deleting rows) to retract a model from the log. Treated
    /// by the claim gate as NOT-LIVE — the same leaky-bucket + deadline-ramp path as
    /// `Coming`/`Leaving`. NOTE: this is *not* the same as a model with **no events
    /// at all**: the gate claims a no-events model at full capacity (it is unmanaged
    /// — `mf.state IS NULL`), whereas an `Absent` model is explicitly held not-live.
    Absent,
}

impl ModelFilterState {
    /// The textual value stored in `model_filters.state`.
    pub fn as_str(self) -> &'static str {
        match self {
            ModelFilterState::Live => "live",
            ModelFilterState::Coming => "coming",
            ModelFilterState::Leaving => "leaving",
            ModelFilterState::Absent => "absent",
        }
    }

    /// Parse the textual `model_filters.state` value.
    pub fn parse_state(s: &str) -> Option<Self> {
        match s {
            "live" => Some(ModelFilterState::Live),
            "coming" => Some(ModelFilterState::Coming),
            "leaving" => Some(ModelFilterState::Leaving),
            "absent" => Some(ModelFilterState::Absent),
            _ => None,
        }
    }
}

/// A single `model_filters` event describing a model's internal-liveness
/// transition.
///
/// `expected_ready_at` is only meaningful when `state == Coming`; for `Live`
/// and `Absent` it should be `None`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelFilter {
    /// Model name (NOT unique — many events per model in the log).
    pub model: String,
    /// Liveness state recorded by this event.
    pub state: ModelFilterState,
    /// ETA when `state == Coming`.
    pub expected_ready_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Background queue modality selected by a dedicated daemon worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundClaimKind {
    /// Requests submitted without a file-backed batch.
    Batchless,
    /// Requests belonging to a file-backed background batch.
    Batch,
}

/// Storage trait for persisting and querying requests.
///
/// This trait provides atomic operations for request lifecycle management.
/// The type system ensures valid state transitions, so implementations don't
/// need to validate them.
#[async_trait]
pub trait Storage: Send + Sync {
    /// Create a new file with templates.
    async fn create_file(
        &self,
        name: String,
        description: Option<String>,
        templates: Vec<RequestTemplateInput>,
    ) -> Result<FileId>;

    /// Create a new file with templates from a stream.
    ///
    /// The stream yields FileStreamItem which can be either:
    /// - Metadata: File metadata (can appear anywhere, will be accumulated)
    /// - Template: Request templates (processed as they arrive)
    /// - Abort: Producer initiated rollback without treating it as a fusillade error
    async fn create_file_stream<S: Stream<Item = FileStreamItem> + Send + Unpin>(
        &self,
        stream: S,
    ) -> Result<FileStreamResult>;

    /// Get a file by ID.
    async fn get_file(&self, file_id: FileId) -> Result<File>;

    /// Get a file by ID from the primary pool for read-after-write consistency.
    ///
    /// Use this immediately after creating or modifying a file to ensure you read
    /// the latest committed data. For normal reads, use `get_file()` which may use
    /// read replicas for better performance.
    async fn get_file_from_primary_pool(&self, file_id: FileId) -> Result<File>;

    /// List files with optional filtering.
    async fn list_files(&self, filter: FileFilter) -> Result<Vec<File>>;

    /// Get all content for a file.
    async fn get_file_content(&self, file_id: FileId) -> Result<Vec<FileContentItem>>;

    /// Stream file content.
    /// Returns different content types based on the file's purpose:
    /// - Regular files (purpose='batch'): RequestTemplateInput
    /// - Batch output files (purpose='batch_output'): BatchOutputItem
    /// - Batch error files (purpose='batch_error'): BatchErrorItem
    ///
    /// # Arguments
    /// * `file_id` - The file ID to stream content from
    /// * `offset` - Number of lines to skip (0-indexed)
    /// * `search` - Optional filter by custom_id (case-insensitive substring match)
    fn get_file_content_stream(
        &self,
        file_id: FileId,
        offset: usize,
        search: Option<String>,
    ) -> Pin<Box<dyn Stream<Item = Result<FileContentItem>> + Send>>;

    /// Get aggregated statistics for request templates grouped by model.
    /// This is optimized for cost estimation - it only fetches model names and body sizes,
    /// avoiding the overhead of streaming full template data.
    ///
    /// Returns a vector of per-model statistics including request count and total body bytes.
    async fn get_file_template_stats(
        &self,
        file_id: FileId,
    ) -> Result<Vec<crate::batch::ModelTemplateStats>>;

    /// Delete a file (cascades to batches and executions).
    async fn delete_file(&self, file_id: FileId) -> Result<()>;

    /// Create a batch from a file's current templates.
    ///
    /// Convenience method that calls [`create_batch_record`] to insert the batch
    /// row, then [`populate_batch`] to copy templates into requests. Returns the
    /// fully-populated batch.
    async fn create_batch(&self, input: BatchInput) -> Result<Batch>;

    /// Create a batch record with virtual output/error files, without populating requests.
    ///
    /// Inserts the batch row and creates virtual output/error files so their IDs
    /// are available in the API response immediately.
    /// Returns a batch in `"validating"` status (`requests_started_at` is NULL).
    /// `total_requests` will be set from `input.total_requests` if provided, or `0` otherwise.
    /// Use [`populate_batch`] to copy templates into requests afterward.
    async fn create_batch_record(&self, input: BatchInput) -> Result<Batch>;

    /// Create and populate a file-backed background batch with no completion SLA.
    async fn create_background_batch(&self, input: BackgroundBatchInput) -> Result<Batch> {
        let _ = input;
        Err(crate::error::FusilladeError::Other(anyhow::anyhow!(
            "background batch creation is not implemented for this storage backend"
        )))
    }

    /// Create a background batch record without populating its requests.
    async fn create_background_batch_record(&self, input: BackgroundBatchInput) -> Result<Batch> {
        let _ = input;
        Err(crate::error::FusilladeError::Other(anyhow::anyhow!(
            "background batch creation is not implemented for this storage backend"
        )))
    }

    /// Populate an existing batch with requests from its file's templates.
    ///
    /// Copies templates into the requests table and updates the batch with
    /// total_requests and requests_started_at.
    /// If the file has no templates, returns a [`ValidationError`](crate::FusilladeError::ValidationError)
    /// and the caller is responsible for marking the batch as failed.
    async fn populate_batch(&self, batch_id: BatchId, file_id: FileId) -> Result<()>;

    /// Get a batch by ID.
    ///
    /// # Arguments
    /// * `batch_id` - The batch ID to retrieve
    async fn get_batch(&self, batch_id: BatchId) -> Result<Batch>;

    /// Get batch status.
    ///
    /// # Arguments
    /// * `batch_id` - The batch ID to retrieve status for
    async fn get_batch_status(&self, batch_id: BatchId) -> Result<BatchStatus>;

    /// List all batches for a file.
    ///
    /// # Arguments
    /// * `file_id` - The file ID to list batches for
    async fn list_file_batches(&self, file_id: FileId) -> Result<Vec<BatchStatus>>;

    /// List batches with optional filtering and cursor-based pagination.
    /// Returns batches sorted by created_at DESC (or active-first when `active_first` is set).
    ///
    /// See [`ListBatchesFilter`] for available filter options including:
    /// - `created_by` - Filter by batch creator user ID
    /// - `search` - Case-insensitive substring match against metadata JSON text,
    ///   input filename, or batch ID
    /// - `after` / `limit` - Cursor-based pagination (limit defaults to 100 if not set)
    /// - `api_key_ids` - Filter by API key UUID(s) that created the batch (for per-member attribution)
    /// - `status` - Filter by batch status. Supported values:
    ///   `"in_progress"`, `"completed"`, `"failed"`, `"cancelled"`, `"expired"`.
    ///   `"in_progress"` covers all non-terminal batches (including validating and finalizing
    ///   sub-states). `"cancelled"` includes batches that are still cancelling.
    ///   `"expired"` matches batches with SLA issues: in-progress past their deadline,
    ///   or terminal batches that finished after their deadline.
    ///   Unrecognized values return an error.
    /// - `created_after` / `created_before` - Time range filter on batch creation timestamp
    /// - `active_first` - When true, sorts active batches before terminal ones
    ///   (completed, failed, cancelled, or cancelling), with each group sorted by
    ///   created_at DESC. Cancelling batches are terminal because cancel_batch sets
    ///   both timestamps atomically. Cursor pagination respects this ordering.
    async fn list_batches(&self, filter: ListBatchesFilter) -> Result<Vec<Batch>>;

    /// Get a batch by its output or error file ID.
    async fn get_batch_by_output_file_id(
        &self,
        file_id: FileId,
        file_type: OutputFileType,
    ) -> Result<Option<Batch>>;

    /// Get all requests for a batch.
    async fn get_batch_requests(&self, batch_id: BatchId) -> Result<Vec<AnyRequest>>;

    /// Stream batch results with merged input/output data.
    ///
    /// Returns a stream of BatchResultItem, each containing:
    /// - The original input body from the request template
    /// - The response body (for completed requests)
    /// - The error message (for failed requests)
    /// - The current status
    ///
    /// # Arguments
    /// * `batch_id` - The batch to get results for
    /// * `offset` - Number of results to skip (for pagination)
    /// * `search` - Optional custom_id filter (case-insensitive substring match)
    /// * `status` - Optional status filter (completed, failed, pending, in_progress)
    fn get_batch_results_stream(
        &self,
        batch_id: BatchId,
        offset: usize,
        search: Option<String>,
        status: Option<String>,
    ) -> Pin<Box<dyn Stream<Item = Result<crate::batch::BatchResultItem>> + Send>>;

    /// Given a list of batch IDs, return those that have been cancelled (cancelling_at IS NOT NULL).
    async fn get_cancelled_batch_ids(&self, batch_ids: &[BatchId]) -> Result<Vec<BatchId>>;

    /// Cancel all pending/in-progress requests for a batch.
    async fn cancel_batch(&self, batch_id: BatchId) -> Result<()>;

    /// Transition in-flight child requests (pending, claimed, processing) to a
    /// terminal state after a batch has been cancelled, failed, or expired.
    ///
    /// Intended to be called asynchronously by the caller after the batch has
    /// already reached a terminal state. Requests already in a terminal state
    /// (completed, failed, canceled) are left untouched.
    ///
    /// Returns the number of rows updated.
    async fn cascade_batch_state_to_requests(
        &self,
        batch_id: BatchId,
        target_state: CascadeTargetState,
    ) -> Result<u64>;

    /// Soft-delete a batch by setting `deleted_at`.
    ///
    /// The batch row is marked deleted and (if not already terminal) cancelled
    /// in the same UPDATE. Child requests and their templates are not touched
    /// inline — they are hidden from active views via the `deleted_at` filter
    /// and hard-deleted asynchronously by the orphan-purge daemon (see
    /// `purge_orphaned_rows`) for right-to-erasure compliance.
    async fn delete_batch(&self, batch_id: BatchId) -> Result<()>;

    /// Hard-delete a single request row for right-to-erasure compliance.
    ///
    /// Removes the `requests` row and, if its template is batchless
    /// (`file_id IS NULL`, dedicated 1:1 to this request), the
    /// `request_templates` row as well — batchless templates carry the
    /// prompt body, so leaving them defeats the erasure. File-backed
    /// templates (shared across siblings in a batch) are not touched here;
    /// the orphan-purge daemon cleans those up after the parent file is
    /// soft-deleted.
    ///
    /// FK behavior on the deleted `requests` row:
    /// * `response_steps.request_id` → `ON DELETE CASCADE`: removes only the
    ///   step row(s) whose `request_id` matches this request. After migration
    ///   `20260430000000` (response_steps re-anchoring), each step points at
    ///   its own per-step sub-request fusillade row, so deleting one request
    ///   only cascade-removes that step. Callers wanting to erase a whole
    ///   multi-step response chain must walk the chain and call this method
    ///   for each backing request.
    /// * Self-references `escalated_from_request_id` / `superseded_by_request_id`
    ///   → `ON DELETE SET NULL`, so sibling rows lose their pointer cleanly.
    ///
    /// In-flight handling: this is an unconditional hard delete. A daemon mid-
    /// update on the row sees 0 rows affected on its next write; a streaming
    /// proxy mid-INSERT of `response_steps` FK-violates (logged, not corrupted).
    /// Both are acceptable for explicit user-initiated erasure.
    ///
    /// Unlike [`Self::delete_batch`] (soft-delete + async purge), this is
    /// immediate because the caller has resolved a specific request to erase.
    ///
    /// Returns `RequestNotFound` if the request does not exist (or was already
    /// deleted).
    #[deprecated(
        note = "use delete_response_group or delete_owned_response_group to erase the complete logical response graph"
    )]
    async fn delete_request(&self, request_id: RequestId) -> Result<()>;

    /// Atomically erase a complete batchless logical response graph.
    ///
    /// `response_id` may name the graph head, any member request, or any
    /// response step. The return value counts erased request objects. Storage
    /// backends that do not implement complete-graph storage fail safely.
    async fn delete_response_group(&self, response_id: uuid::Uuid) -> Result<u64> {
        let _ = response_id;
        Err(RetainedResponseMaintenanceError::Disabled.into_fusillade_error())
    }

    /// Atomically erase a complete batchless logical response graph only when
    /// every request member is owned by `creator_id`.
    ///
    /// Ownership is revalidated while the graph is locked. A missing graph or
    /// ownership mismatch is deliberately reported as `RequestNotFound` so an
    /// authenticated caller cannot use this operation as an existence oracle.
    async fn delete_owned_response_group(
        &self,
        response_id: uuid::Uuid,
        creator_id: &str,
    ) -> Result<u64> {
        let _ = (response_id, creator_id);
        Err(RetainedResponseMaintenanceError::Disabled.into_fusillade_error())
    }

    /// Erase all of a creator's fusillade data, for right-to-erasure compliance.
    ///
    /// Processes up to `batch_size` rows per category per call, using
    /// `FOR UPDATE SKIP LOCKED` so it is safe to run concurrently and under
    /// load. Returns the count of *top-level* rows processed this call —
    /// batchless requests deleted plus batches and files soft-deleted. It does
    /// NOT include the batchless templates removed alongside those requests, nor
    /// the batch/file child rows the purge daemon reaps later. It is purely a
    /// loop-termination signal: callers should loop until it returns 0 to drain
    /// everything. A `batch_size < 1` returns 0 (nothing to do). Idempotent.
    ///
    /// Three categories, keyed on `created_by` / `uploaded_by = creator_id`:
    /// * **Batchless requests** (`batch_id IS NULL` — realtime/flex) are
    ///   *hard*-deleted along with their batchless `request_templates` (which
    ///   carry the prompt body). The orphan-purge daemon never reaches these
    ///   because they have no soft-deleted parent batch, so they must be
    ///   removed here or the erasure is incomplete.
    /// * **Batches** are soft-deleted (cancelled if active) with `metadata`
    ///   nullified (it can contain the user's email). Their child requests are
    ///   hard-deleted afterwards by the orphan-purge daemon (`purge_orphaned_rows`).
    /// * **Files** are soft-deleted; their `request_templates` are likewise
    ///   reaped by the orphan-purge daemon once `files.deleted_at` is set.
    ///
    /// Note: completion is therefore eventually-consistent — when this returns
    /// 0, all batches/files are soft-deleted and batchless rows are gone, but
    /// batch/file child rows are erased on the next purge-daemon pass.
    async fn bulk_delete_data(&self, creator_id: &str, batch_size: i64) -> Result<u64>;

    /// Retry failed requests by resetting them to pending state.
    ///
    /// This resets the specified failed requests to pending state with retry_attempt = 0,
    /// allowing them to be picked up by the daemon for reprocessing.
    ///
    /// # Arguments
    /// * `ids` - Request IDs to retry
    ///
    /// # Returns
    /// A vector of results, one for each request ID. Each result indicates whether
    /// the retry succeeded or failed.
    ///
    /// # Errors
    /// Individual retry results may fail if:
    /// - Request ID doesn't exist
    /// - Request is not in failed state
    async fn retry_failed_requests(&self, ids: Vec<RequestId>) -> Result<Vec<Result<()>>>;

    /// Retry a batch: re-pend its FAILED and CANCELED requests in a single
    /// database operation (completed requests are never redone).
    ///
    /// Retry drives the batch back toward completion and overturns
    /// cancellation — the batch's terminal timestamps, cancellation stamps,
    /// and frozen counts are all reset, so cancel can serve as a pause that
    /// retry resumes. (The name predates canceled-row support and is kept
    /// for API stability.)
    ///
    /// This is much more efficient than `retry_failed_requests`, as it
    /// performs bulk UPDATEs instead of loading requests into memory.
    ///
    /// # Returns
    /// The number of requests that were retried (failed + canceled).
    async fn retry_failed_requests_for_batch(&self, batch_id: BatchId) -> Result<u64>;

    /// Get request counts grouped by model and deadline window.
    ///
    /// Each window is the half-open interval `[now + start_secs, now + end_secs)`
    /// applied to each request's deadline. A request is counted in a
    /// window if its deadline falls inside that range. Because the end is
    /// exclusive, adjacent windows (e.g. `(_, Some(0), 3600)` and
    /// `(_, Some(3600), 86400)`) never double-count a request sitting on the
    /// boundary.
    ///
    /// A request's deadline is its batch's `expires_at`. Batchless rows
    /// (flex/async responses, `batch_id IS NULL`) have no batch expiry, so their
    /// deadline is synthesized as `created_at + W`, where `W` is mapped from the
    /// row's `service_tier` via `DaemonConfig.service_tier_completion_windows_ms`
    /// (`'flex'` → 1h by default, NULL/unmapped → `default_completion_window_ms`,
    /// 24h) — the same window the claim path uses, so reported queue depth
    /// matches what the daemon will claim.
    ///
    /// `start_secs` is optional. When `None`, the lower bound is unbounded
    /// (the query matches every request with a deadline strictly before
    /// `now + end_secs`, including overdue ones). Callers that want the
    /// legacy "due within N, including overdue" semantics pass
    /// `(label, None, N)`. Callers that specifically want the "future N
    /// seconds" starting at `now` pass `(label, Some(0), N)`.
    ///
    /// - `windows`: Vec of `(label, start_secs, end_secs)`. When `start_secs`
    ///   is `Some(s)`, `s` must be `<= end_secs`.
    /// - `states`: request states to include (e.g. `["pending"]`, or
    ///   `["pending","claimed","processing"]`).
    /// - `model_filter`: optional model whitelist (empty = all).
    /// - `service_tier_filter`: filter on `service_tier`. Background demand is
    ///   isolated: `Any` and `Exclude` omit it, while `Include` returns it only
    ///   when `Some("background")` is explicit. `None` represents the batch
    ///   tier (`service_tier IS NULL`).
    /// - `strict`: bool. For critical/sensitive operations, set `true` to
    ///   use the write pool and avoid read lags.
    ///
    /// Excludes:
    /// - Requests without a template_id
    /// - Requests in batches being cancelled
    async fn get_pending_request_counts_by_model_and_window(
        &self,
        windows: &[(String, Option<i64>, i64)],
        states: &[String],
        model_filter: &[String],
        service_tier_filter: &ServiceTierFilter,
        strict: bool,
    ) -> Result<HashMap<String, HashMap<String, i64>>>;

    /// [`Storage::get_pending_request_counts_by_model_and_window`] with the
    /// service tier broken out: one row per (model, window, tier) instead of
    /// tier-summed counts, so grouped consumers (the demand cube) get tier
    /// attribution from a single query instead of one call per tier. Every
    /// row's `outcome` is `"pending"`. Parameters and row scoping are
    /// identical to the flat method — including the `background` special case
    /// (rows labeled with window `"background"`) — and the flat method is
    /// the tier-summed fold of this one.
    async fn get_pending_request_counts_by_model_window_and_tier(
        &self,
        windows: &[(String, Option<i64>, i64)],
        states: &[String],
        model_filter: &[String],
        service_tier_filter: &ServiceTierFilter,
        strict: bool,
    ) -> Result<Vec<TrailingDemandCount>>;

    /// Count terminal requests whose outcome timestamp falls inside trailing
    /// (past) windows, broken out by service tier and outcome.
    ///
    /// The trailing counterpart of
    /// [`Storage::get_pending_request_counts_by_model_and_window`]: where that
    /// method windows *anticipated* demand by deadline, this windows *observed*
    /// demand by when each request reached a terminal state — `completed_at`
    /// for completed rows, `failed_at` for failed rows. Failed rows count as
    /// demand on purpose: a realtime request that was refused upstream is
    /// exactly the unserved demand a capacity planner needs to see.
    ///
    /// - `windows`: `(label, start_secs, end_secs)` offsets from now, with
    ///   `start_secs < end_secs <= 0` (e.g. `(-3600, 0)` = the last hour).
    /// - `model_filter`: optional model whitelist (empty = all).
    /// - `service_tier_filter`: same semantics as the pending method; `None`
    ///   in an `Include` set is the batch tier. Background rows are always
    ///   excluded.
    ///
    /// Row scoping: batch/flex rows require `template_id IS NOT NULL` (same
    /// bookkeeping exclusion as the pending method); priority rows are
    /// identified by `service_tier = 'priority' AND batch_id IS NULL` instead,
    /// because the orphan purger may null `template_id` on old realtime rows.
    ///
    /// Reads the live `requests` table only. Batchless tiers (flex/priority)
    /// are exact; batch-tier rows are moved to `batch_requests_archive` once
    /// their parent batch is terminal and frozen, so batch-tier counts cover
    /// only not-yet-archived rows (a lower bound that decays with sweep
    /// latency). The archive is deliberately not unioned in: its query
    /// contract requires an explicit `archive_bucket` predicate for partition
    /// pruning, which an outcome-time range over all batches cannot supply.
    async fn get_completed_request_counts_by_model_and_window(
        &self,
        windows: &[(String, i64, i64)],
        model_filter: &[String],
        service_tier_filter: &ServiceTierFilter,
    ) -> Result<Vec<TrailingDemandCount>>;

    /// Sum the `total_requests` of a creditor's batches for a given completion
    /// window created on or after `cutoff`.
    ///
    /// Used by the control layer to enforce the unverified upload-volume cap at
    /// batch creation: an unverified creditor may submit at most
    /// `unverified_requests_per_completion_hour * window_hours` requests within
    /// a rolling window equal to the completion window. Served by
    /// `idx_batches_completion_window (completion_window, created_by)`.
    ///
    /// - `owner`: the batch `created_by` — the creditor (organization id for org
    ///   members, user id otherwise).
    /// - `cutoff`: only batches with `created_at >= cutoff` are counted.
    /// - `strict`: set `true` to read from the write pool and avoid read lag, so
    ///   a just-created batch is reflected immediately (required for enforcement).
    async fn sum_owner_batch_requests_in_window(
        &self,
        owner: &str,
        completion_window: &str,
        cutoff: DateTime<Utc>,
        strict: bool,
    ) -> Result<i64>;

    /// Count a creditor's batchless `flex` requests created on or after `cutoff`.
    ///
    /// The flex counterpart of [`Storage::sum_owner_batch_requests_in_window`]:
    /// flex requests are batchless (`batch_id IS NULL`, attribution via
    /// `requests.created_by`) and always map to the 1h completion window. Served
    /// by `idx_requests_user_created_sort (created_by, created_at DESC, id DESC,
    /// service_tier) WHERE created_by IS NOT NULL`.
    ///
    /// - `owner`: the request `created_by` — the creditor id.
    /// - `cutoff`: only requests with `created_at >= cutoff` are counted.
    /// - `strict`: retained for signature compatibility. Archive-aware storage
    ///   backends use the write/primary pool for both values so the live and
    ///   retained arms share one current statement snapshot; `true` continues
    ///   to guarantee the documented no-read-lag behavior.
    async fn count_owner_flex_requests_since(
        &self,
        owner: &str,
        cutoff: DateTime<Utc>,
        strict: bool,
    ) -> Result<i64>;
    ///
    /// Cancel one or more individual pending or in-progress requests.
    ///
    /// Requests that have already completed or failed cannot be canceled.
    /// This is a best-effort operation - some requests may have already been processed.
    ///
    /// Returns a result for each request ID indicating whether cancellation succeeded.
    ///
    /// # Errors
    /// Individual cancellation results may fail if:
    /// - Request ID doesn't exist
    /// - Request is already in a terminal state (completed/failed)
    #[tracing::instrument(skip(self, ids), fields(count = ids.len()))]
    async fn cancel_requests(&self, ids: Vec<RequestId>) -> Result<Vec<Result<()>>> {
        tracing::debug!(count = ids.len(), "Cancelling requests");

        let mut results = Vec::new();

        for id in ids {
            // Get the request from storage
            let get_results = self.get_requests(vec![id]).await?;
            let request_result = get_results.into_iter().next().unwrap();

            let result = match request_result {
                Ok(any_request) => match any_request {
                    AnyRequest::Pending(req) => {
                        req.cancel(self).await?;
                        Ok(())
                    }
                    AnyRequest::Claimed(req) => {
                        req.cancel(self).await?;
                        Ok(())
                    }
                    AnyRequest::Processing(req) => {
                        req.cancel(self).await?;
                        Ok(())
                    }
                    AnyRequest::Completed(_) | AnyRequest::Failed(_) | AnyRequest::Canceled(_) => {
                        Err(crate::error::FusilladeError::InvalidState(
                            id,
                            "terminal state".to_string(),
                            "cancellable state".to_string(),
                        ))
                    }
                },
                Err(e) => Err(e),
            };

            results.push(result);
        }

        Ok(results)
    }

    /// Get in progress requests by IDs.
    async fn get_requests(&self, ids: Vec<RequestId>) -> Result<Vec<Result<AnyRequest>>>;

    // These methods are used by the DaemonExecutor for pulling requests, and then persisting their
    // states as they iterate through them

    /// Atomically claim pending batchless requests for processing.
    ///
    /// `available_capacity` maps model names to the number of permits the daemon
    /// is currently holding for that model. Only models present in this map will
    /// be claimed — this is the authoritative set of models to process.
    ///
    /// `user_active_counts` maps user identifiers to their current number of
    /// in-flight requests across all models. Used to prioritise users with fewer
    /// active requests for per-user fair scheduling. Pass an empty map to disable
    /// user-level prioritisation (falls back to deadline-only ordering).
    ///
    /// Implementations may blend user-fairness with SLA urgency (batch deadline
    /// proximity) via `DaemonConfig::urgency_weight`. See the PostgreSQL
    /// implementation for the composite scoring formula.
    ///
    /// The claim gate consults the latest `model_filters` event per model:
    /// `state = 'live'` **or no events at all** ⇒ claim at full capacity (a
    /// model with no events is unmanaged by the controller, so there is no
    /// internal capacity to wait for — it flows straight through to OpenRouter).
    /// An EXPLICIT not-live event (`coming`/`absent`) ⇒ the request is either
    /// claimed at full capacity (→ OpenRouter) when within `ramp(W)` of its
    /// completion-window deadline, or otherwise released only via the
    /// per-`(user, window-class, model)` leaky bucket. So with an empty `model_filters`
    /// table the gate is a no-op (everything claims at full capacity) — it only
    /// engages once the controller starts writing not-live events.
    ///
    /// `leak_cooldown` is the set of `(user, window-class, model)` triples whose
    /// leaky bucket has no token this cycle (the daemon stamped `next_token_at`
    /// in the future after a recent leak). Source B skips these triples, claiming
    /// ≤ 1 per `(user, window-class, model)` not in cooldown. Pass an empty set
    /// to allow every bucket its first token. Claimed rows carry a `leaked` flag
    /// (via the returned request) so the daemon knows which buckets to stamp.
    async fn claim_batchless_requests(
        &self,
        limit: usize,
        daemon_id: DaemonId,
        available_capacity: &std::collections::HashMap<String, usize>,
        user_active_counts: &std::collections::HashMap<String, usize>,
        leak_cooldown: &std::collections::HashSet<(String, String, String)>,
    ) -> Result<Vec<Request<Claimed>>> {
        self.claim_requests(
            limit,
            daemon_id,
            available_capacity,
            user_active_counts,
            leak_cooldown,
        )
        .await
    }

    /// Compatibility method for callers and storage implementations that have
    /// not yet moved to the explicit request daemon API.
    ///
    /// New daemon code should call [`Storage::claim_batchless_requests`] or
    /// [`Storage::claim_batch_requests`] directly. This method is kept
    /// as a batchless-only alias so the request and batch policies cannot be
    /// accidentally recombined.
    async fn claim_requests(
        &self,
        limit: usize,
        daemon_id: DaemonId,
        available_capacity: &std::collections::HashMap<String, usize>,
        user_active_counts: &std::collections::HashMap<String, usize>,
        leak_cooldown: &std::collections::HashSet<(String, String, String)>,
    ) -> Result<Vec<Request<Claimed>>>;

    /// Atomically claim pending requests that belong to live-model batches.
    ///
    /// The batch daemon owns this policy. Implementations should select
    /// candidate batches before probing request rows, limit selected batches by
    /// `batch_limit`, and gate on model liveness: models whose latest
    /// `model_filters` event is `live` are always eligible; models with **no**
    /// filter event (external / always-on providers that scouter does not
    /// manage) are eligible unless `DaemonConfig::batch_claim_require_live` is
    /// set; models whose latest event is `coming`/`absent` are eligible only
    /// once the batch is within the deadline ramp (`claim_ramp_exponent`) —
    /// the SLA escape hatch to fallback providers. No leaky-bucket trickle
    /// applies to batched rows.
    async fn claim_batch_requests(
        &self,
        limit: usize,
        batch_limit: usize,
        daemon_id: DaemonId,
        available_capacity: &std::collections::HashMap<String, usize>,
        user_active_counts: &std::collections::HashMap<String, usize>,
    ) -> Result<Vec<Request<Claimed>>> {
        let _ = (
            limit,
            batch_limit,
            daemon_id,
            available_capacity,
            user_active_counts,
        );
        // Fail loud rather than silently claiming nothing: a backend that
        // doesn't override this would otherwise run a batch daemon that never
        // claims a row — invisible in production until batches stall.
        Err(crate::error::FusilladeError::Other(anyhow::anyhow!(
            "claim_batch_requests is not implemented for this storage backend \
             (override it, or return false from supports_batch_claims to run \
             the daemon request-only)"
        )))
    }

    /// Whether this backend implements [`Storage::claim_batch_requests`].
    ///
    /// The daemon only spawns its batch claim loop when this returns true.
    /// Defaults to true so a backend that forgets to override BOTH methods
    /// fails loudly (the default `claim_batch_requests` errors) instead of
    /// silently never claiming batched rows. A deliberately request-only
    /// backend should override this to return false.
    fn supports_batch_claims(&self) -> bool {
        true
    }

    /// Atomically claim pending background requests for one queue modality.
    #[doc(hidden)]
    async fn claim_background_requests_by_kind(
        &self,
        kind: BackgroundClaimKind,
        limit: usize,
        batch_limit: usize,
        daemon_id: DaemonId,
        available_capacity: &std::collections::HashMap<String, usize>,
        user_active_counts: &std::collections::HashMap<String, usize>,
    ) -> Result<Vec<Request<Claimed>>> {
        let _ = (
            kind,
            limit,
            batch_limit,
            daemon_id,
            available_capacity,
            user_active_counts,
        );
        Err(crate::error::FusilladeError::Other(anyhow::anyhow!(
            "background claims are not implemented for this storage backend"
        )))
    }

    /// Atomically claim pending background requests from the batchless queue.
    async fn claim_background_batchless_requests(
        &self,
        limit: usize,
        daemon_id: DaemonId,
        available_capacity: &std::collections::HashMap<String, usize>,
        user_active_counts: &std::collections::HashMap<String, usize>,
    ) -> Result<Vec<Request<Claimed>>> {
        self.claim_background_requests_by_kind(
            BackgroundClaimKind::Batchless,
            limit,
            0,
            daemon_id,
            available_capacity,
            user_active_counts,
        )
        .await
    }

    /// Atomically claim pending requests from file-backed background batches.
    async fn claim_background_batch_requests(
        &self,
        limit: usize,
        batch_limit: usize,
        daemon_id: DaemonId,
        available_capacity: &std::collections::HashMap<String, usize>,
        user_active_counts: &std::collections::HashMap<String, usize>,
    ) -> Result<Vec<Request<Claimed>>> {
        self.claim_background_requests_by_kind(
            BackgroundClaimKind::Batch,
            limit,
            batch_limit,
            daemon_id,
            available_capacity,
            user_active_counts,
        )
        .await
    }

    /// Combined helper that claims batchless background requests first, then
    /// uses the remaining global and per-model cycle capacity for batches.
    async fn claim_background_requests(
        &self,
        limit: usize,
        batch_limit: usize,
        daemon_id: DaemonId,
        available_capacity: &std::collections::HashMap<String, usize>,
        user_active_counts: &std::collections::HashMap<String, usize>,
    ) -> Result<Vec<Request<Claimed>>> {
        let mut claimed = self
            .claim_background_batchless_requests(
                limit,
                daemon_id,
                available_capacity,
                user_active_counts,
            )
            .await?;
        let remaining = limit.saturating_sub(claimed.len());
        let mut remaining_capacity = available_capacity.clone();
        for request in &claimed {
            if let Some(capacity) = remaining_capacity.get_mut(&request.data.model) {
                *capacity = capacity.saturating_sub(1);
            }
        }
        if remaining > 0
            && batch_limit > 0
            && remaining_capacity.values().any(|capacity| *capacity > 0)
        {
            claimed.extend(
                self.claim_background_batch_requests(
                    remaining,
                    batch_limit,
                    daemon_id,
                    &remaining_capacity,
                    user_active_counts,
                )
                .await?,
            );
        }
        Ok(claimed)
    }

    /// Whether this backend implements background claim methods.
    fn supports_background_claims(&self) -> bool {
        false
    }

    /// Append a single event to the `model_filters` log. Used by the controller
    /// when a model's internal liveness CHANGES (live / coming / absent).
    ///
    /// The gate reads only `state` (live ⇒ claim full; coming/absent ⇒ not-live).
    /// `expected_ready_at` is retained on the type/column for the controller's own
    /// use but is **not read by the claim gate** — callers may leave it `None`.
    ///
    /// This is append-only: there is no delete and no upsert. Retraction is
    /// appending an `Absent` event. Appending **only on change** (so the log
    /// stays a transition log rather than a poll log) is the caller's
    /// responsibility — this function always inserts a row.
    async fn append_model_filter_event(&self, entry: &ModelFilter) -> Result<()>;

    /// Append a batch of events to the `model_filters` log (one row each, in
    /// order). Convenience for the controller publishing several transitions in
    /// one sync. Same append-only / append-on-change semantics as
    /// [`Storage::append_model_filter_event`].
    async fn append_model_filter_events(&self, entries: &[ModelFilter]) -> Result<()>;

    /// List the CURRENT state of every model (the latest event per model),
    /// excluding models whose latest event is an `Absent` tombstone
    /// (observability / tests).
    async fn list_model_filters(&self) -> Result<Vec<ModelFilter>>;

    /// The CURRENT state of every model **and when that state began**: the latest
    /// event per model as `(state, since)`, where `since` is that event's
    /// `created_at`. Includes `Absent` (a model's latest event may be a tombstone);
    /// the caller decides what to do with each state.
    ///
    /// This is the controller's read for time-based decisions off the log — a
    /// `Live` model's `since` is when it went live (minimum-lifetime / anti-thrash),
    /// a `Coming` model's `since` is when it started launching (a stuck-`coming`
    /// watchdog). The timestamp comes from the persisted log, not in-memory state,
    /// so it survives controller restarts.
    async fn current_filter_states(
        &self,
    ) -> Result<std::collections::HashMap<String, (ModelFilterState, chrono::DateTime<chrono::Utc>)>>;

    /// Update an existing request's state in storage.
    ///
    /// Returns `Some(request_id)` if a racing pair was superseded (for cancellation purposes).
    async fn persist<T: RequestState + Clone>(
        &self,
        request: &Request<T>,
    ) -> Result<Option<RequestId>>
    where
        AnyRequest: From<Request<T>>;

    /// Reschedule an in-flight request back to `pending` for an automatic retry,
    /// fenced on the worker that currently owns it.
    ///
    /// This is the daemon's per-attempt retry path. Unlike [`Storage::persist`]
    /// (which matches on `id` only, so the manual retry path can intentionally
    /// resurrect a `failed` row), this transition is guarded by
    /// `state = 'processing' AND daemon_id = <owner>`: it applies ONLY if the row
    /// is still the in-flight claim held by `daemon_id`.
    ///
    /// The guard prevents a finalize-then-resurrect race: if another writer (a
    /// zombie/duplicate worker, or a stale-claim reclaim) has already moved the
    /// row to a terminal state — and a finalizer has sealed the parent batch as a
    /// result — a late retry from this worker must NOT flip it back to `pending`,
    /// orphaning it under a completed batch.
    ///
    /// Returns `true` if the row was rescheduled, `false` if the worker no longer
    /// owns it (lost the race). A `false` result is normal under contention and
    /// should be logged, not treated as an error.
    async fn reschedule_for_retry(
        &self,
        request_id: RequestId,
        owner: DaemonId,
        retry_attempt: u32,
        not_before: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<bool>;

    /// List individual requests across batches with filtering and pagination.
    ///
    /// Supports filtering by creator, completion window, status, model(s),
    /// date range, and active-first sorting. Uses offset-based pagination.
    ///
    /// Note: Token and cost metrics are NOT included — callers should join
    /// against their own analytics tables for that data.
    async fn list_requests(&self, filter: ListRequestsFilter) -> Result<RequestListResult>;

    /// Get a single request by ID with full detail (body, response, error).
    async fn get_request_detail(&self, request_id: RequestId) -> Result<RequestDetail>;

    /// Create a realtime response that the proxy is already handling.
    ///
    /// Inserts a request template (no parent file) and a request row with
    /// `batch_id = NULL` in `processing` state. The proxy completes/fails
    /// the row directly via `complete_request` / `fail_request`; the daemon
    /// never claims it.
    async fn create_realtime(&self, input: CreateRealtimeInput) -> Result<RequestId>;

    /// Create a flex (async) response that the daemon will process.
    ///
    /// Inserts a request template (no parent file) and a request row with
    /// `batch_id = NULL` in `pending` state. The daemon claims and processes
    /// it via the standard flex pipeline.
    async fn create_flex(&self, input: CreateFlexInput) -> Result<RequestId>;

    /// Create a background response for spare-capacity daemon processing.
    async fn create_background(&self, input: CreateBackgroundInput) -> Result<RequestId> {
        let _ = input;
        Err(crate::error::FusilladeError::Other(anyhow::anyhow!(
            "background request creation is not implemented for this storage backend"
        )))
    }

    /// Complete a processing request with the response body.
    ///
    /// Transitions the request from "processing" to "completed" and stores the
    /// response body and HTTP status code.
    async fn complete_request(
        &self,
        request_id: RequestId,
        response_body: &str,
        status_code: u16,
    ) -> Result<()>;

    /// Fail a processing request with an error message and HTTP status code.
    ///
    /// Transitions the request from "processing" to "failed" and stores the
    /// error as a `NonRetriableHttpStatus` JSON object with the given status code.
    async fn fail_request(
        &self,
        request_id: RequestId,
        error: &str,
        status_code: u16,
    ) -> Result<()>;

    /// Persist a batch of already-completed realtime responses in one transaction.
    ///
    /// Designed for the dwctl responses writer: dwctl proxies a realtime
    /// request, captures the upstream response, and flushes a buffer of
    /// completed records here. Two cases are handled together:
    ///
    ///   * Background realtime: a `processing` row exists (created inline by
    ///     `create_realtime` before the 202 response). UPDATEd to `completed`.
    ///   * Non-background realtime: no row exists. INSERTed (template + request)
    ///     directly in `completed` state.
    ///
    /// Rows already in a terminal state (rare: duplicate enqueues, late
    /// completions for flex slip-through) are left alone via `ON CONFLICT`.
    ///
    /// All work runs in a single transaction so commit overhead amortises
    /// across the batch. An empty input is a no-op.
    async fn persist_completed_realtime_batch(
        &self,
        records: &[PersistCompletedRealtimeInput],
    ) -> Result<()>;
}

/// Daemon lifecycle persistence.
///
/// This trait provides storage operations for tracking daemon state,
/// including registration, heartbeat updates, and graceful shutdown.
#[async_trait]
pub trait DaemonStorage: Send + Sync {
    /// Persist daemon state update.
    ///
    /// This is a low-level method used by state transition methods.
    /// The type parameter `T` ensures type-safe state transitions.
    async fn persist_daemon<T: DaemonState + Clone>(&self, record: &DaemonRecord<T>) -> Result<()>
    where
        AnyDaemonRecord: From<DaemonRecord<T>>;

    /// Get daemon by ID.
    ///
    /// Returns an `AnyDaemonRecord` which can hold the daemon in any state.
    async fn get_daemon(&self, daemon_id: DaemonId) -> Result<AnyDaemonRecord>;

    /// List all daemons with optional status filter.
    ///
    /// If `status_filter` is `None`, returns all daemons regardless of status.
    /// Otherwise, returns only daemons matching the specified status.
    async fn list_daemons(
        &self,
        status_filter: Option<DaemonStatus>,
    ) -> Result<Vec<AnyDaemonRecord>>;

    /// Purge orphaned request_templates and requests whose parent (file or batch)
    /// has been soft-deleted or whose FK is NULL.
    ///
    /// Deletes at most `batch_size` rows from each table per call.
    /// Returns total rows deleted across both tables. Called periodically by
    /// the daemon purge task for right-to-erasure compliance.
    async fn purge_orphaned_rows(&self, batch_size: i64) -> Result<u64>;

    /// Whether this backend explicitly implements the partitioned retained
    /// response lifecycle. Backends must opt in; the default keeps movement
    /// and retirement unavailable.
    fn supports_retained_response_lifecycle(&self) -> bool {
        false
    }

    /// Whether this storage instance has an explicitly installed
    /// single-session connection boundary for retained-response partition
    /// retirement. This is instance capability, not merely backend support.
    fn supports_retained_response_partition_retirement(&self) -> bool {
        false
    }

    /// Whether this storage instance can transfer retired route identifiers
    /// into bounded resurrection fences.
    fn supports_retained_response_route_cleanup(&self) -> bool {
        false
    }

    /// Archive terminal batchless logical response graphs in bounded groups.
    ///
    /// Each graph must be verified complete, then moved in one atomic commit;
    /// every count in [`RetainedResponseArchiveOutcome`] must describe only
    /// fully committed graphs. If a graph is partial (a required request,
    /// response step, or template is absent), implementations must return
    /// [`RetainedResponseMaintenanceError::IncompleteGraph`] and
    /// leave that entire graph live. Completed earlier groups may remain
    /// committed, so callers retry idempotently. Storage backends opt in
    /// explicitly; the default is disabled.
    ///
    /// [`RetainedResponseArchiveCutoffs::terminal_before`] is an immutable
    /// dwell boundary, independent from retention expiry. Implementations
    /// must revalidate every graph member against it and may move a graph only
    /// when its conservative deletion day is after
    /// [`RetainedResponseArchiveCutoffs::observed_at`]'s UTC date. Already-due
    /// live graphs remain for an explicitly gated legacy path.
    async fn archive_terminal_batchless_responses(
        &self,
        _policy: &RetentionPolicy,
        _cutoffs: &RetainedResponseArchiveCutoffs,
        _max_groups: i64,
        _max_bytes: i64,
    ) -> Result<RetainedResponseArchiveOutcome> {
        Err(RetainedResponseMaintenanceError::Disabled.into_fusillade_error())
    }

    /// Report whether the exact operator-managed archive candidate index is
    /// ready and valid. Storage backends opt in explicitly; the default is
    /// disabled.
    async fn retained_response_archive_index_ready(&self) -> Result<bool> {
        Err(RetainedResponseMaintenanceError::Disabled.into_fusillade_error())
    }

    /// Ensure the daily partitions required for retained response graphs.
    /// Storage backends opt in explicitly; the default is disabled.
    async fn ensure_retained_response_partitions(
        &self,
        _policy: &RetentionPolicy,
        _days_ahead: i32,
    ) -> Result<RetainedResponsePartitionRunway> {
        Err(RetainedResponseMaintenanceError::Disabled.into_fusillade_error())
    }

    /// Retire one daily retained-response partition only after it is eligible.
    /// Storage backends opt in explicitly; the default is disabled.
    async fn retire_expired_response_partition(
        &self,
        _select_new: bool,
    ) -> Result<RetainedResponseRetirementOutcome> {
        Err(RetainedResponseMaintenanceError::Disabled.into_fusillade_error())
    }

    /// Remove bounded stale routing metadata for retained responses. Storage
    /// backends opt in explicitly; the default is disabled.
    async fn cleanup_retained_response_routes(&self, _limit: i64) -> Result<u64> {
        Err(RetainedResponseMaintenanceError::Disabled.into_fusillade_error())
    }

    /// Remove bounded expired content-free resurrection fences. A fence that
    /// a concurrent lifecycle action renews or upgrades at deletion time must
    /// survive. Storage backends opt in explicitly; the default is disabled.
    async fn cleanup_expired_response_fences(&self, _limit: i64) -> Result<u64> {
        Err(RetainedResponseMaintenanceError::Disabled.into_fusillade_error())
    }

    /// Move one terminal batch's request rows from `requests` (live) into
    /// `batch_requests_archive` in a single bounded transaction (batches are
    /// capped at 50k rows), stamping `batches.location = 'archive'` and
    /// `batches.archive_bucket`.
    ///
    /// Preconditions are checked inside the transaction; violations return a
    /// `Skipped*` outcome rather than an error — the sweeper treats skips as
    /// normal flow:
    /// - batch exists, not soft-deleted, `location = 'live'`, counts frozen
    ///   (`counts_frozen_at` set). **Only frozen batches move**: freezing
    ///   guarantees rows are settled and the counters are the durable
    ///   record, and it carries Phase 2's `retry_version` protection — any
    ///   retry un-freezes and bumps the version first.
    /// - the weekly archive partition for the batch's bucket exists;
    ///   otherwise the batch simply stays live (fully served, exactly as
    ///   today) and the caller alerts — graceful degradation, no failure.
    /// - no row is referenced by `response_steps` (those stay live until the
    ///   batchless store gives them a home).
    ///
    /// Transaction invariants (fusillade-requests-phase3-plan.md §1):
    /// - forward move is `INSERT ... SELECT r.*, $bucket` with
    ///   `ON CONFLICT DO NOTHING` — idempotent under crash-resume replay.
    /// - the DELETE removes only rows verifiably present in the archive and
    ///   the transaction aborts if any row would be left behind: a row lives
    ///   in exactly one table, always.
    /// - the location stamp re-checks `retry_version` (CAS) even though the
    ///   batch-row lock makes a race impossible on this path — belt and
    ///   braces against future callers taking weaker locks.
    async fn archive_batch(&self, batch_id: BatchId) -> Result<ArchiveOutcome>;

    /// List batches eligible for archiving (`location = 'live'`, counts
    /// frozen, not soft-deleted). Both production movers — the steady-state
    /// sweeper AND the historical backfill — pass `oldest_first = true`: in
    /// steady state the sweeper drains its whole candidate set every few
    /// ticks so order is cosmetic, and under any backlog the
    /// least-recently-created batches are the least likely to ever be read
    /// again, so early issues have minimal blast radius. `false`
    /// (newest-first) exists as an ordering choice for other callers.
    ///
    /// `cancel_grace_secs` is the cancellation grace window: a batch is NOT
    /// a candidate while it has canceled rows that were IN FLIGHT at cancel
    /// (the cascade leaves `claimed_at` set on them; pending-canceled rows
    /// have it NULL) with `canceled_at` younger than the grace. Cancellation
    /// is async and best-effort, and billed in-flight results SUPERSEDE the
    /// cancel (see the persist() transition matrix, fusillade 21.2.1) — the
    /// supersede lands on the LIVE row, so the rows must not move until all
    /// in-flight work has had time to declare itself. Default the grace to
    /// the processing timeout (~10 min): only cancelled batches archive
    /// later, fully served from live meanwhile; normal batches have no such
    /// rows and are unaffected. A frozen batch can never GAIN such a row
    /// (the cascade only touches non-terminal rows and freezing requires
    /// all-terminal), so this selection-time check cannot be raced by the
    /// move itself.
    /// `min_frozen_age_secs` is the post-freeze dwell: 0 means frozen
    /// batches are candidates immediately (the default — reads are mid-move
    /// safe by construction and the sweep cadence provides organic dwell).
    async fn list_archivable_batches(
        &self,
        limit: i64,
        oldest_first: bool,
        cancel_grace_secs: f64,
        min_frozen_age_secs: f64,
    ) -> Result<Vec<BatchId>>;

    /// Count of batches currently eligible for archiving (same predicate as
    /// [`Self::list_archivable_batches`] minus the ordering/limit) — the
    /// sweep-backlog gauge. Index-only on the partial sweep index.
    async fn count_archivable_batches(&self, cancel_grace_secs: f64) -> Result<i64>;

    /// Count of terminal batches (a terminal timestamp set) whose counters
    /// have not been frozen — the finalization-lag gauge. Sustained nonzero
    /// means batches are stuck on the recount path and cannot archive.
    async fn count_unfrozen_terminal_batches(&self) -> Result<i64>;

    /// Stamp terminal timestamps and freeze final counts for non-cancelled
    /// batches whose rows have all reached terminal state. The sweep half of
    /// what `get_batch` does lazily on read; notification delivery consumes
    /// the frozen batches separately. Returns batches finalized.
    async fn finalize_terminal_batches(&self) -> Result<i64>;

    /// Settle-and-freeze cancelled batches past the grace window: physically
    /// cancels any leftover pending/claimed/processing rows, then freezes
    /// counts — atomically per batch, batch-row locked first (uniform
    /// batch→requests lock order). Batches whose settled row count disagrees
    /// with `total_requests` (cancel-during-populate zombies) are skipped and
    /// left unfrozen. Returns batches finalized.
    async fn finalize_cancelled_batches(&self, grace_secs: f64, limit: i64) -> Result<i64>;

    /// Ensure weekly archive partitions exist through now + `weeks_ahead`
    /// (create -> bounds CHECK -> attach; advisory-locked; idempotent).
    /// Returns `(created, ahead)`: partitions created this call, and how
    /// many future weeks (including the current one) now have partitions —
    /// the `fusillade_archive_partitions_ahead` gauge, alert-worthy when it
    /// shrinks below 2.
    async fn ensure_archive_partitions(&self, weeks_ahead: i32) -> Result<(i64, i64)>;

    /// Purge old `model_filters` events, ALWAYS retaining, per model, the most
    /// recent `keep_per_model` events (so the current-state lookup and a short
    /// history window survive) AND every event newer than `retention_secs`
    /// regardless of count.
    ///
    /// Deletes at most `batch_size` rows per call. Returns rows deleted.
    /// Called periodically by the daemon purge task to bound the append-only
    /// log. `keep_per_model >= 1` guarantees the latest event per model is
    /// never purged, so the claim gate never loses a model's current state.
    async fn purge_model_filter_events(
        &self,
        batch_size: i64,
        keep_per_model: i64,
        retention_secs: f64,
    ) -> Result<u64>;
}
