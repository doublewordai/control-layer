//! Versioned retained-response object snapshots.
//!
//! This module defines the representation/decoding boundary, the bounded
//! atomic mover, and active-bucket read routing for retained response graphs.
//! Partition retirement remains a separate lifecycle phase.

use std::fmt;

use chrono::{DateTime, NaiveDate, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

use super::{PoolProvider, PostgresRequestManager};
use crate::error::{FusilladeError, Result};
use crate::manager::{
    RetainedResponseArchiveCutoffs, RetainedResponseArchiveOutcome,
    RetainedResponseMaintenanceError, RetainedResponseWriteError, RetentionPolicy,
};
use crate::request::{
    ListRequestsFilter, RequestDetail, RequestId, RequestListResult, RequestSummary,
};

/// The only retained response payload version this binary knows how to read.
pub(crate) const RETAINED_RESPONSE_SCHEMA_V1: i16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponseWriteDisposition {
    AlreadyRetained,
    NotFound,
}

impl ResponseWriteDisposition {
    pub(crate) fn into_fusillade_error(self) -> FusilladeError {
        match self {
            Self::AlreadyRetained => RetainedResponseWriteError::AlreadyRetained,
            Self::NotFound => RetainedResponseWriteError::NotFound,
        }
        .into_fusillade_error()
    }
}

#[derive(Debug, Default)]
pub(crate) struct ResponseWriteDispositions {
    pub(crate) already_retained: std::collections::HashSet<Uuid>,
    pub(crate) unavailable: std::collections::HashSet<Uuid>,
}

impl ResponseWriteDispositions {
    fn disposition_for(&self, object_id: Uuid) -> Option<ResponseWriteDisposition> {
        if self.already_retained.contains(&object_id) {
            Some(ResponseWriteDisposition::AlreadyRetained)
        } else if self.unavailable.contains(&object_id) {
            Some(ResponseWriteDisposition::NotFound)
        } else {
            None
        }
    }
}

/// Classify UUIDs without consulting retained payloads. Active routes win
/// over archival fences. A route whose canonical bucket is retiring or
/// retired is itself an unconditional unavailability fence; after route
/// cleanup, the durable unexpired resurrection fence carries that decision.
pub(crate) async fn classify_response_write(
    tx: &mut Transaction<'_, Postgres>,
    object_ids: &[Uuid],
) -> Result<Option<ResponseWriteDisposition>> {
    if object_ids.is_empty() {
        return Ok(None);
    }
    let dispositions = classify_response_write_ids(tx, object_ids).await?;
    if object_ids
        .iter()
        .any(|object_id| dispositions.already_retained.contains(object_id))
    {
        return Ok(Some(ResponseWriteDisposition::AlreadyRetained));
    }
    Ok(object_ids
        .iter()
        .find_map(|object_id| dispositions.disposition_for(*object_id)))
}

/// Classify every input UUID independently. Exact active routes take
/// precedence over archival fences; an identity is unavailable when its route
/// is retiring/retired or, after route cleanup, it has an unexpired fence.
pub(crate) async fn classify_response_write_ids(
    tx: &mut Transaction<'_, Postgres>,
    object_ids: &[Uuid],
) -> Result<ResponseWriteDispositions> {
    if object_ids.is_empty() {
        return Ok(ResponseWriteDispositions::default());
    }
    let classified: Vec<(Uuid, bool)> = sqlx::query_as(
        r#"
        WITH input_ids AS (
            SELECT DISTINCT object_id
            FROM UNNEST($1::uuid[]) AS object_id
        ), candidate_route AS (
            SELECT input.object_id, route.group_id, route.delete_on
            FROM input_ids input
            JOIN retained_response_group_routes route ON route.group_id = input.object_id
            UNION
            SELECT input.object_id, route.group_id, route.delete_on
            FROM input_ids input
            JOIN retained_response_request_routes route ON route.request_id = input.object_id
        ), active_ids AS (
            SELECT DISTINCT route.object_id
            FROM candidate_route route
            JOIN retained_response_group_routes group_route
              ON group_route.group_id = route.group_id
             AND group_route.delete_on = route.delete_on
            JOIN retained_response_buckets bucket ON bucket.delete_on = route.delete_on
            JOIN pg_namespace namespace ON namespace.nspname = bucket.partition_schema
            JOIN pg_class child
              ON child.relnamespace = namespace.oid
             AND child.relname = bucket.partition_table
             AND child.oid = bucket.partition_oid
            JOIN pg_inherits inheritance
              ON inheritance.inhrelid = child.oid
             AND NOT inheritance.inhdetachpending
            WHERE bucket.state = 'active'
              AND bucket.partition_schema = current_schema()
              AND bucket.partition_table =
                  'retained_response_objects_d' || to_char(route.delete_on, 'YYYYMMDD')
              AND inheritance.inhparent =
                  to_regclass(format('%I.retained_response_objects', current_schema()))
              AND pg_get_expr(child.relpartbound, child.oid) = format(
                  'FOR VALUES FROM (%L) TO (%L)', route.delete_on, route.delete_on + 1
              )
        ), retiring_ids AS (
            SELECT DISTINCT route.object_id
            FROM candidate_route route
            JOIN retained_response_group_routes group_route
              ON group_route.group_id = route.group_id
             AND group_route.delete_on = route.delete_on
            JOIN retained_response_buckets bucket ON bucket.delete_on = route.delete_on
            WHERE bucket.state IN ('retiring', 'retired')
              AND bucket.partition_schema = current_schema()
              AND bucket.partition_table =
                  'retained_response_objects_d' || to_char(route.delete_on, 'YYYYMMDD')
              AND NOT EXISTS (
                  SELECT 1
                  FROM active_ids active
                  WHERE active.object_id = route.object_id
              )
        ), fenced_ids AS (
            SELECT input.object_id
            FROM input_ids input
            JOIN retained_response_resurrection_fences fence
              ON fence.object_id = input.object_id
            WHERE fence.expires_at > clock_timestamp()
              AND NOT EXISTS (
                  SELECT 1
                  FROM active_ids active
                  WHERE active.object_id = input.object_id
              )
        )
        SELECT object_id, TRUE AS active FROM active_ids
        UNION ALL
        SELECT object_id, FALSE AS active FROM retiring_ids
        UNION ALL
        SELECT object_id, FALSE AS active FROM fenced_ids
        WHERE NOT EXISTS (
            SELECT 1 FROM retiring_ids retiring
            WHERE retiring.object_id = fenced_ids.object_id
        )
        "#,
    )
    .bind(object_ids)
    .fetch_all(&mut **tx)
    .await
    .map_err(|_| FusilladeError::Other(anyhow::anyhow!("Failed to classify response writes")))?;
    let mut dispositions = ResponseWriteDispositions::default();
    for (object_id, active) in classified {
        if active {
            dispositions.already_retained.insert(object_id);
        } else {
            dispositions.unavailable.insert(object_id);
        }
    }
    Ok(dispositions)
}

/// Content-free errors for the retained-response serialization boundary.
///
/// These errors intentionally do not retain a source error: source errors from
/// JSON or SQL decoding can include customer-controlled values in their
/// display text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetainedResponseSerializationError {
    UnsupportedSchemaVersion,
    UnsupportedObjectKind,
    ObjectKindMismatch,
    MalformedPayload,
    RowMetadataMismatch,
    InvalidRequestTemplateAssociation,
    InvalidRequestSnapshot,
    IncompleteGroup,
    EncodeFailure,
    MalformedRow,
}

impl fmt::Display for RetainedResponseSerializationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::UnsupportedSchemaVersion => "Unsupported retained response schema version",
            Self::UnsupportedObjectKind => "Retained response object kind is unsupported",
            Self::ObjectKindMismatch => {
                "Retained response object kind does not match the requested decoder"
            }
            Self::MalformedPayload => "Retained response payload is malformed",
            Self::RowMetadataMismatch => {
                "Retained response row metadata does not match its payload"
            }
            Self::InvalidRequestTemplateAssociation => {
                "Retained request/template association is invalid"
            }
            Self::InvalidRequestSnapshot => "Retained request snapshot is invalid",
            Self::IncompleteGroup => "Retained response group is incomplete",
            Self::EncodeFailure => "Retained response payload could not be encoded",
            Self::MalformedRow => "Retained response row is malformed",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for RetainedResponseSerializationError {}

impl RetainedResponseSerializationError {
    pub(crate) fn into_fusillade_error(self) -> FusilladeError {
        FusilladeError::Other(anyhow::Error::new(self))
    }
}

/// The tag stored in `retained_response_objects.object_kind`.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RetainedObjectKind {
    Group,
    Request,
}

impl RetainedObjectKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Group => "group",
            Self::Request => "request",
        }
    }
}

impl TryFrom<&str> for RetainedObjectKind {
    type Error = RetainedResponseSerializationError;

    fn try_from(value: &str) -> std::result::Result<Self, Self::Error> {
        match value {
            "group" => Ok(Self::Group),
            "request" => Ok(Self::Request),
            _ => Err(RetainedResponseSerializationError::UnsupportedObjectKind),
        }
    }
}

impl fmt::Debug for RetainedObjectKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Deserialize a JSON object only after proving all listed nullable V1 fields
/// are present. An explicit JSON `null` remains valid; a missing key does not.
fn deserialize_object_with_required_fields<'de, D, T>(
    deserializer: D,
    required_fields: &[&str],
) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let object = value
        .as_object()
        .ok_or_else(|| <D::Error as serde::de::Error>::custom("expected V1 object"))?;
    if required_fields
        .iter()
        .any(|field| !object.contains_key(*field))
    {
        return Err(<D::Error as serde::de::Error>::custom(
            "missing nullable V1 field",
        ));
    }
    serde_json::from_value(value).map_err(<D::Error as serde::de::Error>::custom)
}

/// A complete immutable copy of a `requests` row.
///
/// The request-to-template relationship remains explicit even though the
/// graph mover only accepts the dedicated batchless-template form.
#[derive(Clone, PartialEq, Serialize)]
pub(crate) struct RetainedRequestSnapshot {
    pub(crate) id: Uuid,
    pub(crate) batch_id: Option<Uuid>,
    pub(crate) template_id: Option<Uuid>,
    pub(crate) custom_id: Option<String>,
    pub(crate) model: String,
    pub(crate) state: String,
    pub(crate) retry_attempt: i32,
    pub(crate) not_before: Option<DateTime<Utc>>,
    pub(crate) daemon_id: Option<Uuid>,
    pub(crate) claimed_at: Option<DateTime<Utc>>,
    pub(crate) started_at: Option<DateTime<Utc>>,
    pub(crate) response_status: Option<i16>,
    pub(crate) response_body: Option<String>,
    pub(crate) completed_at: Option<DateTime<Utc>>,
    pub(crate) error: Option<String>,
    pub(crate) failed_at: Option<DateTime<Utc>>,
    pub(crate) canceled_at: Option<DateTime<Utc>>,
    pub(crate) response_size: i64,
    pub(crate) routed_model: Option<String>,
    pub(crate) service_tier: Option<String>,
    pub(crate) created_by: Option<String>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct RetainedRequestSnapshotWire {
    id: Uuid,
    batch_id: Option<Uuid>,
    template_id: Option<Uuid>,
    custom_id: Option<String>,
    model: String,
    state: String,
    retry_attempt: i32,
    not_before: Option<DateTime<Utc>>,
    daemon_id: Option<Uuid>,
    claimed_at: Option<DateTime<Utc>>,
    started_at: Option<DateTime<Utc>>,
    response_status: Option<i16>,
    response_body: Option<String>,
    completed_at: Option<DateTime<Utc>>,
    error: Option<String>,
    failed_at: Option<DateTime<Utc>>,
    canceled_at: Option<DateTime<Utc>>,
    response_size: i64,
    routed_model: Option<String>,
    service_tier: Option<String>,
    created_by: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl<'de> Deserialize<'de> for RetainedRequestSnapshot {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire: RetainedRequestSnapshotWire = deserialize_object_with_required_fields(
            deserializer,
            &[
                "batch_id",
                "template_id",
                "custom_id",
                "not_before",
                "daemon_id",
                "claimed_at",
                "started_at",
                "response_status",
                "response_body",
                "completed_at",
                "error",
                "failed_at",
                "canceled_at",
                "routed_model",
                "service_tier",
                "created_by",
            ],
        )?;
        Ok(Self {
            id: wire.id,
            batch_id: wire.batch_id,
            template_id: wire.template_id,
            custom_id: wire.custom_id,
            model: wire.model,
            state: wire.state,
            retry_attempt: wire.retry_attempt,
            not_before: wire.not_before,
            daemon_id: wire.daemon_id,
            claimed_at: wire.claimed_at,
            started_at: wire.started_at,
            response_status: wire.response_status,
            response_body: wire.response_body,
            completed_at: wire.completed_at,
            error: wire.error,
            failed_at: wire.failed_at,
            canceled_at: wire.canceled_at,
            response_size: wire.response_size,
            routed_model: wire.routed_model,
            service_tier: wire.service_tier,
            created_by: wire.created_by,
            created_at: wire.created_at,
            updated_at: wire.updated_at,
        })
    }
}

impl RetainedRequestSnapshot {
    fn terminal_at(
        &self,
    ) -> std::result::Result<Option<DateTime<Utc>>, RetainedResponseSerializationError> {
        match self.state.as_str() {
            "pending" | "claimed" | "processing" => Ok(None),
            "completed" => Ok(self.completed_at),
            "failed" => Ok(self.failed_at),
            "canceled" => Ok(self.canceled_at),
            _ => Err(RetainedResponseSerializationError::InvalidRequestSnapshot),
        }
    }
}

impl fmt::Debug for RetainedRequestSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RetainedRequestSnapshot")
            .field("has_id", &true)
            .field("has_batch_id", &self.batch_id.is_some())
            .field("has_template_id", &self.template_id.is_some())
            .field("has_custom_id", &self.custom_id.is_some())
            .field("has_model", &!self.model.is_empty())
            .field("has_state", &!self.state.is_empty())
            .field("has_not_before", &self.not_before.is_some())
            .field("has_daemon_id", &self.daemon_id.is_some())
            .field("has_claimed_at", &self.claimed_at.is_some())
            .field("has_started_at", &self.started_at.is_some())
            .field("has_response_status", &self.response_status.is_some())
            .field("has_response_body", &self.response_body.is_some())
            .field("has_completed_at", &self.completed_at.is_some())
            .field("has_error", &self.error.is_some())
            .field("has_failed_at", &self.failed_at.is_some())
            .field("has_canceled_at", &self.canceled_at.is_some())
            .field("has_routed_model", &self.routed_model.is_some())
            .field("has_service_tier", &self.service_tier.is_some())
            .field("has_created_by", &self.created_by.is_some())
            .finish()
    }
}

/// A complete immutable copy of a dedicated `request_templates` row.
#[derive(Clone, PartialEq, Serialize)]
pub(crate) struct RetainedTemplateSnapshot {
    pub(crate) id: Uuid,
    pub(crate) file_id: Option<Uuid>,
    pub(crate) custom_id: Option<String>,
    pub(crate) endpoint: String,
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) body: String,
    pub(crate) model: String,
    pub(crate) api_key: String,
    pub(crate) line_number: i32,
    pub(crate) body_byte_size: i64,
    pub(crate) metadata: Option<serde_json::Value>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct RetainedTemplateSnapshotWire {
    id: Uuid,
    file_id: Option<Uuid>,
    custom_id: Option<String>,
    endpoint: String,
    method: String,
    path: String,
    body: String,
    model: String,
    api_key: String,
    line_number: i32,
    body_byte_size: i64,
    metadata: Option<serde_json::Value>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl<'de> Deserialize<'de> for RetainedTemplateSnapshot {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire: RetainedTemplateSnapshotWire = deserialize_object_with_required_fields(
            deserializer,
            &["file_id", "custom_id", "metadata"],
        )?;
        Ok(Self {
            id: wire.id,
            file_id: wire.file_id,
            custom_id: wire.custom_id,
            endpoint: wire.endpoint,
            method: wire.method,
            path: wire.path,
            body: wire.body,
            model: wire.model,
            api_key: wire.api_key,
            line_number: wire.line_number,
            body_byte_size: wire.body_byte_size,
            metadata: wire.metadata,
            created_at: wire.created_at,
            updated_at: wire.updated_at,
        })
    }
}

impl fmt::Debug for RetainedTemplateSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RetainedTemplateSnapshot")
            .field("has_id", &true)
            .field("has_file_id", &self.file_id.is_some())
            .field("has_custom_id", &self.custom_id.is_some())
            .field("has_endpoint", &!self.endpoint.is_empty())
            .field("has_method", &!self.method.is_empty())
            .field("has_path", &!self.path.is_empty())
            .field("has_body", &!self.body.is_empty())
            .field("has_model", &!self.model.is_empty())
            .field("has_api_key", &!self.api_key.is_empty())
            .field("has_metadata", &self.metadata.is_some())
            .finish()
    }
}

/// V1 request object payload: one request row and a snapshot of its
/// template. The template is either dedicated (moved and deleted with the
/// graph) or an external copy of a file's template (the file keeps its row);
/// the snapshot keeps the retained record self-describing either way.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RetainedRequestPayloadV1 {
    pub(crate) request: RetainedRequestSnapshot,
    pub(crate) template: RetainedTemplateSnapshot,
}

impl RetainedRequestPayloadV1 {
    fn validate(&self) -> std::result::Result<(), RetainedResponseSerializationError> {
        if self.request.batch_id.is_some() || self.request.template_id != Some(self.template.id) {
            return Err(RetainedResponseSerializationError::InvalidRequestTemplateAssociation);
        }
        if self.request.created_by.as_deref().is_none_or(str::is_empty) {
            return Err(RetainedResponseSerializationError::InvalidRequestSnapshot);
        }
        self.request
            .terminal_at()?
            .ok_or(RetainedResponseSerializationError::InvalidRequestSnapshot)?;
        Ok(())
    }

    fn validate_delete_on(
        &self,
        delete_on: NaiveDate,
    ) -> std::result::Result<(), RetainedResponseSerializationError> {
        self.validate()?;
        let terminal_at = self
            .request
            .terminal_at()?
            .ok_or(RetainedResponseSerializationError::InvalidRequestSnapshot)?;
        let delete_at = delete_on
            .and_hms_opt(0, 0, 0)
            .expect("midnight is valid for every NaiveDate")
            .and_utc();
        if self.request.created_at >= delete_at || terminal_at >= delete_at {
            return Err(RetainedResponseSerializationError::InvalidRequestSnapshot);
        }
        Ok(())
    }

    /// Reconstruct the existing batchless request-detail model from its V1 snapshot.
    pub(crate) fn to_request_detail(
        &self,
    ) -> std::result::Result<RequestDetail, RetainedResponseSerializationError> {
        self.validate()?;
        let duration_ms = self.duration_ms();
        Ok(RequestDetail {
            id: self.request.id,
            batch_id: self.request.batch_id,
            model: self.request.model.clone(),
            status: self.request.state.clone(),
            created_at: self.request.created_at,
            completed_at: self.request.completed_at,
            failed_at: self.request.failed_at,
            duration_ms,
            response_status: self.request.response_status,
            body: Some(self.template.body.clone()),
            response_body: self.request.response_body.clone(),
            error: self.request.error.clone(),
            service_tier: self.request.service_tier.clone(),
            created_by: self.request.created_by.clone().unwrap_or_default(),
        })
    }

    pub(crate) fn to_request_summary(
        &self,
    ) -> std::result::Result<RequestSummary, RetainedResponseSerializationError> {
        self.validate()?;
        Ok(RequestSummary {
            id: self.request.id,
            batch_id: self.request.batch_id,
            model: self.request.model.clone(),
            status: self.request.state.clone(),
            created_at: self.request.created_at,
            completed_at: self.request.completed_at,
            failed_at: self.request.failed_at,
            duration_ms: self.duration_ms(),
            response_status: self.request.response_status,
            service_tier: self.request.service_tier.clone(),
            created_by: self.request.created_by.clone().unwrap_or_default(),
        })
    }

    fn duration_ms(&self) -> Option<f64> {
        match (self.request.started_at, self.request.completed_at) {
            (Some(started_at), Some(completed_at)) => Some(
                (completed_at.timestamp_micros() - started_at.timestamp_micros()) as f64 / 1_000.0,
            ),
            _ => None,
        }
    }
}

impl fmt::Debug for RetainedRequestPayloadV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RetainedRequestPayloadV1")
            .field("request", &self.request)
            .field("template", &self.template)
            .finish()
    }
}

/// Row-bound V1 request payload. The flat layout preserves the reviewed V1
/// request/template fields while binding them to the route column that
/// selects a retained graph.
#[derive(Serialize, Deserialize)]
struct RetainedRequestPayloadEnvelopeV1 {
    group_id: Uuid,
    #[serde(flatten)]
    payload: RetainedRequestPayloadV1,
}

/// Content-free group header that proves the exact retained graph membership.
///
/// A retained graph is exactly one request plus its template, so the group is
/// identified by its request and the member list is always that one request.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RetainedGroup {
    pub(crate) group_id: Uuid,
    pub(crate) request_ids: Vec<Uuid>,
}

impl RetainedGroup {
    fn validate_header(&self) -> std::result::Result<(), RetainedResponseSerializationError> {
        if has_duplicate_ids(&self.request_ids)
            || self.request_ids.len() != 1
            || self.request_ids[0] != self.group_id
        {
            return Err(RetainedResponseSerializationError::IncompleteGroup);
        }
        Ok(())
    }

    fn validate_members(
        &self,
        requests: &[RetainedRequestPayloadV1],
    ) -> std::result::Result<(), RetainedResponseSerializationError> {
        self.validate_header()?;

        let request_ids = requests
            .iter()
            .map(|payload| payload.request.id)
            .collect::<Vec<_>>();
        if self.request_ids != request_ids || has_duplicate_ids(&request_ids) {
            return Err(RetainedResponseSerializationError::IncompleteGroup);
        }
        Ok(())
    }

    /// Compose a tagged group header and every member payload after proving the
    /// supplied member list is exact. This is representation-only: callers
    /// still own database locking and the all-or-nothing move transaction.
    pub(crate) fn compose_rows(
        delete_on: NaiveDate,
        group: Self,
        requests: Vec<RetainedRequestPayloadV1>,
    ) -> std::result::Result<Vec<RetainedResponseObjectRow>, RetainedResponseSerializationError>
    {
        group.validate_members(&requests)?;

        let mut rows = Vec::with_capacity(1 + requests.len());
        rows.push(RetainedResponseObjectRow::group(delete_on, &group)?);
        for payload in &requests {
            rows.push(RetainedResponseObjectRow::request(
                delete_on,
                group.group_id,
                payload,
            )?);
        }
        Ok(rows)
    }
}

impl fmt::Debug for RetainedGroup {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RetainedGroup")
            .field("has_group_id", &true)
            .field("request_count", &self.request_ids.len())
            .finish()
    }
}

fn has_duplicate_ids(ids: &[Uuid]) -> bool {
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.windows(2).any(|pair| pair[0] == pair[1])
}

/// A typed row in `retained_response_objects` before it is bound to SQLx.
#[derive(Clone, PartialEq)]
pub(crate) struct RetainedResponseObjectRow {
    pub(crate) delete_on: NaiveDate,
    pub(crate) group_id: Uuid,
    pub(crate) object_kind: RetainedObjectKind,
    pub(crate) object_id: Uuid,
    pub(crate) request_id: Option<Uuid>,
    pub(crate) created_by: Option<String>,
    pub(crate) service_tier: Option<String>,
    pub(crate) state: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) created_at: Option<DateTime<Utc>>,
    pub(crate) terminal_at: Option<DateTime<Utc>>,
    pub(crate) schema_version: i16,
    pub(crate) payload: serde_json::Value,
}

impl RetainedResponseObjectRow {
    pub(crate) fn group(
        delete_on: NaiveDate,
        group: &RetainedGroup,
    ) -> std::result::Result<Self, RetainedResponseSerializationError> {
        Ok(Self {
            delete_on,
            group_id: group.group_id,
            object_kind: RetainedObjectKind::Group,
            object_id: group.group_id,
            request_id: None,
            created_by: None,
            service_tier: None,
            state: None,
            model: None,
            created_at: None,
            terminal_at: None,
            schema_version: RETAINED_RESPONSE_SCHEMA_V1,
            payload: to_payload(group)?,
        })
    }

    pub(crate) fn request(
        delete_on: NaiveDate,
        group_id: Uuid,
        payload: &RetainedRequestPayloadV1,
    ) -> std::result::Result<Self, RetainedResponseSerializationError> {
        payload.validate_delete_on(delete_on)?;
        Ok(Self {
            delete_on,
            group_id,
            object_kind: RetainedObjectKind::Request,
            object_id: payload.request.id,
            request_id: Some(payload.request.id),
            created_by: payload.request.created_by.clone(),
            service_tier: payload.request.service_tier.clone(),
            state: Some(payload.request.state.clone()),
            model: Some(payload.request.model.clone()),
            created_at: Some(payload.request.created_at),
            terminal_at: payload.request.terminal_at()?,
            schema_version: RETAINED_RESPONSE_SCHEMA_V1,
            payload: to_payload(&RetainedRequestPayloadEnvelopeV1 {
                group_id,
                payload: payload.clone(),
            })?,
        })
    }

    /// Decode the SQL row fields without retaining driver errors that could
    /// expose payload data through an error chain.
    pub(crate) fn from_pg_row(row: &sqlx::postgres::PgRow) -> Result<Self> {
        let decode = || -> std::result::Result<Self, RetainedResponseSerializationError> {
            let object_kind: String = row
                .try_get("object_kind")
                .map_err(|_| RetainedResponseSerializationError::MalformedRow)?;
            Ok(Self {
                delete_on: row
                    .try_get("delete_on")
                    .map_err(|_| RetainedResponseSerializationError::MalformedRow)?,
                group_id: row
                    .try_get("group_id")
                    .map_err(|_| RetainedResponseSerializationError::MalformedRow)?,
                object_kind: RetainedObjectKind::try_from(object_kind.as_str())?,
                object_id: row
                    .try_get("object_id")
                    .map_err(|_| RetainedResponseSerializationError::MalformedRow)?,
                request_id: row
                    .try_get("request_id")
                    .map_err(|_| RetainedResponseSerializationError::MalformedRow)?,
                created_by: row
                    .try_get("created_by")
                    .map_err(|_| RetainedResponseSerializationError::MalformedRow)?,
                service_tier: row
                    .try_get("service_tier")
                    .map_err(|_| RetainedResponseSerializationError::MalformedRow)?,
                state: row
                    .try_get("state")
                    .map_err(|_| RetainedResponseSerializationError::MalformedRow)?,
                model: row
                    .try_get("model")
                    .map_err(|_| RetainedResponseSerializationError::MalformedRow)?,
                created_at: row
                    .try_get("created_at")
                    .map_err(|_| RetainedResponseSerializationError::MalformedRow)?,
                terminal_at: row
                    .try_get("terminal_at")
                    .map_err(|_| RetainedResponseSerializationError::MalformedRow)?,
                schema_version: row
                    .try_get("schema_version")
                    .map_err(|_| RetainedResponseSerializationError::MalformedRow)?,
                payload: row
                    .try_get("payload")
                    .map_err(|_| RetainedResponseSerializationError::MalformedRow)?,
            })
        };
        decode().map_err(RetainedResponseSerializationError::into_fusillade_error)
    }

    pub(crate) fn decode_group(
        &self,
    ) -> std::result::Result<RetainedGroup, RetainedResponseSerializationError> {
        self.require_kind(RetainedObjectKind::Group)?;
        let group: RetainedGroup = self.decode_payload()?;
        if self.object_id != group.group_id
            || self.group_id != group.group_id
            || self.request_id.is_some()
            || self.created_by.is_some()
            || self.service_tier.is_some()
            || self.state.is_some()
            || self.model.is_some()
            || self.created_at.is_some()
            || self.terminal_at.is_some()
        {
            return Err(RetainedResponseSerializationError::RowMetadataMismatch);
        }
        group.validate_header()?;
        Ok(group)
    }

    pub(crate) fn decode_request(
        &self,
    ) -> std::result::Result<RetainedRequestPayloadV1, RetainedResponseSerializationError> {
        self.require_kind(RetainedObjectKind::Request)?;
        let envelope: RetainedRequestPayloadEnvelopeV1 = self.decode_payload()?;
        let payload = envelope.payload;
        payload.validate_delete_on(self.delete_on)?;
        if self.group_id != envelope.group_id
            || self.object_id != payload.request.id
            || self.request_id != Some(payload.request.id)
            || self.created_by != payload.request.created_by
            || self.service_tier != payload.request.service_tier
            || self.state.as_deref() != Some(payload.request.state.as_str())
            || self.model.as_deref() != Some(payload.request.model.as_str())
            || self.created_at != Some(payload.request.created_at)
            || self.terminal_at != payload.request.terminal_at()?
        {
            return Err(RetainedResponseSerializationError::RowMetadataMismatch);
        }
        Ok(payload)
    }

    fn require_kind(
        &self,
        expected: RetainedObjectKind,
    ) -> std::result::Result<(), RetainedResponseSerializationError> {
        if self.schema_version != RETAINED_RESPONSE_SCHEMA_V1 {
            return Err(RetainedResponseSerializationError::UnsupportedSchemaVersion);
        }
        if self.object_kind != expected {
            return Err(RetainedResponseSerializationError::ObjectKindMismatch);
        }
        Ok(())
    }

    fn decode_payload<T: DeserializeOwned>(
        &self,
    ) -> std::result::Result<T, RetainedResponseSerializationError> {
        serde_json::from_value(self.payload.clone())
            .map_err(|_| RetainedResponseSerializationError::MalformedPayload)
    }
}

impl fmt::Debug for RetainedResponseObjectRow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RetainedResponseObjectRow")
            .field("has_group_id", &true)
            .field("object_kind", &self.object_kind)
            .field("has_object_id", &true)
            .field("has_request_id", &self.request_id.is_some())
            .field("has_created_by", &self.created_by.is_some())
            .field("has_service_tier", &self.service_tier.is_some())
            .field("has_state", &self.state.is_some())
            .field("has_model", &self.model.is_some())
            .field("has_created_at", &self.created_at.is_some())
            .field("has_terminal_at", &self.terminal_at.is_some())
            .field("has_payload", &true)
            .finish()
    }
}

fn to_payload<T: Serialize>(
    value: &T,
) -> std::result::Result<serde_json::Value, RetainedResponseSerializationError> {
    serde_json::to_value(value).map_err(|_| RetainedResponseSerializationError::EncodeFailure)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetainedResponseReadError {
    DatabaseFailure,
}

impl fmt::Display for RetainedResponseReadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Retained response read failed")
    }
}

impl std::error::Error for RetainedResponseReadError {}

fn read_database_failure<T>(_: T) -> FusilladeError {
    FusilladeError::Other(anyhow::Error::new(
        RetainedResponseReadError::DatabaseFailure,
    ))
}

async fn begin_primary_read<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
) -> Result<Transaction<'static, Postgres>> {
    let mut tx = manager.begin_write().await.map_err(read_database_failure)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(read_database_failure)?;
    Ok(tx)
}

const RETAINED_OBJECT_COLUMNS: &str = "object.delete_on, object.group_id, object.object_kind, \
    object.object_id, object.request_id, object.created_by, object.service_tier, object.state, \
    object.model, object.created_at, object.terminal_at, object.schema_version, object.payload";

pub(crate) async fn get_request_detail<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    request_id: RequestId,
) -> Result<RequestDetail> {
    let mut tx = begin_primary_read(manager).await?;
    let live = sqlx::query_as::<_, RequestDetail>(
        r#"
        SELECT
            request.id, request.batch_id, request.model, request.state,
            request.created_at, request.completed_at, request.failed_at,
            (CASE WHEN request.completed_at IS NOT NULL AND request.started_at IS NOT NULL
                THEN EXTRACT(EPOCH FROM (request.completed_at - request.started_at)) * 1000
                ELSE NULL END)::float8 AS duration_ms,
            request.response_status,
            CASE WHEN file.deleted_at IS NULL OR template.file_id IS NULL
                THEN template.body ELSE NULL END AS body,
            request.response_body, request.error, request.service_tier,
            request.created_by
        FROM requests request
        LEFT JOIN LATERAL (
            SELECT * FROM request_templates_all t WHERE t.id = request.template_id LIMIT 1
        ) template ON TRUE
        LEFT JOIN files file ON template.file_id = file.id
        WHERE request.id = $1 AND request.created_by IS NOT NULL
        "#,
    )
    .bind(request_id.0)
    .fetch_optional(&mut *tx)
    .await
    .map_err(read_database_failure)?;

    let detail = if let Some(live) = live {
        Some(live)
    } else {
        let query = format!(
            r#"
            SELECT {RETAINED_OBJECT_COLUMNS}
            FROM retained_response_request_routes route
            JOIN retained_response_group_routes group_route
              ON group_route.group_id = route.group_id
             AND group_route.delete_on = route.delete_on
            JOIN retained_response_buckets bucket
              ON bucket.delete_on = route.delete_on
            JOIN pg_namespace namespace
              ON namespace.nspname = bucket.partition_schema
            JOIN pg_class child
              ON child.relnamespace = namespace.oid
             AND child.relname = bucket.partition_table
             AND child.oid = bucket.partition_oid
            JOIN pg_inherits inheritance
              ON inheritance.inhrelid = child.oid
             AND NOT inheritance.inhdetachpending
            JOIN retained_response_objects object
              ON object.delete_on = route.delete_on
             AND object.group_id = route.group_id
             AND object.object_kind = 'request'
             AND object.object_id = route.request_id
             AND object.request_id = route.request_id
            WHERE route.request_id = $1
              AND bucket.state = 'active'
              AND bucket.partition_schema = current_schema()
              AND bucket.partition_table =
                  'retained_response_objects_d' || to_char(route.delete_on, 'YYYYMMDD')
              AND inheritance.inhparent =
                  to_regclass(format('%I.retained_response_objects', current_schema()))
              AND pg_get_expr(child.relpartbound, child.oid) = format(
                  'FOR VALUES FROM (%L) TO (%L)', route.delete_on, route.delete_on + 1
              )
            "#,
        );
        sqlx::query(&query)
            .bind(request_id.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(read_database_failure)?
            .map(|row| {
                RetainedResponseObjectRow::from_pg_row(&row)?
                    .decode_request()
                    .and_then(|payload| payload.to_request_detail())
                    .map_err(RetainedResponseSerializationError::into_fusillade_error)
            })
            .transpose()?
    };

    tx.commit().await.map_err(read_database_failure)?;
    detail.ok_or(FusilladeError::RequestNotFound(request_id))
}

/// The exact combined count, kept for the planner-shape test; production
/// counts each arm separately under a budget (see `count_requests_with_budget`).
#[cfg(test)]
const LIST_REQUESTS_COUNT_SQL: &str = r#"
    -- Two independent scalar counts, NOT one union CTE: the CTE fences the
    -- planner out of a parallel scan on the large live arm, turning a
    -- seconds-long count into tens of seconds at production scale.
    SELECT (
        SELECT COUNT(*)
        FROM requests request
        WHERE request.created_by IS NOT NULL
          AND ($1::text IS NULL OR request.created_by = $1)
          AND ($2::text IS NULL OR request.state = $2)
          AND ($3::text[] IS NULL OR request.model = ANY($3))
          AND ($4::timestamptz IS NULL OR request.created_at >= $4)
          AND ($5::timestamptz IS NULL OR request.created_at <= $5)
          AND ($6::text[] IS NULL OR request.service_tier = ANY($6))
    ) + (
        SELECT COUNT(*)
        FROM retained_response_buckets bucket
        JOIN pg_namespace namespace
          ON namespace.nspname = bucket.partition_schema
        JOIN pg_class child
          ON child.relnamespace = namespace.oid
         AND child.relname = bucket.partition_table
         AND child.oid = bucket.partition_oid
        JOIN pg_inherits inheritance
          ON inheritance.inhrelid = child.oid
         AND NOT inheritance.inhdetachpending
        JOIN retained_response_objects object
          ON object.delete_on = bucket.delete_on
         AND object.object_kind = 'request'
        JOIN retained_response_request_routes route
          ON route.request_id = object.object_id
         AND route.group_id = object.group_id
         AND route.delete_on = object.delete_on
        JOIN retained_response_group_routes group_route
          ON group_route.group_id = object.group_id
         AND group_route.delete_on = object.delete_on
        WHERE bucket.state = 'active'
          AND bucket.partition_schema = current_schema()
          AND bucket.partition_table =
              'retained_response_objects_d' || to_char(bucket.delete_on, 'YYYYMMDD')
          AND inheritance.inhparent =
              to_regclass(format('%I.retained_response_objects', current_schema()))
          AND pg_get_expr(child.relpartbound, child.oid) = format(
              'FOR VALUES FROM (%L) TO (%L)', bucket.delete_on, bucket.delete_on + 1
          )
          AND object.created_by IS NOT NULL
          AND ($1::text IS NULL OR object.created_by = $1)
          AND ($2::text IS NULL OR object.state = $2)
          AND ($3::text[] IS NULL OR object.model = ANY($3))
          AND ($4::timestamptz IS NULL OR object.created_at >= $4)
          -- V1 proves created_at < delete_on, so this necessary lower
          -- bound enables daily partition pruning.
          AND ($4::timestamptz IS NULL OR object.delete_on > ($4 AT TIME ZONE 'UTC')::date)
          AND ($5::timestamptz IS NULL OR object.created_at <= $5)
          AND ($6::text[] IS NULL OR object.service_tier = ANY($6))
          AND NOT EXISTS (
              SELECT 1 FROM requests live
              WHERE live.id = object.object_id AND live.created_by IS NOT NULL
          )
    )::bigint
"#;
const LIVE_REQUEST_COUNT_SQL: &str = r#"
        SELECT COUNT(*)
        FROM requests request
        WHERE request.created_by IS NOT NULL
          AND ($1::text IS NULL OR request.created_by = $1)
          AND ($2::text IS NULL OR request.state = $2)
          AND ($3::text[] IS NULL OR request.model = ANY($3))
          AND ($4::timestamptz IS NULL OR request.created_at >= $4)
          AND ($5::timestamptz IS NULL OR request.created_at <= $5)
          AND ($6::text[] IS NULL OR request.service_tier = ANY($6))
"#;

const RETAINED_REQUEST_COUNT_SQL: &str = r#"
        SELECT COUNT(*)
        FROM retained_response_buckets bucket
        JOIN pg_namespace namespace
          ON namespace.nspname = bucket.partition_schema
        JOIN pg_class child
          ON child.relnamespace = namespace.oid
         AND child.relname = bucket.partition_table
         AND child.oid = bucket.partition_oid
        JOIN pg_inherits inheritance
          ON inheritance.inhrelid = child.oid
         AND NOT inheritance.inhdetachpending
        JOIN retained_response_objects object
          ON object.delete_on = bucket.delete_on
         AND object.object_kind = 'request'
        JOIN retained_response_request_routes route
          ON route.request_id = object.object_id
         AND route.group_id = object.group_id
         AND route.delete_on = object.delete_on
        JOIN retained_response_group_routes group_route
          ON group_route.group_id = object.group_id
         AND group_route.delete_on = object.delete_on
        WHERE bucket.state = 'active'
          AND bucket.partition_schema = current_schema()
          AND bucket.partition_table =
              'retained_response_objects_d' || to_char(bucket.delete_on, 'YYYYMMDD')
          AND inheritance.inhparent =
              to_regclass(format('%I.retained_response_objects', current_schema()))
          AND pg_get_expr(child.relpartbound, child.oid) = format(
              'FOR VALUES FROM (%L) TO (%L)', bucket.delete_on, bucket.delete_on + 1
          )
          AND object.created_by IS NOT NULL
          AND ($1::text IS NULL OR object.created_by = $1)
          AND ($2::text IS NULL OR object.state = $2)
          AND ($3::text[] IS NULL OR object.model = ANY($3))
          AND ($4::timestamptz IS NULL OR object.created_at >= $4)
          -- V1 proves created_at < delete_on, so this necessary lower
          -- bound enables daily partition pruning.
          AND ($4::timestamptz IS NULL OR object.delete_on > ($4 AT TIME ZONE 'UTC')::date)
          AND ($5::timestamptz IS NULL OR object.created_at <= $5)
          AND ($6::text[] IS NULL OR object.service_tier = ANY($6))
          AND NOT EXISTS (
              SELECT 1 FROM requests live
              WHERE live.id = object.object_id AND live.created_by IS NOT NULL
          )
"#;

/// Total-count budget: an exact count is worth a short wait, but an
/// unfiltered listing over a 100M-row live table is not allowed to hold a
/// connection for the full scan. Each arm gets `COUNT_BUDGET`; on timeout
/// (SQLSTATE 57014) the arm falls back to the planner's row estimate, which
/// tracks within a few percent when statistics are fresh. Runs on the read
/// pool in its own transaction so it never pins the primary.
const COUNT_BUDGET: &str = "100ms";

async fn count_requests_with_budget<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    filter: &ListRequestsFilter,
) -> Result<i64> {
    let mut tx = manager.begin_read().await.map_err(read_database_failure)?;
    sqlx::query(&format!("SET LOCAL statement_timeout = '{COUNT_BUDGET}'"))
        .execute(&mut *tx)
        .await
        .map_err(read_database_failure)?;
    let mut total = 0_i64;
    for arm in [LIVE_REQUEST_COUNT_SQL, RETAINED_REQUEST_COUNT_SQL] {
        let exact: std::result::Result<i64, sqlx::Error> = sqlx::query_scalar(arm)
            .bind(filter.created_by.as_deref())
            .bind(filter.status.as_deref())
            .bind(filter.models.as_deref())
            .bind(filter.created_after)
            .bind(filter.created_before)
            .bind(filter.service_tiers.as_deref())
            .fetch_one(&mut *tx)
            .await;
        total += match exact {
            Ok(n) => n,
            Err(sqlx::Error::Database(e)) if e.code().as_deref() == Some("57014") => {
                // The failed statement aborted this transaction; estimate on a
                // fresh one. EXPLAIN is planning only, so it runs without the
                // count budget: under the 100ms budget the retained arm's
                // EXPLAIN (planning over the partition catalog) is itself
                // cancelled with 57014, and that cancellation propagated as a
                // 500 on GET /admin/api/v1/batches/requests ("Retained response
                // read failed"). Re-apply the budget afterwards so the next
                // COUNT arm is still capped. A failed estimate degrades to 0
                // rather than failing the whole listing.
                tx.rollback().await.map_err(read_database_failure)?;
                tx = manager.begin_read().await.map_err(read_database_failure)?;
                sqlx::query("SET LOCAL statement_timeout = 0")
                    .execute(&mut *tx)
                    .await
                    .map_err(read_database_failure)?;
                let plan = sqlx::query_scalar::<_, serde_json::Value>(&format!(
                    "EXPLAIN (FORMAT JSON) {}",
                    arm.replacen("SELECT COUNT(*)", "SELECT 1", 1)
                ))
                .bind(filter.created_by.as_deref())
                .bind(filter.status.as_deref())
                .bind(filter.models.as_deref())
                .bind(filter.created_after)
                .bind(filter.created_before)
                .bind(filter.service_tiers.as_deref())
                .fetch_one(&mut *tx)
                .await;
                sqlx::query(&format!("SET LOCAL statement_timeout = '{COUNT_BUDGET}'"))
                    .execute(&mut *tx)
                    .await
                    .map_err(read_database_failure)?;
                match plan {
                    Ok(plan) => plan
                        .get(0)
                        .and_then(|p| p.get("Plan"))
                        .and_then(|p| p.get("Plan Rows"))
                        .and_then(|r| r.as_f64())
                        .map(|r| r.round() as i64)
                        .unwrap_or(0),
                    Err(error) => {
                        tracing::warn!(%error, "count estimate for list_requests arm unavailable; using 0");
                        0
                    }
                }
            }
            Err(e) => return Err(read_database_failure(e)),
        };
    }
    tx.commit().await.map_err(read_database_failure)?;
    Ok(total)
}

fn list_requests_page_sql(active_first: bool) -> String {
    let order_clause = if active_first {
        "CASE sort_state WHEN 'processing' THEN 0 WHEN 'claimed' THEN 1 WHEN 'pending' THEN 2 ELSE 3 END ASC, sort_created_at DESC, sort_id DESC"
    } else {
        "sort_created_at DESC, sort_id DESC"
    };
    // Each arm carries its own ORDER BY + LIMIT so the planner can satisfy
    // the page from ordered indexes (Merge Append over two bounded arms)
    // instead of sorting the whole union. The live arm's expressions mirror
    // idx_requests_active_first_tier / idx_requests_created_tier exactly.
    let live_order = if active_first {
        "CASE request.state WHEN 'processing' THEN 0 WHEN 'claimed' THEN 1 WHEN 'pending' THEN 2 ELSE 3 END ASC, request.created_at DESC, request.id DESC"
    } else {
        "request.created_at DESC, request.id DESC"
    };
    let retained_order = if active_first {
        "CASE object.state WHEN 'processing' THEN 0 WHEN 'claimed' THEN 1 WHEN 'pending' THEN 2 ELSE 3 END ASC, object.created_at DESC, object.object_id DESC"
    } else {
        "object.created_at DESC, object.object_id DESC"
    };
    format!(
        r#"
        WITH candidates AS (
            (SELECT
                request.id AS sort_id,
                request.state AS sort_state,
                request.created_at AS sort_created_at,
                FALSE AS retained,
                jsonb_build_object(
                    'id', request.id,
                    'batch_id', request.batch_id,
                    'model', request.model,
                    'status', request.state,
                    'created_at', request.created_at,
                    'completed_at', request.completed_at,
                    'failed_at', request.failed_at,
                    'duration_ms', (CASE
                        WHEN request.completed_at IS NOT NULL AND request.started_at IS NOT NULL
                        THEN EXTRACT(EPOCH FROM (request.completed_at - request.started_at)) * 1000
                        ELSE NULL END)::float8,
                    'response_status', request.response_status,
                    'service_tier', request.service_tier,
                    'created_by', request.created_by
                ) AS live_summary,
                NULL::date AS delete_on,
                NULL::uuid AS group_id,
                NULL::text AS object_kind,
                NULL::uuid AS object_id,
                NULL::uuid AS request_id,
                NULL::text AS created_by,
                NULL::text AS service_tier,
                NULL::text AS state,
                NULL::text AS model,
                NULL::timestamptz AS created_at,
                NULL::timestamptz AS terminal_at,
                NULL::smallint AS schema_version,
                NULL::jsonb AS payload
            FROM requests request
            WHERE request.created_by IS NOT NULL
              AND ($1::text IS NULL OR request.created_by = $1)
              AND ($2::text IS NULL OR request.state = $2)
              AND ($3::text[] IS NULL OR request.model = ANY($3))
              AND ($4::timestamptz IS NULL OR request.created_at >= $4)
              AND ($5::timestamptz IS NULL OR request.created_at <= $5)
              AND ($6::text[] IS NULL OR request.service_tier = ANY($6))
            ORDER BY {live_order}
            LIMIT $7 + $8)

            UNION ALL

            (SELECT
                object.object_id AS sort_id,
                object.state AS sort_state,
                object.created_at AS sort_created_at,
                TRUE AS retained,
                NULL::jsonb AS live_summary,
                object.delete_on,
                object.group_id,
                object.object_kind,
                object.object_id,
                object.request_id,
                object.created_by,
                object.service_tier,
                object.state,
                object.model,
                object.created_at,
                object.terminal_at,
                object.schema_version,
                object.payload
            FROM retained_response_buckets bucket
            JOIN pg_namespace namespace
              ON namespace.nspname = bucket.partition_schema
            JOIN pg_class child
              ON child.relnamespace = namespace.oid
             AND child.relname = bucket.partition_table
             AND child.oid = bucket.partition_oid
            JOIN pg_inherits inheritance
              ON inheritance.inhrelid = child.oid
             AND NOT inheritance.inhdetachpending
            JOIN retained_response_objects object
              ON object.delete_on = bucket.delete_on
             AND object.object_kind = 'request'
            JOIN retained_response_request_routes route
              ON route.request_id = object.object_id
             AND route.group_id = object.group_id
             AND route.delete_on = object.delete_on
            JOIN retained_response_group_routes group_route
              ON group_route.group_id = object.group_id
             AND group_route.delete_on = object.delete_on
            WHERE bucket.state = 'active'
              AND bucket.partition_schema = current_schema()
              AND bucket.partition_table =
                  'retained_response_objects_d' || to_char(bucket.delete_on, 'YYYYMMDD')
              AND inheritance.inhparent =
                  to_regclass(format('%I.retained_response_objects', current_schema()))
              AND pg_get_expr(child.relpartbound, child.oid) = format(
                  'FOR VALUES FROM (%L) TO (%L)', bucket.delete_on, bucket.delete_on + 1
              )
              AND object.created_by IS NOT NULL
              AND ($1::text IS NULL OR object.created_by = $1)
              AND ($2::text IS NULL OR object.state = $2)
              AND ($3::text[] IS NULL OR object.model = ANY($3))
              AND ($4::timestamptz IS NULL OR object.created_at >= $4)
              -- V1 proves created_at < delete_on, so this necessary lower
              -- bound enables daily partition pruning.
              AND ($4::timestamptz IS NULL OR object.delete_on > ($4 AT TIME ZONE 'UTC')::date)
              AND ($5::timestamptz IS NULL OR object.created_at <= $5)
              AND ($6::text[] IS NULL OR object.service_tier = ANY($6))
              AND NOT EXISTS (
                  SELECT 1 FROM requests live
                  WHERE live.id = object.object_id AND live.created_by IS NOT NULL
              )
            ORDER BY {retained_order}
            LIMIT $7 + $8)
        )
        SELECT retained, live_summary, delete_on, group_id, object_kind,
               object_id, request_id, created_by, service_tier,
               state, model, created_at, terminal_at, schema_version, payload
        FROM candidates
        ORDER BY {order_clause}
        LIMIT $7 OFFSET $8
        "#,
    )
}

pub(crate) async fn list_requests<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    filter: ListRequestsFilter,
) -> Result<RequestListResult> {
    if filter.skip < 0 {
        return Err(FusilladeError::ValidationError(
            "skip must be >= 0".to_string(),
        ));
    }
    if filter.limit <= 0 {
        return Err(FusilladeError::ValidationError(
            "limit must be > 0".to_string(),
        ));
    }

    let count = count_requests_with_budget(manager, &filter).await?;
    let mut tx = begin_primary_read(manager).await?;

    let query = list_requests_page_sql(filter.active_first);
    let rows = sqlx::query(&query)
        .bind(filter.created_by.as_deref())
        .bind(filter.status.as_deref())
        .bind(filter.models.as_deref())
        .bind(filter.created_after)
        .bind(filter.created_before)
        .bind(filter.service_tiers.as_deref())
        .bind(filter.limit)
        .bind(filter.skip)
        .fetch_all(&mut *tx)
        .await
        .map_err(read_database_failure)?;

    let mut data = Vec::with_capacity(rows.len());
    for row in rows {
        if row
            .try_get::<bool, _>("retained")
            .map_err(read_database_failure)?
        {
            data.push(
                RetainedResponseObjectRow::from_pg_row(&row)?
                    .decode_request()
                    .and_then(|payload| payload.to_request_summary())
                    .map_err(RetainedResponseSerializationError::into_fusillade_error)?,
            );
        } else {
            let value: serde_json::Value =
                row.try_get("live_summary").map_err(read_database_failure)?;
            data.push(serde_json::from_value(value).map_err(|_| {
                RetainedResponseSerializationError::MalformedRow.into_fusillade_error()
            })?);
        }
    }
    tx.commit().await.map_err(read_database_failure)?;
    Ok(RequestListResult {
        data,
        total_count: count,
    })
}

const COUNT_OWNER_FLEX_REQUESTS_SQL: &str = r#"
    SELECT COUNT(*)::bigint
    FROM (
        SELECT request.id
        FROM requests request
        WHERE request.created_by = $1
          AND request.created_at >= $2
          AND request.service_tier = 'flex'

        UNION ALL

        SELECT object.object_id
        FROM retained_response_buckets bucket
        JOIN pg_namespace namespace
          ON namespace.nspname = bucket.partition_schema
        JOIN pg_class child
          ON child.relnamespace = namespace.oid
         AND child.relname = bucket.partition_table
         AND child.oid = bucket.partition_oid
        JOIN pg_inherits inheritance
          ON inheritance.inhrelid = child.oid
         AND NOT inheritance.inhdetachpending
        JOIN retained_response_objects object
          ON object.delete_on = bucket.delete_on
         AND object.object_kind = 'request'
        JOIN retained_response_request_routes route
          ON route.request_id = object.object_id
         AND route.group_id = object.group_id
         AND route.delete_on = object.delete_on
        JOIN retained_response_group_routes group_route
          ON group_route.group_id = object.group_id
         AND group_route.delete_on = object.delete_on
        WHERE bucket.state = 'active'
          AND bucket.partition_schema = current_schema()
          AND bucket.partition_table =
              'retained_response_objects_d' || to_char(bucket.delete_on, 'YYYYMMDD')
          AND inheritance.inhparent =
              to_regclass(format('%I.retained_response_objects', current_schema()))
          AND pg_get_expr(child.relpartbound, child.oid) = format(
              'FOR VALUES FROM (%L) TO (%L)', bucket.delete_on, bucket.delete_on + 1
          )
          AND object.created_by = $1
          AND object.created_at >= $2
          -- V1 proves created_at < delete_on.
          AND object.delete_on > ($2 AT TIME ZONE 'UTC')::date
          AND object.service_tier = 'flex'
          AND NOT EXISTS (
              SELECT 1 FROM requests live
              WHERE live.id = object.object_id AND live.created_by IS NOT NULL
          )
    ) candidate
"#;

pub(crate) async fn count_owner_flex_requests_since<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    owner: &str,
    cutoff: DateTime<Utc>,
) -> Result<i64> {
    sqlx::query_scalar(COUNT_OWNER_FLEX_REQUESTS_SQL)
        .bind(owner)
        .bind(cutoff)
        .fetch_one(manager.write_executor())
        .await
        .map_err(read_database_failure)
}

pub(crate) const TRAILING_DEMAND_SQL: &str = r#"
    SELECT model, service_tier, outcome, SUM(count)::BIGINT AS count
    FROM (
    SELECT
        request.model,
        request.service_tier,
        'completed'::text AS outcome,
        COUNT(*)::BIGINT AS count
    FROM requests request
    WHERE request.state = 'completed'
      AND request.completed_at >= $1
      AND request.completed_at < $2
      AND (cardinality($3::text[]) = 0 OR request.model = ANY($3))
      AND request.service_tier IS DISTINCT FROM 'background'
      AND (
          request.template_id IS NOT NULL
          OR (request.service_tier = 'priority' AND request.batch_id IS NULL)
      )
      AND (
          $6 = 'any'
          OR ($6 = 'include' AND (
              (request.service_tier IS NOT NULL AND request.service_tier = ANY($4))
              OR ($5 AND request.service_tier IS NULL)
          ))
          OR ($6 = 'exclude' AND (
              (request.service_tier IS NULL AND NOT $5)
              OR (request.service_tier IS NOT NULL AND request.service_tier <> ALL($4))
          ))
      )
    GROUP BY request.model, request.service_tier

    UNION ALL

    SELECT
        request.model,
        request.service_tier,
        'failed'::text AS outcome,
        COUNT(*)::BIGINT AS count
    FROM requests request
    WHERE request.state = 'failed'
      AND request.failed_at >= $1
      AND request.failed_at < $2
      AND (cardinality($3::text[]) = 0 OR request.model = ANY($3))
      AND request.service_tier IS DISTINCT FROM 'background'
      AND (
          request.template_id IS NOT NULL
          OR (request.service_tier = 'priority' AND request.batch_id IS NULL)
      )
      AND (
          $6 = 'any'
          OR ($6 = 'include' AND (
              (request.service_tier IS NOT NULL AND request.service_tier = ANY($4))
              OR ($5 AND request.service_tier IS NULL)
          ))
          OR ($6 = 'exclude' AND (
              (request.service_tier IS NULL AND NOT $5)
              OR (request.service_tier IS NOT NULL AND request.service_tier <> ALL($4))
          ))
      )
    GROUP BY request.model, request.service_tier

    UNION ALL

    SELECT
        retained.model,
        retained.service_tier,
        'completed'::text AS outcome,
        COUNT(*)::BIGINT AS count
    FROM retained_response_buckets bucket
    JOIN pg_namespace namespace
      ON namespace.nspname = bucket.partition_schema
    JOIN pg_class child
      ON child.relnamespace = namespace.oid
     AND child.relname = bucket.partition_table
     AND child.oid = bucket.partition_oid
    JOIN pg_inherits inheritance
      ON inheritance.inhrelid = child.oid
     AND NOT inheritance.inhdetachpending
    JOIN retained_response_objects retained
      ON retained.delete_on = bucket.delete_on
     AND retained.object_kind = 'request'
    JOIN retained_response_request_routes route
      ON route.request_id = retained.object_id
     AND route.group_id = retained.group_id
     AND route.delete_on = retained.delete_on
    JOIN retained_response_group_routes group_route
      ON group_route.group_id = retained.group_id
     AND group_route.delete_on = retained.delete_on
    WHERE bucket.state = 'active'
      AND bucket.partition_schema = current_schema()
      AND bucket.partition_table =
          'retained_response_objects_d' || to_char(bucket.delete_on, 'YYYYMMDD')
      AND inheritance.inhparent =
          to_regclass(format('%I.retained_response_objects', current_schema()))
      AND pg_get_expr(child.relpartbound, child.oid) = format(
          'FOR VALUES FROM (%L) TO (%L)', bucket.delete_on, bucket.delete_on + 1
      )
      AND retained.state = 'completed'
      AND retained.terminal_at >= $1
      AND retained.terminal_at < $2
      -- V1 proves terminal_at < delete_on, so this necessary lower
      -- bound enables daily partition pruning for the trailing window.
      AND retained.delete_on > ($1 AT TIME ZONE 'UTC')::date
      AND (cardinality($3::text[]) = 0 OR retained.model = ANY($3))
      AND retained.service_tier IS DISTINCT FROM 'background'
      AND (
          $6 = 'any'
          OR ($6 = 'include' AND (
              (retained.service_tier IS NOT NULL AND retained.service_tier = ANY($4))
              OR ($5 AND retained.service_tier IS NULL)
          ))
          OR ($6 = 'exclude' AND (
              (retained.service_tier IS NULL AND NOT $5)
              OR (retained.service_tier IS NOT NULL AND retained.service_tier <> ALL($4))
          ))
      )
      AND NOT EXISTS (
          SELECT 1 FROM requests live
          WHERE live.id = retained.object_id AND live.created_by IS NOT NULL
      )
    GROUP BY retained.model, retained.service_tier

    UNION ALL

    SELECT
        retained.model,
        retained.service_tier,
        'failed'::text AS outcome,
        COUNT(*)::BIGINT AS count
    FROM retained_response_buckets bucket
    JOIN pg_namespace namespace
      ON namespace.nspname = bucket.partition_schema
    JOIN pg_class child
      ON child.relnamespace = namespace.oid
     AND child.relname = bucket.partition_table
     AND child.oid = bucket.partition_oid
    JOIN pg_inherits inheritance
      ON inheritance.inhrelid = child.oid
     AND NOT inheritance.inhdetachpending
    JOIN retained_response_objects retained
      ON retained.delete_on = bucket.delete_on
     AND retained.object_kind = 'request'
    JOIN retained_response_request_routes route
      ON route.request_id = retained.object_id
     AND route.group_id = retained.group_id
     AND route.delete_on = retained.delete_on
    JOIN retained_response_group_routes group_route
      ON group_route.group_id = retained.group_id
     AND group_route.delete_on = retained.delete_on
    WHERE bucket.state = 'active'
      AND bucket.partition_schema = current_schema()
      AND bucket.partition_table =
          'retained_response_objects_d' || to_char(bucket.delete_on, 'YYYYMMDD')
      AND inheritance.inhparent =
          to_regclass(format('%I.retained_response_objects', current_schema()))
      AND pg_get_expr(child.relpartbound, child.oid) = format(
          'FOR VALUES FROM (%L) TO (%L)', bucket.delete_on, bucket.delete_on + 1
      )
      AND retained.state = 'failed'
      AND retained.terminal_at >= $1
      AND retained.terminal_at < $2
      -- V1 proves terminal_at < delete_on, so this necessary lower
      -- bound enables daily partition pruning for the trailing window.
      AND retained.delete_on > ($1 AT TIME ZONE 'UTC')::date
      AND (cardinality($3::text[]) = 0 OR retained.model = ANY($3))
      AND retained.service_tier IS DISTINCT FROM 'background'
      AND (
          $6 = 'any'
          OR ($6 = 'include' AND (
              (retained.service_tier IS NOT NULL AND retained.service_tier = ANY($4))
              OR ($5 AND retained.service_tier IS NULL)
          ))
          OR ($6 = 'exclude' AND (
              (retained.service_tier IS NULL AND NOT $5)
              OR (retained.service_tier IS NOT NULL AND retained.service_tier <> ALL($4))
          ))
      )
      AND NOT EXISTS (
          SELECT 1 FROM requests live
          WHERE live.id = retained.object_id AND live.created_by IS NOT NULL
      )
    GROUP BY retained.model, retained.service_tier
    ) terminal_counts
    GROUP BY model, service_tier, outcome
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetainedResponseMovementError {
    CandidateIndexUnavailable,
    PartitionUnavailable,
    IntegrityMismatch,
    DatabaseFailure,
}

impl fmt::Display for RetainedResponseMovementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CandidateIndexUnavailable => "Retained response candidate index is unavailable",
            Self::PartitionUnavailable => "Retained response partition is unavailable",
            Self::IntegrityMismatch => "Retained response integrity verification failed",
            Self::DatabaseFailure => "Retained response movement failed",
        })
    }
}

impl std::error::Error for RetainedResponseMovementError {}

impl RetainedResponseMovementError {
    fn into_fusillade_error(self) -> FusilladeError {
        FusilladeError::Other(anyhow::Error::new(self))
    }
}

type MovementResult<T> = std::result::Result<T, FusilladeError>;

fn database_failure<T>(_: T) -> FusilladeError {
    RetainedResponseMovementError::DatabaseFailure.into_fusillade_error()
}

fn incomplete_graph() -> FusilladeError {
    RetainedResponseMaintenanceError::IncompleteGraph.into_fusillade_error()
}

struct Candidate {
    request_id: Uuid,
    group_id: Uuid,
    discovered_topology: GraphTopology,
}

enum MoveGraphOutcome {
    Archived {
        requests: u64,
        templates: u64,
        bytes: u64,
    },
    SkippedLocked,
    Deferred,
    AlreadyGone,
}

/// The per-call retained-payload byte budget, shared by every graph move in
/// one archive pass. Reservations are atomic so concurrent movers cannot
/// jointly exceed the budget: a graph is reserved only while the running
/// total plus its bytes still fits, or when nothing has been reserved yet
/// (the one oversized graph a pass may move, exactly as the sequential mover
/// allowed the first archived graph to exceed the budget). A reservation is
/// never released: every path after it either commits the move or returns an
/// error that aborts the whole pass.
struct ByteBudget {
    max_bytes: u64,
    reserved: std::sync::atomic::AtomicU64,
}

impl ByteBudget {
    fn new(max_bytes: u64) -> Self {
        Self {
            max_bytes,
            reserved: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// `allow_oversized` is granted only to the first candidate a pass
    /// dispatches (the oldest), so the one oversized graph a pass may move is
    /// always the head of the queue rather than whichever mover serialised
    /// its payload first; a large head can then never be starved by smaller
    /// siblings that keep winning the race.
    fn try_reserve(&self, bytes: u64, allow_oversized: bool) -> bool {
        self.reserved
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |reserved| {
                    let fits = reserved
                        .checked_add(bytes)
                        .is_some_and(|total| total <= self.max_bytes);
                    (fits || (allow_oversized && reserved == 0))
                        .then(|| reserved.saturating_add(bytes))
                },
            )
            .is_ok()
    }
}

/// The live members of a retained graph. A graph is exactly one request (its
/// template travels with it), so the topology is always that single request.
#[derive(Debug, PartialEq, Eq)]
struct GraphTopology {
    request_ids: Vec<Uuid>,
}

fn graph_topology(request_id: Uuid) -> GraphTopology {
    GraphTopology {
        request_ids: vec![request_id],
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LockSetOutcome {
    Locked,
    Skipped,
    Missing,
}

fn request_snapshot(row: &PgRow) -> MovementResult<RetainedRequestSnapshot> {
    let decode = || -> std::result::Result<RetainedRequestSnapshot, sqlx::Error> {
        Ok(RetainedRequestSnapshot {
            id: row.try_get("id")?,
            batch_id: row.try_get("batch_id")?,
            template_id: row.try_get("template_id")?,
            custom_id: row.try_get("custom_id")?,
            model: row.try_get("model")?,
            state: row.try_get("state")?,
            retry_attempt: row.try_get("retry_attempt")?,
            not_before: row.try_get("not_before")?,
            daemon_id: row.try_get("daemon_id")?,
            claimed_at: row.try_get("claimed_at")?,
            started_at: row.try_get("started_at")?,
            response_status: row.try_get("response_status")?,
            response_body: row.try_get("response_body")?,
            completed_at: row.try_get("completed_at")?,
            error: row.try_get("error")?,
            failed_at: row.try_get("failed_at")?,
            canceled_at: row.try_get("canceled_at")?,
            response_size: row.try_get("response_size")?,
            routed_model: row.try_get("routed_model")?,
            service_tier: row.try_get("service_tier")?,
            created_by: row.try_get("created_by")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    };
    decode().map_err(database_failure)
}

fn template_snapshot(row: &PgRow) -> MovementResult<RetainedTemplateSnapshot> {
    let decode = || -> std::result::Result<RetainedTemplateSnapshot, sqlx::Error> {
        Ok(RetainedTemplateSnapshot {
            id: row.try_get("id")?,
            file_id: row.try_get("file_id")?,
            custom_id: row.try_get("custom_id")?,
            endpoint: row.try_get("endpoint")?,
            method: row.try_get("method")?,
            path: row.try_get("path")?,
            body: row.try_get("body")?,
            model: row.try_get("model")?,
            api_key: row.try_get("api_key")?,
            line_number: row.try_get("line_number")?,
            body_byte_size: row.try_get("body_byte_size")?,
            metadata: row.try_get("metadata")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    };
    decode().map_err(database_failure)
}

async fn upsert_resurrection_fences(
    tx: &mut Transaction<'_, Postgres>,
    object_ids: &[Uuid],
    reason: &'static str,
    max_late_writer_seconds: Option<u64>,
) -> MovementResult<()> {
    let Some(seconds) = max_late_writer_seconds.filter(|seconds| *seconds > 0) else {
        return Err(RetainedResponseMaintenanceError::FencePolicyMissing.into_fusillade_error());
    };
    let seconds = i64::try_from(seconds).map_err(database_failure)?;
    sqlx::query(
        r#"
        WITH object_ids AS (
            SELECT DISTINCT object_id
            FROM UNNEST($1::uuid[]) AS object_id
        ), event AS MATERIALIZED (
            SELECT clock_timestamp() AS happened_at
        )
        INSERT INTO retained_response_resurrection_fences (object_id, reason, expires_at)
        SELECT object_ids.object_id, $2, event.happened_at + make_interval(secs => $3)
        FROM object_ids
        CROSS JOIN event
        ON CONFLICT (object_id) DO UPDATE
        SET reason = EXCLUDED.reason,
            expires_at = GREATEST(
                retained_response_resurrection_fences.expires_at,
                EXCLUDED.expires_at
            )
        "#,
    )
    .bind(object_ids)
    .bind(reason)
    .bind(seconds as f64)
    .execute(&mut **tx)
    .await
    .map_err(database_failure)?;
    Ok(())
}

/// Resolve a live graph seed: the group id and its request id. A group's id
/// is its request's id, so a live request seeds itself.
async fn resolve_live_group_seed(
    tx: &mut Transaction<'_, Postgres>,
    response_id: Uuid,
) -> MovementResult<Option<(Uuid, Uuid)>> {
    sqlx::query_as::<_, (Uuid, Uuid)>("SELECT id, id FROM requests WHERE id = $1")
        .bind(response_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(database_failure)
}

async fn lock_live_graph_for_erasure(
    tx: &mut Transaction<'_, Postgres>,
    response_id: Uuid,
) -> MovementResult<Option<(Uuid, GraphTopology, Vec<Uuid>)>> {
    let Some((group_id, request_id)) = resolve_live_group_seed(tx, response_id).await? else {
        return Ok(None);
    };
    let initial = graph_topology(request_id);

    let locked_requests: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM requests WHERE id = ANY($1) ORDER BY id FOR UPDATE")
            .bind(&initial.request_ids)
            .fetch_all(&mut **tx)
            .await
            .map_err(database_failure)?;
    if locked_requests != initial.request_ids {
        return Ok(None);
    }

    let Some((locked_group_id, locked_request_id)) =
        resolve_live_group_seed(tx, response_id).await?
    else {
        return Ok(None);
    };
    if locked_group_id != group_id || locked_request_id != request_id {
        return Err(incomplete_graph());
    }
    let revalidated = graph_topology(request_id);

    let request_templates: Vec<(Uuid, Option<Uuid>, bool)> = sqlx::query_as(
        r#"
        SELECT request.id,
               request.template_id,
               COALESCE(template.file_id IS NULL, FALSE) AS dedicated
        FROM requests request
        LEFT JOIN LATERAL (
            SELECT * FROM request_templates_all t WHERE t.id = request.template_id LIMIT 1
        ) template ON TRUE
        WHERE request.id = ANY($1)
        ORDER BY request.id
        "#,
    )
    .bind(&revalidated.request_ids)
    .fetch_all(&mut **tx)
    .await
    .map_err(database_failure)?;
    if request_templates.len() != revalidated.request_ids.len() {
        return Err(incomplete_graph());
    }
    let mut template_ids = request_templates
        .into_iter()
        .filter_map(|(_, template_id, dedicated)| dedicated.then_some(template_id).flatten())
        .collect::<Vec<_>>();
    template_ids.sort_unstable();
    template_ids.dedup();
    if !template_ids.is_empty() {
        let locked_templates: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM request_templates WHERE id = ANY($1) ORDER BY id FOR UPDATE",
        )
        .bind(&template_ids)
        .fetch_all(&mut **tx)
        .await
        .map_err(database_failure)?;
        if locked_templates != template_ids {
            return Err(incomplete_graph());
        }
    }
    Ok(Some((group_id, revalidated, template_ids)))
}

async fn resolve_retained_route(
    tx: &mut Transaction<'_, Postgres>,
    response_id: Uuid,
) -> MovementResult<Option<(Uuid, NaiveDate)>> {
    let routes: Vec<(Uuid, NaiveDate)> = sqlx::query_as(
        r#"
        SELECT group_id, delete_on
        FROM retained_response_group_routes
        WHERE group_id = $1
        UNION
        SELECT group_id, delete_on
        FROM retained_response_request_routes
        WHERE request_id = $1
        ORDER BY group_id, delete_on
        "#,
    )
    .bind(response_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(database_failure)?;
    match routes.as_slice() {
        [] => Ok(None),
        [route] => Ok(Some(*route)),
        _ => Err(incomplete_graph()),
    }
}

async fn lock_response_graph(
    tx: &mut Transaction<'_, Postgres>,
    group_id: Uuid,
) -> MovementResult<()> {
    sqlx::query(
        r#"
        SELECT pg_advisory_xact_lock(
            hashtextextended(
                'retained_response_graph:' || current_schema() || ':' || $1::text,
                0
            )
        )
        "#,
    )
    .bind(group_id)
    .execute(&mut **tx)
    .await
    .map_err(database_failure)?;
    Ok(())
}

async fn try_lock_response_graph(
    tx: &mut Transaction<'_, Postgres>,
    group_id: Uuid,
) -> MovementResult<bool> {
    sqlx::query_scalar(
        r#"
        SELECT pg_try_advisory_xact_lock(
            hashtextextended(
                'retained_response_graph:' || current_schema() || ':' || $1::text,
                0
            )
        )
        "#,
    )
    .bind(group_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(database_failure)
}

async fn resolve_response_graph_identity(
    tx: &mut Transaction<'_, Postgres>,
    response_id: Uuid,
) -> MovementResult<Option<Uuid>> {
    let live_group = resolve_live_group_seed(tx, response_id)
        .await?
        .map(|(group_id, _)| group_id);
    let retained_group = resolve_retained_route(tx, response_id)
        .await?
        .map(|(group_id, _)| group_id);
    match (live_group, retained_group) {
        (None, None) => Ok(None),
        (Some(group_id), None) | (None, Some(group_id)) => Ok(Some(group_id)),
        (Some(live_group), Some(retained_group)) if live_group == retained_group => {
            Ok(Some(live_group))
        }
        _ => Err(incomplete_graph()),
    }
}

async fn response_write_graph_ids(
    tx: &mut Transaction<'_, Postgres>,
    object_ids: &[Uuid],
) -> MovementResult<Vec<Uuid>> {
    let mut group_ids = Vec::with_capacity(object_ids.len());
    for object_id in object_ids {
        group_ids.push(
            resolve_response_graph_identity(tx, *object_id)
                .await?
                .unwrap_or(*object_id),
        );
    }
    group_ids.sort_unstable();
    group_ids.dedup();
    Ok(group_ids)
}

/// Begin a response lifecycle write while holding its current canonical graph
/// locks. A graph's identity can move between live and retained while this
/// task waits for its tentative lock. Re-resolving after acquisition detects
/// that change; rolling back before retrying prevents waiting for a new lock
/// while retaining the obsolete one, which would invert lock order with other
/// graph writers.
pub(crate) async fn begin_response_write_transaction(
    pool: &PgPool,
    retry_config: &crate::DbRetryConfig,
    object_ids: &[Uuid],
) -> Result<Transaction<'static, Postgres>> {
    const MAX_CANONICAL_LOCK_ATTEMPTS: usize = 8;

    for _ in 0..MAX_CANONICAL_LOCK_ATTEMPTS {
        let mut tx = crate::db::begin_transaction(pool, retry_config)
            .await
            .map_err(database_failure)?;
        let tentative_group_ids = response_write_graph_ids(&mut tx, object_ids).await?;
        for group_id in tentative_group_ids.iter().copied() {
            lock_response_graph(&mut tx, group_id).await?;
        }
        let locked_group_ids = response_write_graph_ids(&mut tx, object_ids).await?;
        if locked_group_ids == tentative_group_ids {
            return Ok(tx);
        }
        tx.rollback().await.map_err(database_failure)?;
    }

    Err(incomplete_graph())
}

async fn erase_retained_graph(
    tx: &mut Transaction<'_, Postgres>,
    group_id: Uuid,
    response_id: Uuid,
    delete_on: NaiveDate,
    expected_creator: Option<&str>,
    max_late_writer_seconds: Option<u64>,
) -> MovementResult<DeleteResponseGroupOutcome> {
    sqlx::query(
        r#"
        SELECT pg_advisory_xact_lock(
            hashtextextended(
                'retained_response_objects.partition:' || current_schema() || ':'
                    || to_char($1::date, 'YYYYMMDD'),
                0
            )
        )
        "#,
    )
    .bind(delete_on)
    .execute(&mut **tx)
    .await
    .map_err(database_failure)?;

    // This is the authoritative lifecycle check. Retirement takes the same
    // partition advisory lock before changing the bucket fence, so only an
    // exact `active` bucket observed here may reach payload SQL. A caller that
    // discovered an active route before waiting on this lock cannot race the
    // committed `retiring` transition.
    let bucket: Option<(
        String,
        String,
        String,
        sqlx::postgres::types::Oid,
        DateTime<Utc>,
    )> = sqlx::query_as(
        r#"
        SELECT bucket.state, bucket.partition_schema, bucket.partition_table,
               bucket.partition_oid, bucket.state_changed_at
        FROM retained_response_buckets bucket
        WHERE bucket.delete_on = $1
        FOR UPDATE
        "#,
    )
    .bind(delete_on)
    .fetch_optional(&mut **tx)
    .await
    .map_err(database_failure)?;
    let Some((state, schema, child, child_oid, state_changed_at)) = bucket else {
        return Err(incomplete_graph());
    };
    if schema
        != sqlx::query_scalar::<_, String>("SELECT current_schema()")
            .fetch_one(&mut **tx)
            .await
            .map_err(database_failure)?
        || child != format!("retained_response_objects_d{}", delete_on.format("%Y%m%d"))
    {
        return Err(
            RetainedResponseMaintenanceError::RetirementIdentityMismatch.into_fusillade_error()
        );
    }
    match state.as_str() {
        "retiring" => {
            let exact_pending: bool = sqlx::query_scalar(
                r#"
                SELECT EXISTS (
                    SELECT 1
                    FROM retention_partition_retirements journal
                    JOIN pg_namespace namespace
                      ON namespace.nspname = $2
                     AND namespace.oid = journal.partition_schema_oid
                    JOIN pg_class parent
                      ON parent.relnamespace = namespace.oid
                     AND parent.relname = 'retained_response_objects'
                     AND parent.oid = journal.parent_oid
                    JOIN pg_class child
                      ON child.relnamespace = namespace.oid
                     AND child.relname = $3
                     AND child.oid = $4::oid
                    WHERE journal.parent_table = 'retained_response_objects'
                      AND journal.partition_schema = $2
                      AND journal.partition_table = $3
                      AND journal.partition_oid = $4::oid
                      AND journal.lower_bound = $1
                      AND journal.upper_bound = $1 + 1
                      AND journal.completed_at IS NULL
                )
                "#,
            )
            .bind(delete_on)
            .bind(&schema)
            .bind(&child)
            .bind(child_oid)
            .fetch_one(&mut **tx)
            .await
            .map_err(database_failure)?;
            if exact_pending {
                return Ok(DeleteResponseGroupOutcome::Unavailable);
            }
            return Err(
                RetainedResponseMaintenanceError::RetirementIdentityMismatch.into_fusillade_error()
            );
        }
        "retired" => {
            let exact_completed: bool = sqlx::query_scalar(
                r#"
                SELECT EXISTS (
                    SELECT 1
                    FROM retention_partition_retirements journal
                    JOIN pg_namespace namespace
                      ON namespace.nspname = $2
                     AND namespace.oid = journal.partition_schema_oid
                    JOIN pg_class parent
                      ON parent.relnamespace = namespace.oid
                     AND parent.relname = 'retained_response_objects'
                     AND parent.oid = journal.parent_oid
                    WHERE journal.parent_table = 'retained_response_objects'
                      AND journal.partition_schema = $2
                      AND journal.partition_table = $3
                      AND journal.partition_oid = $4::oid
                      AND journal.lower_bound = $1
                      AND journal.upper_bound = $1 + 1
                      AND journal.completed_at = $5
                      AND NOT EXISTS (SELECT 1 FROM pg_class WHERE oid = $4::oid)
                )
                "#,
            )
            .bind(delete_on)
            .bind(&schema)
            .bind(&child)
            .bind(child_oid)
            .bind(state_changed_at)
            .fetch_one(&mut **tx)
            .await
            .map_err(database_failure)?;
            if exact_completed {
                return Ok(DeleteResponseGroupOutcome::Unavailable);
            }
            return Err(
                RetainedResponseMaintenanceError::RetirementIdentityMismatch.into_fusillade_error()
            );
        }
        "active" => {}
        _ => return Err(incomplete_graph()),
    }

    let bucket_valid: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM retained_response_buckets bucket
            JOIN pg_namespace namespace
              ON namespace.nspname = bucket.partition_schema
            JOIN pg_class child
              ON child.relnamespace = namespace.oid
             AND child.relname = bucket.partition_table
             AND child.oid = bucket.partition_oid
            JOIN pg_inherits inheritance
              ON inheritance.inhrelid = child.oid
             AND NOT inheritance.inhdetachpending
            WHERE bucket.delete_on = $1
              AND bucket.state = 'active'
              AND bucket.partition_schema = current_schema()
              AND bucket.partition_table =
                  'retained_response_objects_d' || to_char($1::date, 'YYYYMMDD')
              AND inheritance.inhparent =
                  to_regclass(format('%I.retained_response_objects', current_schema()))
              AND pg_get_expr(child.relpartbound, child.oid) = format(
                  'FOR VALUES FROM (%L) TO (%L)', $1::date, $1::date + 1
              )
            FOR UPDATE OF bucket
        )
        "#,
    )
    .bind(delete_on)
    .fetch_one(&mut **tx)
    .await
    .map_err(database_failure)?;
    if !bucket_valid {
        return Err(RetainedResponseMaintenanceError::IncompleteGraph.into_fusillade_error());
    }

    if let Some(expected_creator) = expected_creator {
        let owned: bool = sqlx::query_scalar(
            "SELECT COALESCE(BOOL_AND(created_by = $3), FALSE) \
             FROM retained_response_objects \
             WHERE delete_on = $1 AND group_id = $2 AND object_kind = 'request'",
        )
        .bind(delete_on)
        .bind(group_id)
        .bind(expected_creator)
        .fetch_one(&mut **tx)
        .await
        .map_err(database_failure)?;
        if !owned {
            return Err(FusilladeError::RequestNotFound(RequestId(response_id)));
        }
    }

    let group_route: Option<(Uuid, NaiveDate)> = sqlx::query_as(
        "SELECT group_id, delete_on FROM retained_response_group_routes \
         WHERE group_id = $1 ORDER BY group_id FOR UPDATE",
    )
    .bind(group_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(database_failure)?;
    if group_route != Some((group_id, delete_on)) {
        return Err(incomplete_graph());
    }
    let request_routes: Vec<(Uuid, Uuid, NaiveDate)> = sqlx::query_as(
        "SELECT request_id, group_id, delete_on FROM retained_response_request_routes \
         WHERE group_id = $1 ORDER BY request_id FOR UPDATE",
    )
    .bind(group_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(database_failure)?;

    let object_rows = sqlx::query(&format!(
        "SELECT {RETAINED_OBJECT_COLUMNS} FROM retained_response_objects object \
         WHERE object.delete_on = $1 AND object.group_id = $2 \
         ORDER BY object.object_kind, object.object_id"
    ))
    .bind(delete_on)
    .bind(group_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(database_failure)?;
    let object_rows = object_rows
        .iter()
        .map(RetainedResponseObjectRow::from_pg_row)
        .collect::<Result<Vec<_>>>()?;
    let mut headers = Vec::new();
    let mut requests = Vec::new();
    for row in &object_rows {
        if row.delete_on != delete_on || row.group_id != group_id {
            return Err(incomplete_graph());
        }
        match row.object_kind {
            RetainedObjectKind::Group => {
                headers.push(row.decode_group().map_err(|_| incomplete_graph())?)
            }
            RetainedObjectKind::Request => {
                requests.push(row.decode_request().map_err(|_| incomplete_graph())?)
            }
        }
    }
    let [header] = headers.as_mut_slice() else {
        return Err(incomplete_graph());
    };
    header.request_ids.sort_unstable();
    requests.sort_unstable_by_key(|payload| payload.request.id);
    header
        .validate_members(&requests)
        .map_err(|_| incomplete_graph())?;
    let routed_request_ids = request_routes
        .iter()
        .map(|(request_id, route_group_id, route_delete_on)| {
            if *route_group_id != group_id || *route_delete_on != delete_on {
                return Err(incomplete_graph());
            }
            Ok(*request_id)
        })
        .collect::<MovementResult<Vec<_>>>()?;
    if routed_request_ids != header.request_ids {
        return Err(incomplete_graph());
    }
    let object_count = object_rows.len() as i64;
    if object_count != 1 + header.request_ids.len() as i64 {
        return Err(incomplete_graph());
    }

    let fence_ids = std::iter::once(group_id)
        .chain(header.request_ids.iter().copied())
        .collect::<Vec<_>>();
    upsert_resurrection_fences(tx, &fence_ids, "erased", max_late_writer_seconds).await?;
    let deleted_objects =
        sqlx::query("DELETE FROM retained_response_objects WHERE delete_on = $1 AND group_id = $2")
            .bind(delete_on)
            .bind(group_id)
            .execute(&mut **tx)
            .await
            .map_err(database_failure)?
            .rows_affected();
    if deleted_objects != object_count as u64 {
        return Err(incomplete_graph());
    }
    let deleted_requests =
        sqlx::query("DELETE FROM retained_response_request_routes WHERE group_id = $1")
            .bind(group_id)
            .execute(&mut **tx)
            .await
            .map_err(database_failure)?
            .rows_affected();
    let deleted_group =
        sqlx::query("DELETE FROM retained_response_group_routes WHERE group_id = $1")
            .bind(group_id)
            .execute(&mut **tx)
            .await
            .map_err(database_failure)?
            .rows_affected();
    if deleted_requests != header.request_ids.len() as u64 || deleted_group != 1 {
        return Err(incomplete_graph());
    }
    Ok(DeleteResponseGroupOutcome::Deleted(
        header.request_ids.len() as u64,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeleteResponseGroupOutcome {
    Deleted(u64),
    Unavailable,
}

async fn delete_locked_response_group_in_transaction(
    tx: &mut Transaction<'_, Postgres>,
    response_id: Uuid,
    expected_creator: Option<&str>,
    max_late_writer_seconds: Option<u64>,
) -> Result<DeleteResponseGroupOutcome> {
    if resolve_response_graph_identity(tx, response_id)
        .await?
        .is_none()
    {
        return Err(FusilladeError::RequestNotFound(RequestId(response_id)));
    }

    if let Some((group_id, topology, template_ids)) =
        lock_live_graph_for_erasure(tx, response_id).await?
    {
        if let Some(expected_creator) = expected_creator {
            let owned: bool = sqlx::query_scalar(
                "SELECT COUNT(*) = $3 AND COALESCE(BOOL_AND(created_by = $2), FALSE) \
                 FROM requests WHERE id = ANY($1)",
            )
            .bind(&topology.request_ids)
            .bind(expected_creator)
            .bind(topology.request_ids.len() as i64)
            .fetch_one(&mut **tx)
            .await
            .map_err(database_failure)?;
            if !owned {
                return Err(FusilladeError::RequestNotFound(RequestId(response_id)));
            }
        }
        let fence_ids = std::iter::once(group_id)
            .chain(topology.request_ids.iter().copied())
            .collect::<Vec<_>>();
        upsert_resurrection_fences(tx, &fence_ids, "erased", max_late_writer_seconds).await?;
        let deleted_requests = sqlx::query("DELETE FROM requests WHERE id = ANY($1)")
            .bind(&topology.request_ids)
            .execute(&mut **tx)
            .await
            .map_err(database_failure)?
            .rows_affected();
        if !template_ids.is_empty() {
            sqlx::query(
                "DELETE FROM request_templates template \
                 WHERE template.id = ANY($1) AND template.file_id IS NULL \
                   AND NOT EXISTS (SELECT 1 FROM requests WHERE template_id = template.id)",
            )
            .bind(&template_ids)
            .execute(&mut **tx)
            .await
            .map_err(database_failure)?;
        }
        if deleted_requests != topology.request_ids.len() as u64 {
            return Err(incomplete_graph());
        }
        // A late writer may have recreated the live graph after it moved, so
        // the retained copy can coexist with the live one. Erasure must take
        // both, or the retained snapshot resurfaces once the live row is gone.
        if let Some((retained_group_id, delete_on)) =
            resolve_retained_route(tx, response_id).await?
        {
            erase_retained_graph(
                tx,
                retained_group_id,
                response_id,
                delete_on,
                expected_creator,
                max_late_writer_seconds,
            )
            .await?;
        }
        return Ok(DeleteResponseGroupOutcome::Deleted(deleted_requests));
    }

    if let Some((group_id, delete_on)) = resolve_retained_route(tx, response_id).await? {
        return erase_retained_graph(
            tx,
            group_id,
            response_id,
            delete_on,
            expected_creator,
            max_late_writer_seconds,
        )
        .await;
    }

    Err(FusilladeError::RequestNotFound(RequestId(response_id)))
}

pub(crate) async fn delete_response_group<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    response_id: Uuid,
) -> Result<u64> {
    let mut tx = manager.begin_response_write(&[response_id]).await?;
    let deleted = delete_locked_response_group_in_transaction(
        &mut tx,
        response_id,
        None,
        manager.retained_response_fence_seconds(),
    )
    .await?;
    tx.commit().await.map_err(database_failure)?;
    Ok(match deleted {
        DeleteResponseGroupOutcome::Deleted(deleted) => deleted,
        DeleteResponseGroupOutcome::Unavailable => 0,
    })
}

pub(crate) async fn delete_owned_response_group<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    response_id: Uuid,
    creator_id: &str,
) -> Result<u64> {
    let mut tx = manager.begin_response_write(&[response_id]).await?;
    let deleted = delete_locked_response_group_in_transaction(
        &mut tx,
        response_id,
        Some(creator_id),
        manager.retained_response_fence_seconds(),
    )
    .await?;
    tx.commit().await.map_err(database_failure)?;
    match deleted {
        DeleteResponseGroupOutcome::Deleted(deleted) => Ok(deleted),
        DeleteResponseGroupOutcome::Unavailable => {
            Err(FusilladeError::RequestNotFound(RequestId(response_id)))
        }
    }
}

pub(crate) async fn delete_creator_response_groups(
    tx: &mut Transaction<'_, Postgres>,
    creator_id: &str,
    group_limit: i64,
    max_late_writer_seconds: Option<u64>,
) -> Result<u64> {
    let candidates: Vec<(Uuid, Uuid, DateTime<Utc>)> = sqlx::query_as(
        r#"
        WITH live_seeds AS MATERIALIZED (
            SELECT request.id, request.created_at
            FROM requests request
            WHERE request.batch_id IS NULL
              AND request.created_by = $1
            ORDER BY request.created_at, request.id
            LIMIT $2
        ), live_members AS (
            SELECT seed.id AS group_id,
                   seed.id AS response_id,
                   seed.created_at
            FROM live_seeds seed
        ), retained_members AS MATERIALIZED (
            SELECT object.group_id, object.object_id AS response_id,
                   object.created_at
            FROM retained_response_objects object
            JOIN retained_response_buckets bucket
              ON bucket.delete_on = object.delete_on
             AND bucket.state = 'active'
            WHERE object.object_kind = 'request'
              AND object.created_by = $1
            ORDER BY object.created_at, object.object_id
            LIMIT $2
        ), bounded_members AS (
            SELECT * FROM live_members
            UNION ALL
            SELECT * FROM retained_members
        )
        SELECT group_id,
               (ARRAY_AGG(response_id ORDER BY response_id))[1] AS response_id,
               MIN(created_at) AS sort_at
        FROM bounded_members
        GROUP BY group_id
        ORDER BY sort_at, group_id
        LIMIT $2
        "#,
    )
    .bind(creator_id)
    .bind(group_limit)
    .fetch_all(&mut **tx)
    .await
    .map_err(database_failure)?;

    let mut progress = 0_u64;
    for (group_id, response_id, _) in candidates {
        if !try_lock_response_graph(tx, group_id).await? {
            // This is a loop-termination signal, not a deletion count. A busy
            // candidate proves work remains and prevents a concurrent purge
            // caller from treating temporary lock contention as completion.
            progress += 1;
            continue;
        }
        match resolve_response_graph_identity(tx, response_id).await? {
            Some(locked_group_id) if locked_group_id == group_id => {}
            // Candidate discovery raced a lifecycle/topology change. The
            // creator path is deliberately non-blocking, so do not act under
            // the stale lock or wait for a second graph while retaining it.
            Some(_) | None => {
                progress += 1;
                continue;
            }
        }
        match delete_locked_response_group_in_transaction(
            tx,
            response_id,
            Some(creator_id),
            max_late_writer_seconds,
        )
        .await
        {
            Ok(DeleteResponseGroupOutcome::Deleted(deleted)) => progress += deleted,
            Ok(DeleteResponseGroupOutcome::Unavailable) => {
                // Retirement owns the payload now, but it has not yet proved
                // physical deletion. The at-least-once creator-erasure job
                // must retry instead of certifying this graph as erased.
                return Err(
                    RetainedResponseMaintenanceError::RetirementPending.into_fusillade_error()
                );
            }
            Err(FusilladeError::RequestNotFound(_)) => {
                if resolve_response_graph_identity(tx, response_id)
                    .await?
                    .is_none()
                {
                    // A concurrent lifecycle action completed after candidate
                    // discovery. Count that disappearance as forward progress.
                    progress += 1;
                } else {
                    return Err(incomplete_graph());
                }
            }
            Err(error) => return Err(error),
        }
    }
    if progress == 0 {
        let retirement_pending: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS (
                SELECT 1
                FROM retention_partition_retirements journal
                WHERE journal.parent_table = 'retained_response_objects'
                  AND journal.completed_at IS NULL
            ) OR EXISTS (
                SELECT 1
                FROM retained_response_buckets bucket
                WHERE bucket.state = 'retiring'
                  AND NOT EXISTS (
                      SELECT 1
                      FROM retention_partition_retirements journal
                      WHERE journal.parent_table = 'retained_response_objects'
                        AND journal.partition_schema = bucket.partition_schema
                        AND journal.partition_table = bucket.partition_table
                        AND journal.partition_oid = bucket.partition_oid
                        AND journal.lower_bound = bucket.delete_on
                        AND journal.upper_bound = bucket.delete_on + 1
                        AND journal.completed_at IS NULL
                  )
            )
            "#,
        )
        .fetch_one(&mut **tx)
        .await
        .map_err(database_failure)?;
        if retirement_pending {
            return Err(RetainedResponseMaintenanceError::RetirementPending.into_fusillade_error());
        }
    }
    Ok(progress)
}

async fn selected_graph_is_absent(
    tx: &mut Transaction<'_, Postgres>,
    candidate: &Candidate,
) -> MovementResult<bool> {
    sqlx::query_scalar("SELECT NOT EXISTS (SELECT 1 FROM requests WHERE id = ANY($1))")
        .bind(&candidate.discovered_topology.request_ids)
        .fetch_one(&mut **tx)
        .await
        .map_err(database_failure)
}

async fn lock_requests(
    tx: &mut Transaction<'_, Postgres>,
    request_ids: &[Uuid],
) -> MovementResult<LockSetOutcome> {
    let locked: Vec<Uuid> = sqlx::query_scalar(
        r#"
        SELECT id
        FROM requests
        WHERE id = ANY($1)
        ORDER BY id
        FOR UPDATE SKIP LOCKED
        "#,
    )
    .bind(request_ids)
    .fetch_all(&mut **tx)
    .await
    .map_err(database_failure)?;
    if locked == request_ids {
        return Ok(LockSetOutcome::Locked);
    }
    let existing: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM requests WHERE id = ANY($1)")
        .bind(request_ids)
        .fetch_one(&mut **tx)
        .await
        .map_err(database_failure)?;
    if existing != request_ids.len() as i64 {
        return Ok(LockSetOutcome::Missing);
    }
    Ok(LockSetOutcome::Skipped)
}

fn canonical_payload_bytes(rows: &[RetainedResponseObjectRow]) -> MovementResult<Vec<u8>> {
    let mut bytes = Vec::new();
    for row in rows {
        let payload = serde_json::to_vec(&row.payload)
            .map_err(|_| RetainedResponseMovementError::IntegrityMismatch.into_fusillade_error())?;
        bytes.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&payload);
    }
    Ok(bytes)
}

fn row_metadata_matches(
    left: &RetainedResponseObjectRow,
    right: &RetainedResponseObjectRow,
) -> bool {
    left.delete_on == right.delete_on
        && left.group_id == right.group_id
        && left.object_kind == right.object_kind
        && left.object_id == right.object_id
        && left.request_id == right.request_id
        && left.created_by == right.created_by
        && left.service_tier == right.service_tier
        && left.state == right.state
        && left.model == right.model
        && left.created_at == right.created_at
        && left.terminal_at == right.terminal_at
        && left.schema_version == right.schema_version
}

async fn lock_active_partition(
    tx: &mut Transaction<'_, Postgres>,
    delete_on: NaiveDate,
) -> MovementResult<bool> {
    sqlx::query(
        r#"
        -- SHARED: movers only need to exclude retirement, which fences a day
        -- under the exclusive form of this same key (the retirement claim);
        -- they never need to exclude each other. Concurrent moves into one
        -- day are safe (distinct graphs, primary keys on every object and
        -- route), and this is what lets the backfill fan-out overlap the
        -- write phase instead of queueing on the day.
        SELECT pg_advisory_xact_lock_shared(
            hashtextextended(
                'retained_response_objects.partition:' || current_schema() || ':'
                    || to_char($1::date, 'YYYYMMDD'),
                0
            )
        )
        "#,
    )
    .bind(delete_on)
    .execute(&mut **tx)
    .await
    .map_err(database_failure)?;

    sqlx::query_scalar::<_, NaiveDate>(
        r#"
        SELECT bucket.delete_on
        FROM retained_response_buckets bucket
        JOIN pg_namespace namespace
          ON namespace.nspname = bucket.partition_schema
        JOIN pg_class child
          ON child.relnamespace = namespace.oid
         AND child.relname = bucket.partition_table
         AND child.oid = bucket.partition_oid
        JOIN pg_inherits inheritance
          ON inheritance.inhrelid = child.oid
         AND NOT inheritance.inhdetachpending
        WHERE bucket.delete_on = $1
          AND bucket.state = 'active'
          AND bucket.partition_schema = current_schema()
          AND bucket.partition_table =
              'retained_response_objects_d' || to_char($1::date, 'YYYYMMDD')
          AND inheritance.inhparent =
              to_regclass(format('%I.retained_response_objects', current_schema()))
          AND pg_get_expr(child.relpartbound, child.oid) = format(
              'FOR VALUES FROM (%L) TO (%L)', $1::date, $1::date + 1
        )
        FOR SHARE OF bucket
        "#,
    )
    .bind(delete_on)
    .fetch_optional(&mut **tx)
    .await
    .map_err(database_failure)
    .map(|bucket| bucket.is_some())
}

async fn insert_and_verify_objects(
    tx: &mut Transaction<'_, Postgres>,
    delete_on: NaiveDate,
    group_id: Uuid,
    expected: &[RetainedResponseObjectRow],
) -> MovementResult<()> {
    let request_ids = expected
        .iter()
        .filter(|row| row.object_kind == RetainedObjectKind::Request)
        .map(|row| row.object_id)
        .collect::<Vec<_>>();
    let conflicting_bucket: bool = sqlx::query_scalar(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM retained_response_objects
            WHERE delete_on <> $1
              AND (
                    (object_kind = 'group' AND object_id = $2)
                 OR (object_kind = 'request' AND object_id = ANY($3))
              )
        )
        "#,
    )
    .bind(delete_on)
    .bind(group_id)
    .bind(&request_ids)
    .fetch_one(&mut **tx)
    .await
    .map_err(database_failure)?;
    if conflicting_bucket {
        return Err(RetainedResponseMovementError::IntegrityMismatch.into_fusillade_error());
    }

    for row in expected {
        sqlx::query(
            r#"
            INSERT INTO retained_response_objects (
                delete_on, group_id, object_kind, object_id, request_id,
                created_by, service_tier, state, model,
                created_at, terminal_at, schema_version, payload
            ) VALUES (
                $1, $2, $3, $4, $5,
                $6, $7, $8, $9,
                $10, $11, $12, $13
            )
            ON CONFLICT (delete_on, object_kind, object_id) DO NOTHING
            "#,
        )
        .bind(row.delete_on)
        .bind(row.group_id)
        .bind(row.object_kind.as_str())
        .bind(row.object_id)
        .bind(row.request_id)
        .bind(&row.created_by)
        .bind(&row.service_tier)
        .bind(&row.state)
        .bind(&row.model)
        .bind(row.created_at)
        .bind(row.terminal_at)
        .bind(row.schema_version)
        .bind(&row.payload)
        .execute(&mut **tx)
        .await
        .map_err(database_failure)?;
    }

    let stored = sqlx::query(
        r#"
        SELECT delete_on, group_id, object_kind, object_id, request_id,
               created_by, service_tier, state, model,
               created_at, terminal_at, schema_version, payload
        FROM retained_response_objects
        WHERE delete_on = $1 AND group_id = $2
        ORDER BY CASE object_kind
                     WHEN 'group' THEN 0
                     WHEN 'request' THEN 1
                     ELSE 2
                 END,
                 object_id
        "#,
    )
    .bind(delete_on)
    .bind(group_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(database_failure)?;
    let mut actual = Vec::with_capacity(stored.len());
    for row in &stored {
        actual.push(RetainedResponseObjectRow::from_pg_row(row).map_err(|_| {
            RetainedResponseMovementError::IntegrityMismatch.into_fusillade_error()
        })?);
    }
    if actual.len() != expected.len()
        || actual
            .iter()
            .zip(expected)
            .any(|(actual, expected)| !row_metadata_matches(actual, expected))
    {
        return Err(RetainedResponseMovementError::IntegrityMismatch.into_fusillade_error());
    }

    let expected_bytes = canonical_payload_bytes(expected)?;
    let actual_bytes = canonical_payload_bytes(&actual)?;
    // Both byte strings are already in memory (the read-back row was decoded
    // locally), so compare them directly rather than shipping each payload
    // back to the server to be hashed there.
    if expected_bytes != actual_bytes {
        return Err(RetainedResponseMovementError::IntegrityMismatch.into_fusillade_error());
    }
    Ok(())
}

async fn insert_and_verify_routes(
    tx: &mut Transaction<'_, Postgres>,
    delete_on: NaiveDate,
    group_id: Uuid,
    request_ids: &[Uuid],
) -> MovementResult<()> {
    sqlx::query(
        r#"
        INSERT INTO retained_response_group_routes (group_id, delete_on)
        VALUES ($1, $2)
        ON CONFLICT (group_id) DO NOTHING
        "#,
    )
    .bind(group_id)
    .bind(delete_on)
    .execute(&mut **tx)
    .await
    .map_err(database_failure)?;
    for request_id in request_ids {
        sqlx::query(
            r#"
            INSERT INTO retained_response_request_routes (request_id, group_id, delete_on)
            VALUES ($1, $2, $3)
            ON CONFLICT (request_id) DO NOTHING
            "#,
        )
        .bind(request_id)
        .bind(group_id)
        .bind(delete_on)
        .execute(&mut **tx)
        .await
        .map_err(database_failure)?;
    }

    let group_route: Option<NaiveDate> = sqlx::query_scalar(
        "SELECT delete_on FROM retained_response_group_routes WHERE group_id = $1",
    )
    .bind(group_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(database_failure)?;
    if group_route != Some(delete_on) {
        return Err(RetainedResponseMovementError::IntegrityMismatch.into_fusillade_error());
    }
    let request_routes: Vec<(Uuid, Uuid, NaiveDate)> = sqlx::query_as(
        r#"
        SELECT request_id, group_id, delete_on
        FROM retained_response_request_routes
        WHERE request_id = ANY($1) OR group_id = $2
        ORDER BY request_id
        "#,
    )
    .bind(request_ids)
    .bind(group_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(database_failure)?;
    if request_routes.len() != request_ids.len()
        || request_routes.iter().zip(request_ids).any(
            |((route_request_id, route_group_id, route_delete_on), request_id)| {
                route_request_id != request_id
                    || *route_group_id != group_id
                    || *route_delete_on != delete_on
            },
        )
    {
        return Err(RetainedResponseMovementError::IntegrityMismatch.into_fusillade_error());
    }
    Ok(())
}

async fn move_graph<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    candidate: Candidate,
    policy: &RetentionPolicy,
    cutoffs: &RetainedResponseArchiveCutoffs,
    budget: &ByteBudget,
    allow_oversized: bool,
) -> MovementResult<MoveGraphOutcome> {
    let mut tx = manager.begin_write().await.map_err(database_failure)?;
    // A group's id is its request's id; the discovered candidate must agree.
    let group_id = candidate.request_id;
    if candidate.group_id != group_id {
        return Err(incomplete_graph());
    }
    if !try_lock_response_graph(&mut tx, group_id).await? {
        return Ok(MoveGraphOutcome::SkippedLocked);
    }
    let topology = graph_topology(candidate.request_id);
    if topology != candidate.discovered_topology {
        return Err(incomplete_graph());
    }

    match lock_requests(&mut tx, &topology.request_ids).await? {
        LockSetOutcome::Locked => {}
        LockSetOutcome::Skipped => return Ok(MoveGraphOutcome::SkippedLocked),
        LockSetOutcome::Missing => {
            if selected_graph_is_absent(&mut tx, &candidate).await? {
                return Ok(MoveGraphOutcome::AlreadyGone);
            }
            return Err(incomplete_graph());
        }
    }
    let request_ids = topology.request_ids;

    let request_rows = sqlx::query(
        r#"
        SELECT id, batch_id, template_id, custom_id, model, state,
               retry_attempt, not_before, daemon_id, claimed_at, started_at,
               response_status, response_body, completed_at, error, failed_at,
               canceled_at, response_size, routed_model, service_tier,
               created_by, created_at, updated_at
        FROM requests
        WHERE id = ANY($1)
        ORDER BY id
        "#,
    )
    .bind(&request_ids)
    .fetch_all(&mut *tx)
    .await
    .map_err(database_failure)?;
    if request_rows.len() != request_ids.len() {
        return Err(incomplete_graph());
    }
    let requests = request_rows
        .iter()
        .map(request_snapshot)
        .collect::<MovementResult<Vec<_>>>()?;

    let mut template_ids = requests
        .iter()
        .map(|request| request.template_id.ok_or_else(incomplete_graph))
        .collect::<MovementResult<Vec<_>>>()?;
    template_ids.sort_unstable();
    if template_ids.windows(2).any(|ids| ids[0] == ids[1]) {
        return Err(incomplete_graph());
    }
    let template_rows = sqlx::query(
        r#"
        SELECT id, file_id, custom_id, endpoint, method, path, body, model,
               api_key, line_number, body_byte_size, metadata, created_at, updated_at
        FROM request_templates
        WHERE id = ANY($1)
        ORDER BY id
        FOR UPDATE SKIP LOCKED
        "#,
    )
    .bind(&template_ids)
    .fetch_all(&mut *tx)
    .await
    .map_err(database_failure)?;
    if template_rows.len() != template_ids.len() {
        let existing: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM request_templates WHERE id = ANY($1)")
                .bind(&template_ids)
                .fetch_one(&mut *tx)
                .await
                .map_err(database_failure)?;
        if existing == template_ids.len() as i64 {
            return Ok(MoveGraphOutcome::SkippedLocked);
        }
        return Err(incomplete_graph());
    }
    let templates = template_rows
        .iter()
        .map(template_snapshot)
        .collect::<MovementResult<Vec<_>>>()?;
    // A template is owned by this graph only when it is dedicated (no file)
    // and nothing outside the graph references it; owned templates move with
    // the graph and are deleted. Anything else — a file's template (the shape
    // of the oldest legacy batchless traffic) or a dedicated template shared
    // with another live request — is external input content governed by its
    // own lifecycle: it is snapshotted into the retained record so the record
    // stays self-describing, and its live row is left untouched.
    let externally_referenced: Vec<Uuid> = sqlx::query_scalar(
        r#"
        SELECT DISTINCT template_id
        FROM requests
        WHERE template_id = ANY($1) AND NOT (id = ANY($2))
        "#,
    )
    .bind(&template_ids)
    .bind(&request_ids)
    .fetch_all(&mut *tx)
    .await
    .map_err(database_failure)?;
    let mut owned_template_ids: Vec<Uuid> = templates
        .iter()
        .filter(|template| {
            template.file_id.is_none() && !externally_referenced.contains(&template.id)
        })
        .map(|template| template.id)
        .collect();
    owned_template_ids.sort_unstable();

    let mut delete_on = None;
    for request in &requests {
        if request.batch_id.is_some() {
            return Err(incomplete_graph());
        }
        let Some(tier) = request.service_tier.as_deref() else {
            return Ok(MoveGraphOutcome::Deferred);
        };
        let Some(seconds) = policy.batchless_seconds_by_service_tier.get(tier).copied() else {
            return Ok(MoveGraphOutcome::Deferred);
        };
        let Some(terminal_at) = request.terminal_at().map_err(|_| incomplete_graph())? else {
            return Err(incomplete_graph());
        };
        if request.state == "canceled"
            && request.claimed_at.is_some()
            && terminal_at > cutoffs.cancel_grace_before()
        {
            return Ok(MoveGraphOutcome::Deferred);
        }
        let retention_anchor = terminal_at.max(request.created_at);
        if retention_anchor > cutoffs.terminal_before() {
            return Ok(MoveGraphOutcome::Deferred);
        }
        let request_delete_on = RetentionPolicy::delete_on(retention_anchor, seconds)
            .map_err(|_| incomplete_graph())?;
        delete_on = Some(delete_on.map_or(request_delete_on, |current: NaiveDate| {
            current.max(request_delete_on)
        }));
    }
    let delete_on = delete_on.ok_or_else(incomplete_graph)?;
    // A graph already past its retention period cannot land on a droppable
    // day: a partition never drops on its own day and past days have no
    // partition. Deferring it would leave it in the live heap forever, so
    // it goes into the earliest future day instead. Retention is only ever
    // extended here, by at most the time the graph spent overdue, and the
    // content becomes droppable at the next boundary rather than never.
    let earliest_future = cutoffs
        .observed_at()
        .date_naive()
        .succ_opt()
        .ok_or_else(incomplete_graph)?;
    let delete_on = delete_on.max(earliest_future);

    let template_by_id = templates
        .into_iter()
        .map(|template| (template.id, template))
        .collect::<std::collections::HashMap<_, _>>();
    let request_payloads = requests
        .into_iter()
        .map(|request| {
            let template_id = request.template_id.ok_or_else(incomplete_graph)?;
            let template = template_by_id
                .get(&template_id)
                .cloned()
                .ok_or_else(incomplete_graph)?;
            Ok(RetainedRequestPayloadV1 { request, template })
        })
        .collect::<MovementResult<Vec<_>>>()?;
    let group = RetainedGroup {
        group_id,
        request_ids: request_ids.clone(),
    };
    let objects = RetainedGroup::compose_rows(delete_on, group, request_payloads)
        .map_err(|_| incomplete_graph())?;
    let payload_bytes = objects.iter().try_fold(0_u64, |total, row| {
        let bytes = serde_json::to_vec(&row.payload)
            .map_err(|_| RetainedResponseMovementError::IntegrityMismatch.into_fusillade_error())?;
        total
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| RetainedResponseMovementError::IntegrityMismatch.into_fusillade_error())
    })?;
    if !budget.try_reserve(payload_bytes, allow_oversized) {
        return Ok(MoveGraphOutcome::Deferred);
    }
    if !lock_active_partition(&mut tx, delete_on).await? {
        return Err(RetainedResponseMovementError::PartitionUnavailable.into_fusillade_error());
    }

    insert_and_verify_objects(&mut tx, delete_on, group_id, &objects).await?;
    insert_and_verify_routes(&mut tx, delete_on, group_id, &request_ids).await?;

    let fence_ids = std::iter::once(group_id)
        .chain(request_ids.iter().copied())
        .collect::<Vec<_>>();
    upsert_resurrection_fences(
        &mut tx,
        &fence_ids,
        "archived",
        policy.max_late_writer_seconds,
    )
    .await?;

    let deleted_requests = sqlx::query("DELETE FROM requests WHERE id = ANY($1)")
        .bind(&request_ids)
        .execute(&mut *tx)
        .await
        .map_err(database_failure)?
        .rows_affected();
    if deleted_requests != request_ids.len() as u64 {
        return Err(RetainedResponseMovementError::IntegrityMismatch.into_fusillade_error());
    }
    let deleted_templates =
        sqlx::query("DELETE FROM request_templates WHERE id = ANY($1) AND file_id IS NULL")
            .bind(&owned_template_ids)
            .execute(&mut *tx)
            .await
            .map_err(database_failure)?
            .rows_affected();
    if deleted_templates != owned_template_ids.len() as u64 {
        return Err(RetainedResponseMovementError::IntegrityMismatch.into_fusillade_error());
    }

    let live_members: i64 = sqlx::query_scalar(
        r#"
        SELECT
            (SELECT COUNT(*) FROM requests WHERE id = ANY($1))
          + (SELECT COUNT(*) FROM request_templates WHERE id = ANY($2))
        "#,
    )
    .bind(&request_ids)
    .bind(&owned_template_ids)
    .fetch_one(&mut *tx)
    .await
    .map_err(database_failure)?;
    if live_members != 0 {
        return Err(RetainedResponseMovementError::IntegrityMismatch.into_fusillade_error());
    }
    tx.commit().await.map_err(database_failure)?;
    Ok(MoveGraphOutcome::Archived {
        requests: request_ids.len() as u64,
        templates: owned_template_ids.len() as u64,
        bytes: payload_bytes,
    })
}

// The per-tier lower bound is the first terminal instant whose request-level
// deletion day can still be in the future. This keeps an arbitrary due legacy
// backlog outside the bounded probe budget while the lock-time checks remain
// authoritative. A graph is one request, so its group id is its request id.
const CANDIDATE_DISCOVERY_SQL: &str = r#"
        WITH policy(service_tier, archive_after) AS (
            SELECT * FROM UNNEST($1::text[], $2::timestamptz[])
        ),
        candidate_seed AS MATERIALIZED (
            SELECT candidate.id AS request_id,
                   candidate.id AS group_id,
                   candidate.terminal_at
            FROM policy
            CROSS JOIN LATERAL (
                SELECT request.id,
                       CASE request.state
                           WHEN 'completed' THEN request.completed_at
                           WHEN 'failed' THEN request.failed_at
                           WHEN 'canceled' THEN request.canceled_at
                       END AS terminal_at
                FROM requests request
                WHERE request.service_tier = policy.service_tier
                  AND request.batch_id IS NULL
                  AND request.state IN ('completed', 'failed', 'canceled')
                  AND NOT (request.id = ANY($4))
                  AND CASE request.state
                          WHEN 'completed' THEN request.completed_at
                          WHEN 'failed' THEN request.failed_at
                          WHEN 'canceled' THEN request.canceled_at
                      END >= policy.archive_after
                  AND CASE request.state
                          WHEN 'completed' THEN request.completed_at
                          WHEN 'failed' THEN request.failed_at
                          WHEN 'canceled' THEN request.canceled_at
                      END <= $3
                ORDER BY CASE request.state
                             WHEN 'completed' THEN request.completed_at
                             WHEN 'failed' THEN request.failed_at
                             WHEN 'canceled' THEN request.canceled_at
                         END,
                         request.id
                LIMIT 1
            ) candidate
            ORDER BY candidate.terminal_at, candidate.id
            LIMIT 1
        )
        SELECT candidate.request_id, candidate.group_id
        FROM candidate_seed candidate
        "#;

async fn next_candidate<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    tiers: &[String],
    archive_after: &[DateTime<Utc>],
    terminal_before: DateTime<Utc>,
    excluded_request_ids: &[Uuid],
) -> MovementResult<Option<Candidate>> {
    let row: Option<(Uuid, Uuid)> = sqlx::query_as(CANDIDATE_DISCOVERY_SQL)
        .bind(tiers)
        .bind(archive_after)
        .bind(terminal_before)
        .bind(excluded_request_ids)
        .fetch_optional(manager.read_executor())
        .await
        .map_err(database_failure)?;
    let Some((request_id, group_id)) = row else {
        return Ok(None);
    };
    if group_id != request_id {
        return Err(incomplete_graph());
    }
    Ok(Some(Candidate {
        request_id,
        group_id,
        discovered_topology: graph_topology(request_id),
    }))
}

pub(crate) async fn archive_terminal_batchless_responses<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    policy: &RetentionPolicy,
    cutoffs: &RetainedResponseArchiveCutoffs,
    max_groups: i64,
    max_bytes: i64,
) -> Result<RetainedResponseArchiveOutcome> {
    archive_batchless_responses(manager, policy, cutoffs, max_groups, max_bytes, false, 1).await
}

/// The gated legacy path: no per-tier lower bound, so already-due graphs are
/// discovered oldest-first; `move_graph` lands them on the day after
/// observation. Up to `concurrency` discovered graphs move at the same time.
pub(crate) async fn archive_overdue_batchless_responses<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    policy: &RetentionPolicy,
    cutoffs: &RetainedResponseArchiveCutoffs,
    max_groups: i64,
    max_bytes: i64,
    concurrency: usize,
) -> Result<RetainedResponseArchiveOutcome> {
    archive_batchless_responses(
        manager,
        policy,
        cutoffs,
        max_groups,
        max_bytes,
        true,
        concurrency,
    )
    .await
}

// Concurrency lives here, below discovery, rather than as N whole passes run
// side by side by the daemon. Discovery is an unlocked oldest-first read, so
// N simultaneous passes would all discover the same head of the queue and
// then merely take turns on it through the advisory locks: safe, but no
// faster than one pass. One serial discovery followed by concurrent moves
// over a candidate set of distinct graphs (deduplicated by group, with every
// discovered member excluded from later probes) gives each mover its own
// graph. Each move is still its own transaction with the per-graph advisory
// lock, `FOR UPDATE SKIP LOCKED`, and read-back verification, so it stays
// correct against any other mover — another pass, another pod, or a late
// writer — exactly as before. The shared byte budget is reserved atomically.
async fn archive_batchless_responses<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    policy: &RetentionPolicy,
    cutoffs: &RetainedResponseArchiveCutoffs,
    max_groups: i64,
    max_bytes: i64,
    include_overdue: bool,
    concurrency: usize,
) -> Result<RetainedResponseArchiveOutcome> {
    if max_groups <= 0 || max_bytes <= 0 {
        return Ok(RetainedResponseArchiveOutcome::default());
    }
    policy.validate().map_err(FusilladeError::ValidationError)?;
    if policy.batchless_seconds_by_service_tier.is_empty() {
        return Ok(RetainedResponseArchiveOutcome::default());
    }
    let index_ready: bool =
        sqlx::query_scalar("SELECT retained_response_archive_index_ready(current_schema())")
            .fetch_one(manager.read_executor())
            .await
            .map_err(database_failure)?;
    if !index_ready {
        return Err(RetainedResponseMovementError::CandidateIndexUnavailable.into_fusillade_error());
    }
    let observed_day_start = cutoffs
        .observed_at()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("a date always has a UTC midnight")
        .and_utc();
    let mut policy_tiers = policy
        .batchless_seconds_by_service_tier
        .iter()
        .collect::<Vec<_>>();
    policy_tiers.sort_unstable_by_key(|(tier, _)| *tier);
    let mut tiers = Vec::with_capacity(policy_tiers.len());
    let mut archive_after = Vec::with_capacity(policy_tiers.len());
    for (tier, seconds) in policy_tiers {
        tiers.push(tier.clone());
        let seconds = i64::try_from(*seconds).map_err(|_| {
            FusilladeError::ValidationError("automated retention period is too large".to_owned())
        })?;
        let duration = chrono::Duration::try_seconds(seconds).ok_or_else(|| {
            FusilladeError::ValidationError("automated retention period is too large".to_owned())
        })?;
        let lower_bound = observed_day_start
            .checked_sub_signed(duration)
            .ok_or_else(|| {
                FusilladeError::ValidationError(
                    "automated retention cutoff is out of range".to_owned(),
                )
            })?;
        // The overdue path has no lower bound: everything terminal before the
        // dwell cutoff is discoverable, oldest first. No request predates the
        // Unix epoch, and the epoch is comfortably inside the timestamptz range.
        archive_after.push(if include_overdue {
            DateTime::<Utc>::UNIX_EPOCH
        } else {
            lower_bound
        });
    }
    let candidate_limit = max_groups.saturating_add(1);
    let max_probes = candidate_limit.saturating_mul(2);
    let mut candidates = Vec::new();
    let mut seen_groups = std::collections::HashSet::new();
    let mut excluded_request_ids = Vec::new();
    let mut discovery_exhausted = false;
    for _ in 0..max_probes {
        if candidates.len() as i64 == candidate_limit {
            break;
        }
        let Some(candidate) = next_candidate(
            manager,
            &tiers,
            &archive_after,
            cutoffs.terminal_before(),
            &excluded_request_ids,
        )
        .await?
        else {
            discovery_exhausted = true;
            break;
        };
        excluded_request_ids.extend(candidate.discovered_topology.request_ids.iter().copied());
        excluded_request_ids.sort_unstable();
        excluded_request_ids.dedup();
        if seen_groups.insert(candidate.group_id) {
            candidates.push(candidate);
        }
    }
    let mut outcome = RetainedResponseArchiveOutcome {
        may_have_more: candidates.len() as i64 > max_groups || !discovery_exhausted,
        ..Default::default()
    };
    let budget = ByteBudget::new(max_bytes as u64);
    let concurrency = concurrency.max(1);
    let max_groups = max_groups as u64;
    let mut pending = candidates.into_iter();
    let mut in_flight = futures::stream::FuturesUnordered::new();
    let mut dispatched = 0_u64;
    loop {
        // Dispatch only while an archived result from every in-flight move
        // would still fit under the graph budget, so the pass can never
        // commit more than `max_groups` graphs; the spare discovered
        // candidate is dispatched only once an earlier move did not archive.
        while in_flight.len() < concurrency
            && outcome.groups_archived + in_flight.len() as u64 != max_groups
        {
            let Some(candidate) = pending.next() else {
                break;
            };
            in_flight.push(move_graph(
                manager,
                candidate,
                policy,
                cutoffs,
                &budget,
                dispatched == 0,
            ));
            dispatched += 1;
        }
        let Some(result) = futures::StreamExt::next(&mut in_flight).await else {
            break;
        };
        let moved = match result {
            Ok(moved) => moved,
            Err(error)
                if RetainedResponseMaintenanceError::from_fusillade_error(&error)
                    == Some(RetainedResponseMaintenanceError::IncompleteGraph) =>
            {
                // Left live for a human to look at; the pass carries on so
                // one malformed graph at the head of the queue cannot stall
                // the worker forever.
                outcome.incomplete_skipped += 1;
                outcome.may_have_more = true;
                continue;
            }
            // Returning drops the other in-flight moves; their uncommitted
            // transactions roll back when the connections return to the pool.
            // Graphs this pass already committed stay archived but are not
            // counted by the caller, so say so (content-free).
            Err(error) => {
                if outcome.groups_archived > 0 {
                    tracing::warn!(
                        groups_committed_before_failure = outcome.groups_archived,
                        "Retained-response archive pass aborted after some graphs had committed"
                    );
                }
                return Err(error);
            }
        };
        match moved {
            MoveGraphOutcome::Archived {
                requests,
                templates,
                bytes,
            } => {
                outcome.groups_archived += 1;
                outcome.requests_archived += requests;
                outcome.templates_archived += templates;
                outcome.bytes_archived += bytes;
            }
            MoveGraphOutcome::SkippedLocked => {
                outcome.skipped_locked = true;
                outcome.may_have_more = true;
            }
            MoveGraphOutcome::Deferred => {
                outcome.may_have_more = true;
            }
            MoveGraphOutcome::AlreadyGone => {}
        }
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, NaiveDate, Utc};
    use serde_json::json;
    use sqlx::PgPool;
    use uuid::Uuid;

    fn timestamp(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .expect("fixture timestamp must be valid")
            .to_utc()
    }

    fn plan_uses_index(plan: &serde_json::Value, index_name: &str) -> bool {
        match plan {
            serde_json::Value::Array(values) => values
                .iter()
                .any(|value| plan_uses_index(value, index_name)),
            serde_json::Value::Object(fields) => {
                fields.get("Index Name").and_then(|value| value.as_str()) == Some(index_name)
                    || fields
                        .values()
                        .any(|value| plan_uses_index(value, index_name))
            }
            _ => false,
        }
    }

    fn plan_mentions_relation(plan: &serde_json::Value, relation_name: &str) -> bool {
        match plan {
            serde_json::Value::Array(values) => values
                .iter()
                .any(|value| plan_mentions_relation(value, relation_name)),
            serde_json::Value::Object(fields) => {
                fields.get("Relation Name").and_then(|value| value.as_str()) == Some(relation_name)
                    || fields
                        .values()
                        .any(|value| plan_mentions_relation(value, relation_name))
            }
            _ => false,
        }
    }

    fn relation_actual_loops(plan: &serde_json::Value, relation_name: &str) -> u64 {
        match plan {
            serde_json::Value::Array(values) => values
                .iter()
                .map(|value| relation_actual_loops(value, relation_name))
                .sum(),
            serde_json::Value::Object(fields) => {
                let this_relation = if fields.get("Relation Name").and_then(|value| value.as_str())
                    == Some(relation_name)
                {
                    fields
                        .get("Actual Loops")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0)
                } else {
                    0
                };
                this_relation
                    + fields
                        .values()
                        .map(|value| relation_actual_loops(value, relation_name))
                        .sum::<u64>()
            }
            _ => 0,
        }
    }

    async fn explain_bounded_list_query(pool: &PgPool, sql: &str, page: bool) -> serde_json::Value {
        let explain = format!("EXPLAIN (ANALYZE, FORMAT JSON) {sql}");
        let query = sqlx::query_scalar(&explain)
            .bind(Option::<&str>::None)
            .bind(Option::<&str>::None)
            .bind(Option::<Vec<String>>::None)
            .bind(Some(timestamp("2026-08-10T00:00:00Z")))
            .bind(Option::<DateTime<Utc>>::None)
            .bind(Option::<Vec<String>>::None);
        if page {
            query.bind(20_i64).bind(0_i64)
        } else {
            query
        }
        .fetch_one(pool)
        .await
        .expect("bounded retained-list SQL must be explainable")
    }

    #[sqlx::test]
    async fn retained_list_and_count_plans_prune_children_before_created_after(pool: PgPool) {
        let relevant_delete_on = NaiveDate::from_ymd_opt(2026, 8, 20).unwrap();
        let irrelevant_delete_on = NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
        for delete_on in [irrelevant_delete_on, relevant_delete_on] {
            sqlx::query("SELECT ensure_retained_response_partition($1, NULL)")
                .bind(delete_on)
                .execute(&pool)
                .await
                .expect("planner fixture partition must be available");
        }
        let planner_group_id = Uuid::from_u128(0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb);
        let planner_request_id = Uuid::from_u128(0xcccccccccccccccccccccccccccccccc);
        sqlx::query(
            r#"
            INSERT INTO retained_response_objects (
                delete_on, group_id, object_kind, object_id, request_id,
                created_by, service_tier, state, model,
                created_at, terminal_at, schema_version, payload
            ) VALUES (
                $1, $2, 'request', $3, $3,
                'planner-owner', 'flex', 'completed', 'planner-model',
                '2026-08-11T09:00:00Z', '2026-08-12T10:00:00Z', 1, '{}'::jsonb
            )
            "#,
        )
        .bind(relevant_delete_on)
        .bind(planner_group_id)
        .bind(planner_request_id)
        .execute(&pool)
        .await
        .expect("matching retained planner row must insert");
        sqlx::query(
            "INSERT INTO retained_response_group_routes (group_id, delete_on) VALUES ($1, $2)",
        )
        .bind(planner_group_id)
        .bind(relevant_delete_on)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO retained_response_request_routes (request_id, group_id, delete_on) VALUES ($1, $2, $3)",
        )
        .bind(planner_request_id)
        .bind(planner_group_id)
        .bind(relevant_delete_on)
        .execute(&pool)
        .await
        .unwrap();

        let list_count: i64 = sqlx::query_scalar(LIST_REQUESTS_COUNT_SQL)
            .bind(Option::<&str>::None)
            .bind(Option::<&str>::None)
            .bind(Option::<Vec<String>>::None)
            .bind(Some(timestamp("2026-08-10T00:00:00Z")))
            .bind(Option::<DateTime<Utc>>::None)
            .bind(Option::<Vec<String>>::None)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(list_count, 1);
        let page_rows = sqlx::query(&list_requests_page_sql(false))
            .bind(Option::<&str>::None)
            .bind(Option::<&str>::None)
            .bind(Option::<Vec<String>>::None)
            .bind(Some(timestamp("2026-08-10T00:00:00Z")))
            .bind(Option::<DateTime<Utc>>::None)
            .bind(Option::<Vec<String>>::None)
            .bind(20_i64)
            .bind(0_i64)
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(page_rows.len(), 1);
        assert!(page_rows[0].get::<bool, _>("retained"));
        assert_eq!(page_rows[0].get::<Uuid, _>("object_id"), planner_request_id);

        for (label, sql, page) in [
            ("count", LIST_REQUESTS_COUNT_SQL.to_owned(), false),
            ("page", list_requests_page_sql(false), true),
        ] {
            let plan = explain_bounded_list_query(&pool, &sql, page).await;
            assert!(
                plan_mentions_relation(&plan, "retained_response_objects_d20260820"),
                "{label} plan must retain the relevant daily child: {plan}"
            );
            assert!(
                relation_actual_loops(&plan, "retained_response_objects_d20260820") > 0,
                "{label} plan must execute the relevant daily child: {plan}"
            );
            assert_eq!(
                relation_actual_loops(&plan, "retained_response_objects_d20260803"),
                0,
                "{label} plan must remove or avoid scanning the irrelevant daily child: {plan}"
            );
        }

        let flex_explain =
            format!("EXPLAIN (ANALYZE, FORMAT JSON) {COUNT_OWNER_FLEX_REQUESTS_SQL}");
        let flex_plan: serde_json::Value = sqlx::query_scalar(&flex_explain)
            .bind("planner-owner")
            .bind(timestamp("2026-08-10T00:00:00Z"))
            .fetch_one(&pool)
            .await
            .expect("bounded retained flex-count SQL must be explainable");
        let flex_count: i64 = sqlx::query_scalar(COUNT_OWNER_FLEX_REQUESTS_SQL)
            .bind("planner-owner")
            .bind(timestamp("2026-08-10T00:00:00Z"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(flex_count, 1);
        assert!(
            plan_mentions_relation(&flex_plan, "retained_response_objects_d20260820"),
            "flex-count plan must retain the relevant daily child: {flex_plan}"
        );
        assert!(
            relation_actual_loops(&flex_plan, "retained_response_objects_d20260820") > 0,
            "flex-count plan must execute the relevant daily child: {flex_plan}"
        );
        assert_eq!(
            relation_actual_loops(&flex_plan, "retained_response_objects_d20260803"),
            0,
            "flex-count plan must remove or avoid scanning the irrelevant daily child: {flex_plan}"
        );

        let trailing_rows = sqlx::query(TRAILING_DEMAND_SQL)
            .bind(timestamp("2026-08-10T00:00:00Z"))
            .bind(timestamp("2026-08-15T00:00:00Z"))
            .bind(vec!["planner-model".to_owned()])
            .bind(vec!["flex".to_owned()])
            .bind(false)
            .bind("include")
            .fetch_all(&pool)
            .await
            .expect("exact trailing-demand SQL must execute");
        assert_eq!(trailing_rows.len(), 1);
        assert_eq!(trailing_rows[0].get::<String, _>("model"), "planner-model");
        assert_eq!(trailing_rows[0].get::<String, _>("outcome"), "completed");
        assert_eq!(trailing_rows[0].get::<i64, _>("count"), 1);

        let trailing_explain = format!("EXPLAIN (ANALYZE, FORMAT JSON) {TRAILING_DEMAND_SQL}");
        let trailing_plan: serde_json::Value = sqlx::query_scalar(&trailing_explain)
            .bind(timestamp("2026-08-10T00:00:00Z"))
            .bind(timestamp("2026-08-15T00:00:00Z"))
            .bind(vec!["planner-model".to_owned()])
            .bind(vec!["flex".to_owned()])
            .bind(false)
            .bind("include")
            .fetch_one(&pool)
            .await
            .expect("exact trailing-demand SQL must be explainable");
        assert!(
            relation_actual_loops(&trailing_plan, "retained_response_objects_d20260820") > 0,
            "trailing plan must execute the relevant daily child: {trailing_plan}"
        );
        assert_eq!(
            relation_actual_loops(&trailing_plan, "retained_response_objects_d20260803"),
            0,
            "trailing plan must remove or avoid scanning the irrelevant daily child: {trailing_plan}"
        );
    }

    #[sqlx::test]
    async fn candidate_discovery_plan_uses_batchless_retention_due_index(pool: PgPool) {
        sqlx::query(
            r#"
            CREATE INDEX idx_requests_batchless_retention_due
            ON requests (
              service_tier,
              (CASE state WHEN 'completed' THEN completed_at
                          WHEN 'failed' THEN failed_at
                          WHEN 'canceled' THEN canceled_at END),
              id
            )
            WHERE batch_id IS NULL
              AND state IN ('completed', 'failed', 'canceled')
            "#,
        )
        .execute(&pool)
        .await
        .expect("candidate index must install");
        let mut tx = pool.begin().await.expect("planner transaction must begin");
        sqlx::query("SET LOCAL enable_seqscan = off")
            .execute(&mut *tx)
            .await
            .expect("sequential scans must be disabled for planner evidence");
        let explain = format!("EXPLAIN (FORMAT JSON) {CANDIDATE_DISCOVERY_SQL}");
        let plan: serde_json::Value = sqlx::query_scalar(&explain)
            .bind(vec!["flex".to_owned()])
            .bind(vec![timestamp("2026-08-01T00:00:00Z")])
            .bind(timestamp("2026-08-08T00:00:00Z"))
            .bind(Vec::<Uuid>::new())
            .fetch_one(&mut *tx)
            .await
            .expect("candidate discovery must be explainable");

        assert!(
            plan_uses_index(&plan, "idx_requests_batchless_retention_due"),
            "candidate seed must use the validated retention-due index: {plan}"
        );
    }

    fn request_id() -> Uuid {
        Uuid::from_u128(0x11111111111111111111111111111111)
    }

    fn template_id() -> Uuid {
        Uuid::from_u128(0x22222222222222222222222222222222)
    }

    /// A group's id is its request's id.
    fn group_id() -> Uuid {
        request_id()
    }

    fn delete_on() -> NaiveDate {
        NaiveDate::from_ymd_opt(2030, 2, 3).expect("fixture date must be valid")
    }

    fn request_payload() -> RetainedRequestPayloadV1 {
        RetainedRequestPayloadV1 {
            request: RetainedRequestSnapshot {
                id: request_id(),
                batch_id: None,
                template_id: Some(template_id()),
                custom_id: Some("request-correlation".to_owned()),
                model: "request-routing-model".to_owned(),
                state: "completed".to_owned(),
                retry_attempt: 7,
                not_before: Some(timestamp("2030-01-02T03:04:05.000006Z")),
                daemon_id: Some(Uuid::from_u128(0x55555555555555555555555555555555)),
                claimed_at: Some(timestamp("2030-01-02T03:04:06.000007Z")),
                started_at: Some(timestamp("2030-01-02T03:04:07.123456Z")),
                response_status: Some(207),
                response_body: Some("response-body-secret".to_owned()),
                completed_at: Some(timestamp("2030-01-02T03:04:11.456789Z")),
                error: Some("error-body-secret".to_owned()),
                failed_at: Some(timestamp("2030-01-02T03:04:12.000001Z")),
                canceled_at: Some(timestamp("2030-01-02T03:04:13.000001Z")),
                response_size: 9_876,
                routed_model: Some("actually-routed-model".to_owned()),
                service_tier: Some("priority".to_owned()),
                created_by: Some("owner-identifier".to_owned()),
                created_at: timestamp("2030-01-02T03:04:00.000001Z"),
                updated_at: timestamp("2030-01-02T03:04:14.000001Z"),
            },
            template: RetainedTemplateSnapshot {
                id: template_id(),
                file_id: None,
                custom_id: Some("template-correlation".to_owned()),
                endpoint: "https://example.invalid/inference".to_owned(),
                method: "POST".to_owned(),
                path: "/v1/responses".to_owned(),
                body: "prompt-body-secret".to_owned(),
                model: "template-routing-model".to_owned(),
                api_key: "api-key-secret".to_owned(),
                line_number: 42,
                body_byte_size: 18,
                metadata: Some(json!({"request_source": "metadata-secret"})),
                created_at: timestamp("2030-01-02T03:03:00.000001Z"),
                updated_at: timestamp("2030-01-02T03:03:01.000001Z"),
            },
        }
    }

    fn other_request_id() -> Uuid {
        Uuid::from_u128(0x66666666666666666666666666666666)
    }

    fn other_template_id() -> Uuid {
        Uuid::from_u128(0x77777777777777777777777777777777)
    }

    fn other_group_id() -> Uuid {
        Uuid::from_u128(0x99999999999999999999999999999999)
    }

    fn other_request_payload() -> RetainedRequestPayloadV1 {
        let mut payload = request_payload();
        payload.request.id = other_request_id();
        payload.request.template_id = Some(other_template_id());
        payload.template.id = other_template_id();
        payload
    }

    fn complete_group() -> (RetainedGroup, RetainedRequestPayloadV1) {
        (
            RetainedGroup {
                group_id: group_id(),
                request_ids: vec![request_id()],
            },
            request_payload(),
        )
    }

    #[test]
    fn request_payload_round_trip_preserves_detail_template_and_unknown_future_fields() {
        // Mutation caught: omitting a nullable request/template field, mapping the
        // template body to the wrong RequestDetail field, or rejecting a future
        // additive JSON key would lose archived API data during a V1 read.
        let payload = request_payload();
        let row = RetainedResponseObjectRow::request(delete_on(), group_id(), &payload)
            .expect("request fixture must encode");
        let mut future_payload = row.payload.clone();
        future_payload["future_additive_field"] = json!({"marker": "future-secret"});
        let decoded = RetainedResponseObjectRow {
            payload: future_payload,
            ..row
        }
        .decode_request()
        .expect("V1 request payload with an additive field must decode");

        let detail = decoded
            .to_request_detail()
            .expect("complete batchless snapshot must reconstruct request detail");
        assert_eq!(detail.id, request_id());
        assert_eq!(detail.batch_id, None);
        assert_eq!(detail.model, "request-routing-model");
        assert_eq!(detail.status, "completed");
        assert_eq!(detail.created_at, timestamp("2030-01-02T03:04:00.000001Z"));
        assert_eq!(
            detail.completed_at,
            Some(timestamp("2030-01-02T03:04:11.456789Z"))
        );
        assert_eq!(
            detail.failed_at,
            Some(timestamp("2030-01-02T03:04:12.000001Z"))
        );
        assert_eq!(detail.response_status, Some(207));
        assert_eq!(detail.body.as_deref(), Some("prompt-body-secret"));
        assert_eq!(
            detail.response_body.as_deref(),
            Some("response-body-secret")
        );
        assert_eq!(detail.error.as_deref(), Some("error-body-secret"));
        assert_eq!(detail.service_tier.as_deref(), Some("priority"));
        assert_eq!(detail.created_by, "owner-identifier");
        assert!(
            (detail.duration_ms.expect("duration must be present") - 4_333.333).abs()
                < f64::EPSILON
        );

        assert_eq!(decoded.request.template_id, Some(template_id()));
        assert_eq!(decoded.template.id, template_id());
        assert_eq!(decoded.template.file_id, None);
        assert_eq!(
            decoded.template.custom_id.as_deref(),
            Some("template-correlation")
        );
        assert_eq!(
            decoded.template.endpoint,
            "https://example.invalid/inference"
        );
        assert_eq!(decoded.template.method, "POST");
        assert_eq!(decoded.template.path, "/v1/responses");
        assert_eq!(decoded.template.model, "template-routing-model");
        assert_eq!(decoded.template.api_key, "api-key-secret");
        assert_eq!(decoded.template.line_number, 42);
        assert_eq!(decoded.template.body_byte_size, 18);
        assert_eq!(
            decoded.template.metadata,
            Some(json!({"request_source": "metadata-secret"}))
        );
        assert_eq!(
            decoded.request.routed_model.as_deref(),
            Some("actually-routed-model")
        );
        assert_eq!(decoded.request.response_size, 9_876);

        let debug = format!("{decoded:?}");
        for secret in [
            "prompt-body-secret",
            "response-body-secret",
            "api-key-secret",
            "metadata-secret",
            "owner-identifier",
            &request_id().to_string(),
        ] {
            assert!(!debug.contains(secret));
        }
    }

    #[test]
    fn request_decoder_accepts_anomalous_safe_chronology_before_delete_on() {
        // Mutation caught: reintroducing terminal_at >= created_at as a V1
        // requirement rejects historical rows that the live schema permits.
        let payload = request_payload();
        let mut row = RetainedResponseObjectRow::request(delete_on(), group_id(), &payload)
            .expect("baseline request fixture must encode");
        let anomalous_created_at = timestamp("2030-01-03T03:04:00.000001Z");
        row.created_at = Some(anomalous_created_at);
        row.payload["request"]["created_at"] = json!(anomalous_created_at);

        let decoded = row
            .decode_request()
            .expect("both timestamps before delete_on must remain readable");
        assert_eq!(decoded.request.created_at, anomalous_created_at);
        assert_eq!(
            decoded.request.completed_at,
            Some(timestamp("2030-01-02T03:04:11.456789Z"))
        );
    }

    #[test]
    fn request_decoder_rejects_created_or_terminal_timestamp_at_delete_on() {
        // Mutation caught: omitting either row.delete_on comparison would make
        // the created_after/terminal-window partition bounds unsound.
        let payload = request_payload();
        let mut created_boundary =
            RetainedResponseObjectRow::request(delete_on(), group_id(), &payload)
                .expect("baseline request fixture must encode");
        let anomalous_created_at = timestamp("2030-01-03T03:04:00.000001Z");
        created_boundary.created_at = Some(anomalous_created_at);
        created_boundary.payload["request"]["created_at"] = json!(anomalous_created_at);
        created_boundary.delete_on =
            NaiveDate::from_ymd_opt(2030, 1, 3).expect("fixture date must be valid");
        assert_eq!(
            created_boundary.decode_request(),
            Err(RetainedResponseSerializationError::InvalidRequestSnapshot)
        );

        let mut terminal_boundary =
            RetainedResponseObjectRow::request(delete_on(), group_id(), &payload)
                .expect("baseline request fixture must encode");
        let earlier_created_at = timestamp("2030-01-01T03:04:00.000001Z");
        terminal_boundary.created_at = Some(earlier_created_at);
        terminal_boundary.payload["request"]["created_at"] = json!(earlier_created_at);
        terminal_boundary.delete_on =
            NaiveDate::from_ymd_opt(2030, 1, 2).expect("fixture date must be valid");
        assert_eq!(
            terminal_boundary.decode_request(),
            Err(RetainedResponseSerializationError::InvalidRequestSnapshot)
        );
    }

    #[test]
    fn tagged_group_composition_preserves_exact_member_list_and_rejects_partial_lists() {
        // Mutation caught: producing a group header with a missing or extra
        // member would let a later mover commit a partial or foreign graph.
        let (group, request) = complete_group();

        let rows = RetainedGroup::compose_rows(delete_on(), group.clone(), vec![request.clone()])
            .expect("complete group must compose");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].object_kind, RetainedObjectKind::Group);
        assert_eq!(rows[1].object_kind, RetainedObjectKind::Request);
        assert_eq!(
            rows[0].decode_group().expect("group header must decode"),
            group
        );
        assert_eq!(
            rows[1]
                .decode_request()
                .expect("request member must decode")
                .request
                .id,
            request_id()
        );

        let missing_member = RetainedGroup::compose_rows(
            delete_on(),
            RetainedGroup {
                request_ids: Vec::new(),
                ..group.clone()
            },
            vec![request.clone()],
        )
        .expect_err("a header without its request must fail closed");
        assert_eq!(
            missing_member,
            RetainedResponseSerializationError::IncompleteGroup
        );

        let extra_member = RetainedGroup::compose_rows(
            delete_on(),
            RetainedGroup {
                request_ids: vec![request_id(), other_request_id()],
                ..group
            },
            vec![request, other_request_payload()],
        )
        .expect_err("a group can only ever hold its one request");
        assert_eq!(
            extra_member,
            RetainedResponseSerializationError::IncompleteGroup
        );
    }

    #[test]
    fn compose_rows_requires_the_group_to_be_its_own_request() {
        // Mutation caught: a group whose id is not its request's id, or whose
        // member is a different request, is not a retained response graph.
        let (group, request) = complete_group();

        let foreign_group = RetainedGroup::compose_rows(
            delete_on(),
            RetainedGroup {
                group_id: other_group_id(),
                ..group.clone()
            },
            vec![request.clone()],
        )
        .expect_err("the group id must be the request id");
        assert_eq!(
            foreign_group,
            RetainedResponseSerializationError::IncompleteGroup
        );

        let foreign_member =
            RetainedGroup::compose_rows(delete_on(), group, vec![other_request_payload()])
                .expect_err("the member must be the declared request");
        assert_eq!(
            foreign_member,
            RetainedResponseSerializationError::IncompleteGroup
        );
    }

    #[test]
    fn group_decoder_rejects_duplicate_request_ids() {
        // Mutation caught: accepting duplicate IDs in an archived group header
        // can make one graph member appear as two independently routed rows.
        let (group, _) = complete_group();
        let row = RetainedResponseObjectRow::group(delete_on(), &group)
            .expect("group fixture must encode");

        let mut duplicate_request = row;
        duplicate_request.payload["request_ids"] = json!([request_id(), request_id()]);
        let request_error = duplicate_request
            .decode_group()
            .expect_err("duplicate request IDs must fail closed");
        assert_eq!(
            request_error,
            RetainedResponseSerializationError::IncompleteGroup
        );
    }

    #[test]
    fn request_decoder_rejects_group_routing_tampering() {
        // Mutation caught: changing only indexed route columns must not attach
        // an otherwise valid content payload to another response graph.
        let (group, request) = complete_group();
        let rows = RetainedGroup::compose_rows(delete_on(), group, vec![request])
            .expect("complete group must compose");

        let request_group_tamper = RetainedResponseObjectRow {
            group_id: other_group_id(),
            ..rows[1].clone()
        }
        .decode_request()
        .expect_err("request rows must bind their group in payload");
        assert_eq!(
            request_group_tamper,
            RetainedResponseSerializationError::RowMetadataMismatch
        );

        let mut request_payload_tamper = rows[1].clone();
        request_payload_tamper.payload["group_id"] = json!(other_group_id());
        let request_payload_error = request_payload_tamper
            .decode_request()
            .expect_err("request payload routing must match its row");
        assert_eq!(
            request_payload_error,
            RetainedResponseSerializationError::RowMetadataMismatch
        );

        let group_row_tamper = RetainedResponseObjectRow {
            group_id: other_group_id(),
            ..rows[0].clone()
        }
        .decode_group()
        .expect_err("group rows must bind their group in payload");
        assert_eq!(
            group_row_tamper,
            RetainedResponseSerializationError::RowMetadataMismatch
        );
    }

    #[test]
    fn decoders_distinguish_missing_nullable_fields_from_explicit_null() {
        // Mutation caught: derive-based Option deserialization turns a
        // truncated V1 field into None, making a malformed archive look valid.
        let request = request_payload();
        let request_row = RetainedResponseObjectRow::request(delete_on(), group_id(), &request)
            .expect("request fixture must encode");

        let mut explicit_request_null = request_row.clone();
        explicit_request_null.payload["request"]["response_body"] = serde_json::Value::Null;
        explicit_request_null
            .decode_request()
            .expect("an explicit nullable request null must decode");
        let mut missing_request_nullable = request_row.clone();
        missing_request_nullable.payload["request"]
            .as_object_mut()
            .expect("request fixture is an object")
            .remove("response_body");
        let missing_request = missing_request_nullable
            .decode_request()
            .expect_err("an omitted nullable request field must fail closed");
        assert_eq!(
            missing_request,
            RetainedResponseSerializationError::MalformedPayload
        );

        let mut explicit_template_null = request_row.clone();
        explicit_template_null.payload["template"]["metadata"] = serde_json::Value::Null;
        explicit_template_null
            .decode_request()
            .expect("an explicit nullable template null must decode");
        let mut missing_template_nullable = request_row;
        missing_template_nullable.payload["template"]
            .as_object_mut()
            .expect("template fixture is an object")
            .remove("metadata");
        let missing_template = missing_template_nullable
            .decode_request()
            .expect_err("an omitted nullable template field must fail closed");
        assert_eq!(
            missing_template,
            RetainedResponseSerializationError::MalformedPayload
        );

        let mut missing_request_route =
            RetainedResponseObjectRow::request(delete_on(), group_id(), &request_payload())
                .expect("request fixture must encode");
        missing_request_route
            .payload
            .as_object_mut()
            .expect("request fixture is an object")
            .remove("group_id");
        let missing_route = missing_request_route
            .decode_request()
            .expect_err("an omitted route field must fail closed");
        assert_eq!(
            missing_route,
            RetainedResponseSerializationError::MalformedPayload
        );
    }

    #[test]
    fn decoders_reject_unknown_versions_malformed_payloads_and_object_kinds_without_content() {
        // Mutation caught: decoding a future version as V1, accepting malformed
        // JSON, or trusting a mismatched row tag would surface the wrong object.
        let payload = request_payload();
        let row = RetainedResponseObjectRow::request(delete_on(), group_id(), &payload)
            .expect("request fixture must encode");

        let unknown_version = RetainedResponseObjectRow {
            schema_version: RETAINED_RESPONSE_SCHEMA_V1 + 1,
            ..row.clone()
        }
        .decode_request()
        .expect_err("future schema versions must not decode as V1");
        assert_eq!(
            unknown_version,
            RetainedResponseSerializationError::UnsupportedSchemaVersion
        );

        let wrong_kind = RetainedResponseObjectRow {
            object_kind: RetainedObjectKind::Group,
            ..row.clone()
        }
        .decode_request()
        .expect_err("a group row must not decode through the request decoder");
        assert_eq!(
            wrong_kind,
            RetainedResponseSerializationError::ObjectKindMismatch
        );

        let malformed = RetainedResponseObjectRow {
            payload: json!({"request": {"id": "not-a-uuid"}}),
            ..row
        }
        .decode_request()
        .expect_err("malformed V1 payloads must fail closed");
        assert_eq!(
            malformed,
            RetainedResponseSerializationError::MalformedPayload
        );

        let unsupported_kind = RetainedObjectKind::try_from("step")
            .expect_err("the retired step object kind must fail closed");
        assert_eq!(
            unsupported_kind,
            RetainedResponseSerializationError::UnsupportedObjectKind
        );
        let unknown_kind = RetainedObjectKind::try_from("untrusted-kind")
            .expect_err("unknown database object kinds must fail closed");
        assert_eq!(
            unknown_kind,
            RetainedResponseSerializationError::UnsupportedObjectKind
        );

        for error in [unknown_version, wrong_kind, malformed, unsupported_kind] {
            let display = error.to_string();
            assert!(!display.contains("request-routing-model"));
            assert!(!display.contains("prompt-body-secret"));
            assert!(!display.contains("untrusted-kind"));
            assert!(!display.contains(&request_id().to_string()));
        }
    }
}
