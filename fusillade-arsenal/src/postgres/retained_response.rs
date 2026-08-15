//! Versioned retained-response object snapshots.
//!
//! This module defines the representation/decoding boundary, the bounded
//! atomic mover, and active-bucket read routing for retained response graphs.
//! Partition retirement remains a separate lifecycle phase.

use std::fmt;

use chrono::{DateTime, NaiveDate, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

use super::{PoolProvider, PostgresRequestManager};
use crate::error::{FusilladeError, Result};
use crate::manager::{
    RetainedResponseArchiveOutcome, RetainedResponseMaintenanceError, RetainedResponseWriteError,
    RetentionSweepPolicy,
};
use crate::request::{
    ListRequestsFilter, RequestDetail, RequestId, RequestListResult, RequestSummary,
};
use crate::response_step::{ResponseStep, StepId, StepKind, StepState};

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

/// Classify UUIDs without consulting retained payloads. Active routes win
/// over fences; once a bucket is retiring, detached, dropped, or its routes
/// have been cleaned, the durable unexpired fence yields a fixed not-found.
pub(crate) async fn classify_response_write(
    tx: &mut Transaction<'_, Postgres>,
    object_ids: &[Uuid],
) -> Result<Option<ResponseWriteDisposition>> {
    if object_ids.is_empty() {
        return Ok(None);
    }
    let active: bool = sqlx::query_scalar(
        r#"
        WITH candidate_route AS (
            SELECT route.group_id, route.delete_on
            FROM retained_response_group_routes route
            WHERE route.group_id = ANY($1)
            UNION
            SELECT route.group_id, route.delete_on
            FROM retained_response_request_routes route
            WHERE route.request_id = ANY($1)
            UNION
            SELECT route.group_id, route.delete_on
            FROM retained_response_step_routes route
            WHERE route.step_id = ANY($1)
        )
        SELECT EXISTS (
            SELECT 1
            FROM candidate_route route
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
            WHERE bucket.state = 'active'
              AND bucket.partition_schema = current_schema()
              AND bucket.partition_table =
                  'retained_response_objects_d' || to_char(route.delete_on, 'YYYYMMDD')
              AND inheritance.inhparent =
                  to_regclass(format('%I.retained_response_objects', current_schema()))
              AND pg_get_expr(child.relpartbound, child.oid) = format(
                  'FOR VALUES FROM (%L) TO (%L)', route.delete_on, route.delete_on + 1
              )
        )
        "#,
    )
    .bind(object_ids)
    .fetch_one(&mut **tx)
    .await
    .map_err(|_| FusilladeError::Other(anyhow::anyhow!("Failed to classify response write")))?;
    if active {
        return Ok(Some(ResponseWriteDisposition::AlreadyRetained));
    }
    let fenced: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM retained_response_resurrection_fences \
         WHERE object_id = ANY($1) AND expires_at > clock_timestamp())",
    )
    .bind(object_ids)
    .fetch_one(&mut **tx)
    .await
    .map_err(|_| FusilladeError::Other(anyhow::anyhow!("Failed to classify response write")))?;
    Ok(fenced.then_some(ResponseWriteDisposition::NotFound))
}

/// Return every input UUID protected by either an exact active retained route
/// or an unexpired resurrection fence. Bulk synthesis uses this before it
/// allocates any payload-bearing template rows.
pub(crate) async fn protected_response_write_ids(
    tx: &mut Transaction<'_, Postgres>,
    object_ids: &[Uuid],
) -> Result<std::collections::HashSet<Uuid>> {
    if object_ids.is_empty() {
        return Ok(std::collections::HashSet::new());
    }
    let protected: Vec<Uuid> = sqlx::query_scalar(
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
            UNION
            SELECT input.object_id, route.group_id, route.delete_on
            FROM input_ids input
            JOIN retained_response_step_routes route ON route.step_id = input.object_id
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
        ), fenced_ids AS (
            SELECT input.object_id
            FROM input_ids input
            JOIN retained_response_resurrection_fences fence
              ON fence.object_id = input.object_id
            WHERE fence.expires_at > clock_timestamp()
        )
        SELECT object_id FROM active_ids
        UNION
        SELECT object_id FROM fenced_ids
        "#,
    )
    .bind(object_ids)
    .fetch_all(&mut **tx)
    .await
    .map_err(|_| FusilladeError::Other(anyhow::anyhow!("Failed to classify response writes")))?;
    Ok(protected.into_iter().collect())
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

    pub(crate) fn from_fusillade_error(error: &FusilladeError) -> Option<Self> {
        match error {
            FusilladeError::Other(error) => error.downcast_ref::<Self>().copied(),
            _ => None,
        }
    }
}

/// The tag stored in `retained_response_objects.object_kind`.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RetainedObjectKind {
    Group,
    Request,
    Step,
}

impl RetainedObjectKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Group => "group",
            Self::Request => "request",
            Self::Step => "step",
        }
    }
}

impl TryFrom<&str> for RetainedObjectKind {
    type Error = RetainedResponseSerializationError;

    fn try_from(value: &str) -> std::result::Result<Self, Self::Error> {
        match value {
            "group" => Ok(Self::Group),
            "request" => Ok(Self::Request),
            "step" => Ok(Self::Step),
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

/// V1 request object payload: one request row and its dedicated template.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RetainedRequestPayloadV1 {
    pub(crate) request: RetainedRequestSnapshot,
    pub(crate) template: RetainedTemplateSnapshot,
}

impl RetainedRequestPayloadV1 {
    fn validate(&self) -> std::result::Result<(), RetainedResponseSerializationError> {
        if self.request.batch_id.is_some()
            || self.request.template_id != Some(self.template.id)
            || self.template.file_id.is_some()
        {
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
/// request/template fields while binding them to the route columns that select
/// a retained graph.
#[derive(Serialize)]
struct RetainedRequestPayloadEnvelopeV1 {
    group_id: Uuid,
    head_step_id: Option<Uuid>,
    #[serde(flatten)]
    payload: RetainedRequestPayloadV1,
}

#[derive(Deserialize)]
struct RetainedRequestPayloadEnvelopeV1Wire {
    group_id: Uuid,
    head_step_id: Option<Uuid>,
    #[serde(flatten)]
    payload: RetainedRequestPayloadV1,
}

impl<'de> Deserialize<'de> for RetainedRequestPayloadEnvelopeV1 {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire: RetainedRequestPayloadEnvelopeV1Wire =
            deserialize_object_with_required_fields(deserializer, &["head_step_id"])?;
        Ok(Self {
            group_id: wire.group_id,
            head_step_id: wire.head_step_id,
            payload: wire.payload,
        })
    }
}

/// A complete immutable copy of a `response_steps` row.
#[derive(Clone, PartialEq, Serialize)]
pub(crate) struct RetainedStepSnapshot {
    pub(crate) id: Uuid,
    pub(crate) request_id: Option<Uuid>,
    pub(crate) prev_step_id: Option<Uuid>,
    pub(crate) parent_step_id: Option<Uuid>,
    pub(crate) step_kind: StepKind,
    pub(crate) step_sequence: i64,
    pub(crate) request_payload: serde_json::Value,
    pub(crate) response_payload: Option<serde_json::Value>,
    pub(crate) state: StepState,
    pub(crate) started_at: Option<DateTime<Utc>>,
    pub(crate) completed_at: Option<DateTime<Utc>>,
    pub(crate) failed_at: Option<DateTime<Utc>>,
    pub(crate) canceled_at: Option<DateTime<Utc>>,
    pub(crate) retry_attempt: i32,
    pub(crate) error: Option<serde_json::Value>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct RetainedStepSnapshotWire {
    id: Uuid,
    request_id: Option<Uuid>,
    prev_step_id: Option<Uuid>,
    parent_step_id: Option<Uuid>,
    step_kind: StepKind,
    step_sequence: i64,
    request_payload: serde_json::Value,
    response_payload: Option<serde_json::Value>,
    state: StepState,
    started_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
    failed_at: Option<DateTime<Utc>>,
    canceled_at: Option<DateTime<Utc>>,
    retry_attempt: i32,
    error: Option<serde_json::Value>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl<'de> Deserialize<'de> for RetainedStepSnapshot {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire: RetainedStepSnapshotWire = deserialize_object_with_required_fields(
            deserializer,
            &[
                "request_id",
                "prev_step_id",
                "parent_step_id",
                "response_payload",
                "started_at",
                "completed_at",
                "failed_at",
                "canceled_at",
                "error",
            ],
        )?;
        Ok(Self {
            id: wire.id,
            request_id: wire.request_id,
            prev_step_id: wire.prev_step_id,
            parent_step_id: wire.parent_step_id,
            step_kind: wire.step_kind,
            step_sequence: wire.step_sequence,
            request_payload: wire.request_payload,
            response_payload: wire.response_payload,
            state: wire.state,
            started_at: wire.started_at,
            completed_at: wire.completed_at,
            failed_at: wire.failed_at,
            canceled_at: wire.canceled_at,
            retry_attempt: wire.retry_attempt,
            error: wire.error,
            created_at: wire.created_at,
            updated_at: wire.updated_at,
        })
    }
}

impl RetainedStepSnapshot {
    fn terminal_at(&self) -> Option<DateTime<Utc>> {
        match self.state {
            StepState::Pending | StepState::Processing => None,
            StepState::Completed => self.completed_at,
            StepState::Failed => self.failed_at,
            StepState::Canceled => self.canceled_at,
        }
    }
}

impl fmt::Debug for RetainedStepSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RetainedStepSnapshot")
            .field("has_id", &true)
            .field("has_request_id", &self.request_id.is_some())
            .field("has_prev_step_id", &self.prev_step_id.is_some())
            .field("has_parent_step_id", &self.parent_step_id.is_some())
            .field("has_request_payload", &true)
            .field("has_response_payload", &self.response_payload.is_some())
            .field("has_started_at", &self.started_at.is_some())
            .field("has_completed_at", &self.completed_at.is_some())
            .field("has_failed_at", &self.failed_at.is_some())
            .field("has_canceled_at", &self.canceled_at.is_some())
            .field("has_error", &self.error.is_some())
            .finish()
    }
}

/// V1 response-step object payload.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RetainedStepPayloadV1 {
    pub(crate) step: RetainedStepSnapshot,
}

impl RetainedStepPayloadV1 {
    pub(crate) fn from_response_step(step: &ResponseStep) -> Self {
        Self {
            step: RetainedStepSnapshot {
                id: step.id.0,
                request_id: step.request_id.map(|request_id| request_id.0),
                prev_step_id: step.prev_step_id.map(|step_id| step_id.0),
                parent_step_id: step.parent_step_id.map(|step_id| step_id.0),
                step_kind: step.step_kind,
                step_sequence: step.step_sequence,
                request_payload: step.request_payload.clone(),
                response_payload: step.response_payload.clone(),
                state: step.state,
                started_at: step.started_at,
                completed_at: step.completed_at,
                failed_at: step.failed_at,
                canceled_at: step.canceled_at,
                retry_attempt: step.retry_attempt,
                error: step.error.clone(),
                created_at: step.created_at,
                updated_at: step.updated_at,
            },
        }
    }

    /// Reconstruct the existing response-step model from its V1 snapshot.
    pub(crate) fn to_response_step(&self) -> ResponseStep {
        ResponseStep {
            id: StepId(self.step.id),
            request_id: self.step.request_id.map(RequestId),
            prev_step_id: self.step.prev_step_id.map(StepId),
            parent_step_id: self.step.parent_step_id.map(StepId),
            step_kind: self.step.step_kind,
            step_sequence: self.step.step_sequence,
            request_payload: self.step.request_payload.clone(),
            response_payload: self.step.response_payload.clone(),
            state: self.step.state,
            started_at: self.step.started_at,
            completed_at: self.step.completed_at,
            failed_at: self.step.failed_at,
            canceled_at: self.step.canceled_at,
            retry_attempt: self.step.retry_attempt,
            error: self.step.error.clone(),
            created_at: self.step.created_at,
            updated_at: self.step.updated_at,
        }
    }
}

impl fmt::Debug for RetainedStepPayloadV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RetainedStepPayloadV1")
            .field("step", &self.step)
            .finish()
    }
}

/// Row-bound V1 step payload. See [`RetainedRequestPayloadEnvelopeV1`] for
/// why the routing identity is stored inside, as well as alongside, the row.
#[derive(Serialize)]
struct RetainedStepPayloadEnvelopeV1 {
    group_id: Uuid,
    head_step_id: Option<Uuid>,
    #[serde(flatten)]
    payload: RetainedStepPayloadV1,
}

#[derive(Deserialize)]
struct RetainedStepPayloadEnvelopeV1Wire {
    group_id: Uuid,
    head_step_id: Option<Uuid>,
    #[serde(flatten)]
    payload: RetainedStepPayloadV1,
}

impl<'de> Deserialize<'de> for RetainedStepPayloadEnvelopeV1 {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire: RetainedStepPayloadEnvelopeV1Wire =
            deserialize_object_with_required_fields(deserializer, &["head_step_id"])?;
        Ok(Self {
            group_id: wire.group_id,
            head_step_id: wire.head_step_id,
            payload: wire.payload,
        })
    }
}

/// Content-free group header that proves the exact retained graph membership.
#[derive(Clone, PartialEq, Serialize)]
pub(crate) struct RetainedGroup {
    pub(crate) group_id: Uuid,
    pub(crate) head_step_id: Option<Uuid>,
    pub(crate) request_ids: Vec<Uuid>,
    pub(crate) step_ids: Vec<Uuid>,
}

#[derive(Deserialize)]
struct RetainedGroupWire {
    group_id: Uuid,
    head_step_id: Option<Uuid>,
    request_ids: Vec<Uuid>,
    step_ids: Vec<Uuid>,
}

impl<'de> Deserialize<'de> for RetainedGroup {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire: RetainedGroupWire =
            deserialize_object_with_required_fields(deserializer, &["head_step_id"])?;
        Ok(Self {
            group_id: wire.group_id,
            head_step_id: wire.head_step_id,
            request_ids: wire.request_ids,
            step_ids: wire.step_ids,
        })
    }
}

impl RetainedGroup {
    fn validate_header(&self) -> std::result::Result<(), RetainedResponseSerializationError> {
        if has_duplicate_ids(&self.request_ids) || has_duplicate_ids(&self.step_ids) {
            return Err(RetainedResponseSerializationError::IncompleteGroup);
        }
        match self.head_step_id {
            Some(head_step_id) => {
                if self.group_id != head_step_id || !self.step_ids.contains(&head_step_id) {
                    return Err(RetainedResponseSerializationError::IncompleteGroup);
                }
            }
            None => {
                if !self.step_ids.is_empty()
                    || self.request_ids.len() != 1
                    || self.request_ids[0] != self.group_id
                {
                    return Err(RetainedResponseSerializationError::IncompleteGroup);
                }
            }
        }
        Ok(())
    }

    fn validate_predecessor_tree(
        &self,
        steps: &[RetainedStepPayloadV1],
        head_step_id: Uuid,
    ) -> std::result::Result<(), RetainedResponseSerializationError> {
        for payload in steps {
            let step = &payload.step;
            if step.id == head_step_id {
                if step.prev_step_id.is_some() {
                    return Err(RetainedResponseSerializationError::IncompleteGroup);
                }
            } else {
                let Some(predecessor_id) = step.prev_step_id else {
                    return Err(RetainedResponseSerializationError::IncompleteGroup);
                };
                if !self.step_ids.contains(&predecessor_id) {
                    return Err(RetainedResponseSerializationError::IncompleteGroup);
                }
            }
        }

        for payload in steps {
            if payload.step.id == head_step_id {
                continue;
            }

            let mut current_step_id = payload.step.id;
            let mut reached_head = false;
            for _ in 0..steps.len() {
                if current_step_id == head_step_id {
                    reached_head = true;
                    break;
                }
                let Some(current_step) = steps
                    .iter()
                    .find(|candidate| candidate.step.id == current_step_id)
                else {
                    return Err(RetainedResponseSerializationError::IncompleteGroup);
                };
                let Some(predecessor_id) = current_step.step.prev_step_id else {
                    return Err(RetainedResponseSerializationError::IncompleteGroup);
                };
                current_step_id = predecessor_id;
            }
            if !reached_head {
                return Err(RetainedResponseSerializationError::IncompleteGroup);
            }
        }

        Ok(())
    }

    fn validate_members(
        &self,
        requests: &[RetainedRequestPayloadV1],
        steps: &[RetainedStepPayloadV1],
    ) -> std::result::Result<(), RetainedResponseSerializationError> {
        self.validate_header()?;

        let request_ids = requests
            .iter()
            .map(|payload| payload.request.id)
            .collect::<Vec<_>>();
        let step_ids = steps
            .iter()
            .map(|payload| payload.step.id)
            .collect::<Vec<_>>();
        if self.request_ids != request_ids
            || self.step_ids != step_ids
            || has_duplicate_ids(&request_ids)
            || has_duplicate_ids(&step_ids)
        {
            return Err(RetainedResponseSerializationError::IncompleteGroup);
        }

        match self.head_step_id {
            None => Ok(()),
            Some(head_step_id) => {
                let head_count = steps
                    .iter()
                    .filter(|payload| payload.step.parent_step_id.is_none())
                    .count();
                let has_declared_head = steps.iter().any(|payload| {
                    payload.step.id == head_step_id && payload.step.parent_step_id.is_none()
                });
                if head_count != 1 || !has_declared_head {
                    return Err(RetainedResponseSerializationError::IncompleteGroup);
                }

                let mut model_request_ids = Vec::new();
                for payload in steps {
                    let step = &payload.step;
                    if step.id != head_step_id && step.parent_step_id != Some(head_step_id) {
                        return Err(RetainedResponseSerializationError::IncompleteGroup);
                    }
                    match (step.step_kind, step.request_id) {
                        (StepKind::ModelCall, Some(request_id)) => {
                            model_request_ids.push(request_id)
                        }
                        (StepKind::ToolCall, None) => {}
                        _ => return Err(RetainedResponseSerializationError::IncompleteGroup),
                    }
                }
                self.validate_predecessor_tree(steps, head_step_id)?;
                if has_duplicate_ids(&model_request_ids)
                    || !same_id_set(&model_request_ids, &self.request_ids)
                {
                    return Err(RetainedResponseSerializationError::IncompleteGroup);
                }
                Ok(())
            }
        }
    }

    /// Compose a tagged group header and every member payload after proving the
    /// supplied member lists are exact. This is representation-only: callers
    /// still own database locking and the all-or-nothing move transaction.
    pub(crate) fn compose_rows(
        delete_on: NaiveDate,
        group: Self,
        requests: Vec<RetainedRequestPayloadV1>,
        steps: Vec<RetainedStepPayloadV1>,
    ) -> std::result::Result<Vec<RetainedResponseObjectRow>, RetainedResponseSerializationError>
    {
        group.validate_members(&requests, &steps)?;

        let mut rows = Vec::with_capacity(1 + requests.len() + steps.len());
        rows.push(RetainedResponseObjectRow::group(delete_on, &group)?);
        for payload in &requests {
            rows.push(RetainedResponseObjectRow::request(
                delete_on,
                group.group_id,
                group.head_step_id,
                payload,
            )?);
        }
        for payload in &steps {
            rows.push(RetainedResponseObjectRow::step(
                delete_on,
                group.group_id,
                group.head_step_id,
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
            .field("has_head_step_id", &self.head_step_id.is_some())
            .field("request_count", &self.request_ids.len())
            .field("step_count", &self.step_ids.len())
            .finish()
    }
}

fn has_duplicate_ids(ids: &[Uuid]) -> bool {
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.windows(2).any(|pair| pair[0] == pair[1])
}

fn same_id_set(left: &[Uuid], right: &[Uuid]) -> bool {
    let mut left = left.to_vec();
    let mut right = right.to_vec();
    left.sort_unstable();
    right.sort_unstable();
    left == right
}

/// A typed row in `retained_response_objects` before it is bound to SQLx.
#[derive(Clone, PartialEq)]
pub(crate) struct RetainedResponseObjectRow {
    pub(crate) delete_on: NaiveDate,
    pub(crate) group_id: Uuid,
    pub(crate) object_kind: RetainedObjectKind,
    pub(crate) object_id: Uuid,
    pub(crate) request_id: Option<Uuid>,
    pub(crate) head_step_id: Option<Uuid>,
    pub(crate) created_by: Option<String>,
    pub(crate) service_tier: Option<String>,
    pub(crate) state: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) created_at: Option<DateTime<Utc>>,
    pub(crate) terminal_at: Option<DateTime<Utc>>,
    pub(crate) step_sequence: Option<i64>,
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
            head_step_id: group.head_step_id,
            created_by: None,
            service_tier: None,
            state: None,
            model: None,
            created_at: None,
            terminal_at: None,
            step_sequence: None,
            schema_version: RETAINED_RESPONSE_SCHEMA_V1,
            payload: to_payload(group)?,
        })
    }

    pub(crate) fn request(
        delete_on: NaiveDate,
        group_id: Uuid,
        head_step_id: Option<Uuid>,
        payload: &RetainedRequestPayloadV1,
    ) -> std::result::Result<Self, RetainedResponseSerializationError> {
        payload.validate_delete_on(delete_on)?;
        Ok(Self {
            delete_on,
            group_id,
            object_kind: RetainedObjectKind::Request,
            object_id: payload.request.id,
            request_id: Some(payload.request.id),
            head_step_id,
            created_by: payload.request.created_by.clone(),
            service_tier: payload.request.service_tier.clone(),
            state: Some(payload.request.state.clone()),
            model: Some(payload.request.model.clone()),
            created_at: Some(payload.request.created_at),
            terminal_at: payload.request.terminal_at()?,
            step_sequence: None,
            schema_version: RETAINED_RESPONSE_SCHEMA_V1,
            payload: to_payload(&RetainedRequestPayloadEnvelopeV1 {
                group_id,
                head_step_id,
                payload: payload.clone(),
            })?,
        })
    }

    pub(crate) fn step(
        delete_on: NaiveDate,
        group_id: Uuid,
        head_step_id: Option<Uuid>,
        payload: &RetainedStepPayloadV1,
    ) -> std::result::Result<Self, RetainedResponseSerializationError> {
        Ok(Self {
            delete_on,
            group_id,
            object_kind: RetainedObjectKind::Step,
            object_id: payload.step.id,
            request_id: payload.step.request_id,
            head_step_id,
            created_by: None,
            service_tier: None,
            state: Some(payload.step.state.as_str().to_owned()),
            model: None,
            created_at: Some(payload.step.created_at),
            terminal_at: payload.step.terminal_at(),
            step_sequence: Some(payload.step.step_sequence),
            schema_version: RETAINED_RESPONSE_SCHEMA_V1,
            payload: to_payload(&RetainedStepPayloadEnvelopeV1 {
                group_id,
                head_step_id,
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
                head_step_id: row
                    .try_get("head_step_id")
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
                step_sequence: row
                    .try_get("step_sequence")
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
            || self.head_step_id != group.head_step_id
            || self.created_by.is_some()
            || self.service_tier.is_some()
            || self.state.is_some()
            || self.model.is_some()
            || self.created_at.is_some()
            || self.terminal_at.is_some()
            || self.step_sequence.is_some()
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
            || self.head_step_id != envelope.head_step_id
            || self.object_id != payload.request.id
            || self.request_id != Some(payload.request.id)
            || self.created_by != payload.request.created_by
            || self.service_tier != payload.request.service_tier
            || self.state.as_deref() != Some(payload.request.state.as_str())
            || self.model.as_deref() != Some(payload.request.model.as_str())
            || self.created_at != Some(payload.request.created_at)
            || self.terminal_at != payload.request.terminal_at()?
            || self.step_sequence.is_some()
        {
            return Err(RetainedResponseSerializationError::RowMetadataMismatch);
        }
        Ok(payload)
    }

    pub(crate) fn decode_step(
        &self,
    ) -> std::result::Result<RetainedStepPayloadV1, RetainedResponseSerializationError> {
        self.require_kind(RetainedObjectKind::Step)?;
        let envelope: RetainedStepPayloadEnvelopeV1 = self.decode_payload()?;
        let payload = envelope.payload;
        if self.group_id != envelope.group_id
            || self.head_step_id != envelope.head_step_id
            || self.object_id != payload.step.id
            || self.request_id != payload.step.request_id
            || self.created_by.is_some()
            || self.service_tier.is_some()
            || self.state.as_deref() != Some(payload.step.state.as_str())
            || self.model.is_some()
            || self.created_at != Some(payload.step.created_at)
            || self.terminal_at != payload.step.terminal_at()
            || self.step_sequence != Some(payload.step.step_sequence)
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
            .field("has_head_step_id", &self.head_step_id.is_some())
            .field("has_created_by", &self.created_by.is_some())
            .field("has_service_tier", &self.service_tier.is_some())
            .field("has_state", &self.state.is_some())
            .field("has_model", &self.model.is_some())
            .field("has_created_at", &self.created_at.is_some())
            .field("has_terminal_at", &self.terminal_at.is_some())
            .field("has_step_sequence", &self.step_sequence.is_some())
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
    object.object_id, object.request_id, object.head_step_id, object.created_by, \
    object.service_tier, object.state, object.model, object.created_at, object.terminal_at, \
    object.step_sequence, object.schema_version, object.payload";

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
        LEFT JOIN request_templates template ON request.template_id = template.id
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

const LIST_REQUESTS_COUNT_SQL: &str = r#"
    WITH candidates AS (
        SELECT request.id
        FROM requests request
        WHERE request.created_by IS NOT NULL
          AND ($1::text IS NULL OR request.created_by = $1)
          AND ($2::text IS NULL OR request.state = $2)
          AND ($3::text[] IS NULL OR request.model = ANY($3))
          AND ($4::timestamptz IS NULL OR request.created_at >= $4)
          AND ($5::timestamptz IS NULL OR request.created_at <= $5)
          AND ($6::text[] IS NULL OR request.service_tier = ANY($6))

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
    )
    SELECT COUNT(*)::bigint FROM candidates
"#;

fn list_requests_page_sql(active_first: bool) -> String {
    let order_clause = if active_first {
        "CASE sort_state WHEN 'processing' THEN 0 WHEN 'claimed' THEN 1 WHEN 'pending' THEN 2 ELSE 3 END ASC, sort_created_at DESC, sort_id DESC"
    } else {
        "sort_created_at DESC, sort_id DESC"
    };
    format!(
        r#"
        WITH candidates AS (
            SELECT
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
                NULL::uuid AS head_step_id,
                NULL::text AS created_by,
                NULL::text AS service_tier,
                NULL::text AS state,
                NULL::text AS model,
                NULL::timestamptz AS created_at,
                NULL::timestamptz AS terminal_at,
                NULL::bigint AS step_sequence,
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

            UNION ALL

            SELECT
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
                object.head_step_id,
                object.created_by,
                object.service_tier,
                object.state,
                object.model,
                object.created_at,
                object.terminal_at,
                object.step_sequence,
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
        )
        SELECT retained, live_summary, delete_on, group_id, object_kind,
               object_id, request_id, head_step_id, created_by, service_tier,
               state, model, created_at, terminal_at, step_sequence,
               schema_version, payload
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

    let mut tx = begin_primary_read(manager).await?;
    let count: i64 = sqlx::query_scalar(LIST_REQUESTS_COUNT_SQL)
        .bind(filter.created_by.as_deref())
        .bind(filter.status.as_deref())
        .bind(filter.models.as_deref())
        .bind(filter.created_after)
        .bind(filter.created_before)
        .bind(filter.service_tiers.as_deref())
        .fetch_one(&mut *tx)
        .await
        .map_err(read_database_failure)?;

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

pub(crate) async fn get_step(
    tx: &mut Transaction<'_, Postgres>,
    step_id: StepId,
) -> Result<Option<ResponseStep>> {
    let query = format!(
        r#"
        SELECT {RETAINED_OBJECT_COLUMNS}
        FROM retained_response_step_routes route
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
         AND object.object_kind = 'step'
         AND object.object_id = route.step_id
        WHERE route.step_id = $1
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
        .bind(step_id.0)
        .fetch_optional(&mut **tx)
        .await
        .map_err(read_database_failure)?
        .map(|row| {
            RetainedResponseObjectRow::from_pg_row(&row)?
                .decode_step()
                .map(|payload| payload.to_response_step())
                .map_err(RetainedResponseSerializationError::into_fusillade_error)
        })
        .transpose()
}

pub(crate) async fn get_step_by_request(
    tx: &mut Transaction<'_, Postgres>,
    request_id: RequestId,
) -> Result<Option<ResponseStep>> {
    let query = format!(
        r#"
        SELECT {RETAINED_OBJECT_COLUMNS}
        FROM retained_response_request_routes request_route
        JOIN retained_response_group_routes group_route
          ON group_route.group_id = request_route.group_id
         AND group_route.delete_on = request_route.delete_on
        JOIN retained_response_buckets bucket
          ON bucket.delete_on = request_route.delete_on
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
          ON object.delete_on = request_route.delete_on
         AND object.group_id = request_route.group_id
         AND object.object_kind = 'step'
         AND object.request_id = request_route.request_id
        JOIN retained_response_step_routes step_route
          ON step_route.step_id = object.object_id
         AND step_route.group_id = object.group_id
         AND step_route.delete_on = object.delete_on
        WHERE request_route.request_id = $1
          AND bucket.state = 'active'
          AND bucket.partition_schema = current_schema()
          AND bucket.partition_table =
              'retained_response_objects_d' || to_char(request_route.delete_on, 'YYYYMMDD')
          AND inheritance.inhparent =
              to_regclass(format('%I.retained_response_objects', current_schema()))
          AND pg_get_expr(child.relpartbound, child.oid) = format(
              'FOR VALUES FROM (%L) TO (%L)',
              request_route.delete_on,
              request_route.delete_on + 1
          )
        "#,
    );
    sqlx::query(&query)
        .bind(request_id.0)
        .fetch_optional(&mut **tx)
        .await
        .map_err(read_database_failure)?
        .map(|row| {
            RetainedResponseObjectRow::from_pg_row(&row)?
                .decode_step()
                .map(|payload| payload.to_response_step())
                .map_err(RetainedResponseSerializationError::into_fusillade_error)
        })
        .transpose()
}

pub(crate) async fn list_chain(
    tx: &mut Transaction<'_, Postgres>,
    head_step_id: StepId,
) -> Result<Vec<ResponseStep>> {
    let query = format!(
        r#"
        SELECT {RETAINED_OBJECT_COLUMNS}
        FROM retained_response_group_routes route
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
         AND object.object_kind IN ('group', 'step')
        LEFT JOIN retained_response_step_routes step_route
          ON object.object_kind = 'step'
         AND step_route.step_id = object.object_id
         AND step_route.group_id = object.group_id
         AND step_route.delete_on = object.delete_on
        WHERE route.group_id = $1
          AND bucket.state = 'active'
          AND bucket.partition_schema = current_schema()
          AND bucket.partition_table =
              'retained_response_objects_d' || to_char(route.delete_on, 'YYYYMMDD')
          AND inheritance.inhparent =
              to_regclass(format('%I.retained_response_objects', current_schema()))
          AND pg_get_expr(child.relpartbound, child.oid) = format(
              'FOR VALUES FROM (%L) TO (%L)', route.delete_on, route.delete_on + 1
          )
          AND (object.object_kind = 'group' OR step_route.step_id IS NOT NULL)
        ORDER BY CASE object.object_kind WHEN 'group' THEN 0 ELSE 1 END,
                 object.step_sequence ASC,
                 object.object_id ASC
        "#,
    );
    let rows = sqlx::query(&query)
        .bind(head_step_id.0)
        .fetch_all(&mut **tx)
        .await
        .map_err(read_database_failure)?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    let mut rows = rows.into_iter();
    let group_row = RetainedResponseObjectRow::from_pg_row(&rows.next().ok_or_else(|| {
        RetainedResponseSerializationError::IncompleteGroup.into_fusillade_error()
    })?)?;
    let group = group_row
        .decode_group()
        .map_err(RetainedResponseSerializationError::into_fusillade_error)?;
    if group.group_id != head_step_id.0 || group.head_step_id != Some(head_step_id.0) {
        return Err(RetainedResponseSerializationError::IncompleteGroup.into_fusillade_error());
    }

    let mut steps = Vec::new();
    for row in rows {
        steps.push(
            RetainedResponseObjectRow::from_pg_row(&row)?
                .decode_step()
                .map_err(RetainedResponseSerializationError::into_fusillade_error)?,
        );
    }
    let actual_ids = steps
        .iter()
        .map(|payload| payload.step.id)
        .collect::<Vec<_>>();
    if !same_id_set(&actual_ids, &group.step_ids) {
        return Err(RetainedResponseSerializationError::IncompleteGroup.into_fusillade_error());
    }
    Ok(steps
        .into_iter()
        .map(|payload| payload.to_response_step())
        .collect())
}

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
        steps: u64,
        templates: u64,
        bytes: u64,
    },
    SkippedLocked,
    Deferred,
    AlreadyGone,
}

#[derive(Debug, PartialEq, Eq)]
struct GraphTopology {
    request_ids: Vec<Uuid>,
    step_ids: Vec<Uuid>,
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

fn step_payload(row: &PgRow) -> MovementResult<RetainedStepPayloadV1> {
    let step_kind: String = row.try_get("step_kind").map_err(database_failure)?;
    let state: String = row.try_get("state").map_err(database_failure)?;
    let step_kind = StepKind::parse(&step_kind).ok_or_else(incomplete_graph)?;
    let state = StepState::parse(&state).ok_or_else(incomplete_graph)?;
    let decode = || -> std::result::Result<RetainedStepPayloadV1, sqlx::Error> {
        Ok(RetainedStepPayloadV1 {
            step: RetainedStepSnapshot {
                id: row.try_get("id")?,
                request_id: row.try_get("request_id")?,
                prev_step_id: row.try_get("prev_step_id")?,
                parent_step_id: row.try_get("parent_step_id")?,
                step_kind,
                step_sequence: row.try_get("step_sequence")?,
                request_payload: row.try_get("request_payload")?,
                response_payload: row.try_get("response_payload")?,
                state,
                started_at: row.try_get("started_at")?,
                completed_at: row.try_get("completed_at")?,
                failed_at: row.try_get("failed_at")?,
                canceled_at: row.try_get("canceled_at")?,
                retry_attempt: row.try_get("retry_attempt")?,
                error: row.try_get("error")?,
                created_at: row.try_get("created_at")?,
                updated_at: row.try_get("updated_at")?,
            },
        })
    };
    decode().map_err(database_failure)
}

async fn current_group_id(
    tx: &mut Transaction<'_, Postgres>,
    request_id: Uuid,
) -> MovementResult<Uuid> {
    sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT COALESCE(parent_step_id, id)
        FROM response_steps
        WHERE request_id = $1
        "#,
    )
    .bind(request_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(database_failure)
    .map(|group_id| group_id.unwrap_or(request_id))
}

async fn graph_topology(
    tx: &mut Transaction<'_, Postgres>,
    group_id: Uuid,
    candidate_request_id: Uuid,
) -> MovementResult<GraphTopology> {
    let step_ids: Vec<Uuid> = sqlx::query_scalar(
        r#"
        WITH RECURSIVE graph_steps(id, request_id) AS (
            SELECT step.id, step.request_id
            FROM response_steps step
            WHERE step.id = $1

            UNION

            SELECT child.id, child.request_id
            FROM response_steps child
            JOIN graph_steps member
              ON child.parent_step_id = member.id
              OR child.prev_step_id = member.id
              OR (
                  member.request_id IS NOT NULL
                  AND child.request_id = member.request_id
              )
        )
        SELECT id
        FROM graph_steps
        ORDER BY id
        "#,
    )
    .bind(group_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(database_failure)?;
    if step_ids.is_empty() {
        return Ok(GraphTopology {
            request_ids: vec![candidate_request_id],
            step_ids,
        });
    }
    let request_ids: Vec<Uuid> = sqlx::query_scalar(
        r#"
        SELECT DISTINCT request_id
        FROM response_steps
        WHERE id = ANY($1) AND request_id IS NOT NULL
        ORDER BY request_id
        "#,
    )
    .bind(&step_ids)
    .fetch_all(&mut **tx)
    .await
    .map_err(database_failure)?;
    if request_ids.is_empty() || !request_ids.contains(&candidate_request_id) {
        return Err(incomplete_graph());
    }
    Ok(GraphTopology {
        request_ids,
        step_ids,
    })
}

async fn graph_closure_is_exact(
    tx: &mut Transaction<'_, Postgres>,
    topology: &GraphTopology,
) -> MovementResult<bool> {
    sqlx::query_scalar(
        r#"
        SELECT NOT EXISTS (
            SELECT 1
            FROM response_steps step
            WHERE (
                    step.request_id = ANY($1)
                 OR step.parent_step_id = ANY($2)
                 OR step.prev_step_id = ANY($2)
            )
              AND NOT (step.id = ANY($2))
        )
        "#,
    )
    .bind(&topology.request_ids)
    .bind(&topology.step_ids)
    .fetch_one(&mut **tx)
    .await
    .map_err(database_failure)
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

async fn resolve_live_group_seed(
    tx: &mut Transaction<'_, Postgres>,
    response_id: Uuid,
) -> MovementResult<Option<(Uuid, Uuid)>> {
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
        r#"
        WITH selected_groups AS (
            SELECT COALESCE(step.parent_step_id, step.id) AS group_id
            FROM response_steps step
            WHERE step.id = $1

            UNION

            SELECT COALESCE(step.parent_step_id, step.id) AS group_id
            FROM response_steps step
            WHERE step.request_id = $1
        ),
        request_group AS (
            SELECT COALESCE(step.parent_step_id, step.id, request.id) AS group_id,
                   request.id AS request_id
            FROM requests request
            LEFT JOIN response_steps step ON step.request_id = request.id
            WHERE request.id = $1
        ),
        step_groups AS (
            SELECT selected.group_id,
                   (
                       SELECT member.request_id
                       FROM response_steps member
                       WHERE member.request_id IS NOT NULL
                         AND (
                              member.id = selected.group_id
                           OR member.parent_step_id = selected.group_id
                         )
                       ORDER BY member.id
                       LIMIT 1
                   ) AS request_id
            FROM selected_groups selected
        )
        SELECT group_id, request_id
        FROM request_group
        UNION
        SELECT group_id, request_id
        FROM step_groups
        WHERE request_id IS NOT NULL
        ORDER BY group_id, request_id
        "#,
    )
    .bind(response_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(database_failure)?;
    match rows.as_slice() {
        [] => Ok(None),
        [row] => Ok(Some(*row)),
        rows if rows.iter().all(|row| row.0 == rows[0].0) => Ok(Some(rows[0])),
        _ => Err(incomplete_graph()),
    }
}

async fn lock_live_graph_for_erasure(
    tx: &mut Transaction<'_, Postgres>,
    response_id: Uuid,
) -> MovementResult<Option<(Uuid, GraphTopology, Vec<Uuid>)>> {
    let Some((group_id, request_id)) = resolve_live_group_seed(tx, response_id).await? else {
        return Ok(None);
    };
    let initial = graph_topology(tx, group_id, request_id).await?;

    let locked_requests: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM requests WHERE id = ANY($1) ORDER BY id FOR UPDATE")
            .bind(&initial.request_ids)
            .fetch_all(&mut **tx)
            .await
            .map_err(database_failure)?;
    if locked_requests != initial.request_ids {
        return Ok(None);
    }
    if !initial.step_ids.is_empty() {
        let locked_steps: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM response_steps WHERE id = ANY($1) ORDER BY id FOR UPDATE",
        )
        .bind(&initial.step_ids)
        .fetch_all(&mut **tx)
        .await
        .map_err(database_failure)?;
        if locked_steps != initial.step_ids {
            return Ok(None);
        }
    }

    let Some((locked_group_id, locked_request_id)) =
        resolve_live_group_seed(tx, response_id).await?
    else {
        return Ok(None);
    };
    if locked_group_id != group_id || locked_request_id != request_id {
        return Err(incomplete_graph());
    }
    let revalidated = graph_topology(tx, group_id, request_id).await?;
    if revalidated != initial || !graph_closure_is_exact(tx, &revalidated).await? {
        return Err(incomplete_graph());
    }

    let request_templates: Vec<(Uuid, Option<Uuid>, bool)> = sqlx::query_as(
        r#"
        SELECT request.id,
               request.template_id,
               COALESCE(template.file_id IS NULL, FALSE) AS dedicated
        FROM requests request
        LEFT JOIN request_templates template ON template.id = request.template_id
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
        UNION
        SELECT group_id, delete_on
        FROM retained_response_step_routes
        WHERE step_id = $1
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

async fn erase_retained_graph(
    tx: &mut Transaction<'_, Postgres>,
    group_id: Uuid,
    delete_on: NaiveDate,
    max_late_writer_seconds: Option<u64>,
) -> MovementResult<u64> {
    sqlx::query(
        r#"
        SELECT pg_advisory_xact_lock(
            hashtextextended(
                'retained_response_objects.partition:' || current_schema() || ':' || $1::text,
                0
            )
        )
        "#,
    )
    .bind(delete_on)
    .execute(&mut **tx)
    .await
    .map_err(database_failure)?;

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
              AND bucket.state IN ('active', 'retiring')
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
    let step_routes: Vec<(Uuid, Uuid, NaiveDate)> = sqlx::query_as(
        "SELECT step_id, group_id, delete_on FROM retained_response_step_routes \
         WHERE group_id = $1 ORDER BY step_id FOR UPDATE",
    )
    .bind(group_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(database_failure)?;

    let header_row = sqlx::query(&format!(
        "SELECT {RETAINED_OBJECT_COLUMNS} FROM retained_response_objects object \
         WHERE object.delete_on = $1 AND object.group_id = $2 \
           AND object.object_kind = 'group' AND object.object_id = $2"
    ))
    .bind(delete_on)
    .bind(group_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(database_failure)?
    .ok_or_else(incomplete_graph)?;
    let header_row =
        RetainedResponseObjectRow::from_pg_row(&header_row).map_err(|_| incomplete_graph())?;
    let mut header = header_row.decode_group().map_err(|_| incomplete_graph())?;
    header.request_ids.sort_unstable();
    header.step_ids.sort_unstable();
    let routed_request_ids = request_routes
        .iter()
        .map(|(request_id, route_group_id, route_delete_on)| {
            if *route_group_id != group_id || *route_delete_on != delete_on {
                return Err(incomplete_graph());
            }
            Ok(*request_id)
        })
        .collect::<MovementResult<Vec<_>>>()?;
    let routed_step_ids = step_routes
        .iter()
        .map(|(step_id, route_group_id, route_delete_on)| {
            if *route_group_id != group_id || *route_delete_on != delete_on {
                return Err(incomplete_graph());
            }
            Ok(*step_id)
        })
        .collect::<MovementResult<Vec<_>>>()?;
    if routed_request_ids != header.request_ids || routed_step_ids != header.step_ids {
        return Err(incomplete_graph());
    }
    let object_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM retained_response_objects \
         WHERE delete_on = $1 AND group_id = $2",
    )
    .bind(delete_on)
    .bind(group_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(database_failure)?;
    if object_count != 1 + header.request_ids.len() as i64 + header.step_ids.len() as i64 {
        return Err(incomplete_graph());
    }

    let fence_ids = std::iter::once(group_id)
        .chain(header.request_ids.iter().copied())
        .chain(header.step_ids.iter().copied())
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
    let deleted_steps =
        sqlx::query("DELETE FROM retained_response_step_routes WHERE group_id = $1")
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
    if deleted_requests != header.request_ids.len() as u64
        || deleted_steps != header.step_ids.len() as u64
        || deleted_group != 1
    {
        return Err(incomplete_graph());
    }
    Ok(header.request_ids.len() as u64)
}

async fn delete_response_group_in_transaction(
    tx: &mut Transaction<'_, Postgres>,
    response_id: Uuid,
    expected_creator: Option<&str>,
    max_late_writer_seconds: Option<u64>,
) -> Result<u64> {
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
                return Err(RetainedResponseWriteError::NotFound.into_fusillade_error());
            }
        }
        let fence_ids = std::iter::once(group_id)
            .chain(topology.request_ids.iter().copied())
            .chain(topology.step_ids.iter().copied())
            .collect::<Vec<_>>();
        upsert_resurrection_fences(tx, &fence_ids, "erased", max_late_writer_seconds).await?;
        let deleted_steps = sqlx::query("DELETE FROM response_steps WHERE id = ANY($1)")
            .bind(&topology.step_ids)
            .execute(&mut **tx)
            .await
            .map_err(database_failure)?
            .rows_affected();
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
        if deleted_steps != topology.step_ids.len() as u64
            || deleted_requests != topology.request_ids.len() as u64
        {
            return Err(incomplete_graph());
        }
        return Ok(deleted_requests);
    }

    if let Some((group_id, delete_on)) = resolve_retained_route(tx, response_id).await? {
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
                return Err(RetainedResponseWriteError::NotFound.into_fusillade_error());
            }
        }
        let deleted =
            erase_retained_graph(tx, group_id, delete_on, max_late_writer_seconds).await?;
        return Ok(deleted);
    }

    Err(FusilladeError::RequestNotFound(RequestId(response_id)))
}

pub(crate) async fn delete_response_group<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    response_id: Uuid,
) -> Result<u64> {
    let mut tx = manager.begin_write().await.map_err(database_failure)?;
    let deleted = delete_response_group_in_transaction(
        &mut tx,
        response_id,
        None,
        manager.config().max_late_writer_seconds,
    )
    .await?;
    tx.commit().await.map_err(database_failure)?;
    Ok(deleted)
}

pub(crate) async fn delete_creator_response_groups(
    tx: &mut Transaction<'_, Postgres>,
    creator_id: &str,
    group_limit: i64,
    max_late_writer_seconds: Option<u64>,
) -> Result<u64> {
    let candidates: Vec<(Uuid, Uuid, DateTime<Utc>, String)> = sqlx::query_as(
        r#"
        WITH live_members AS (
            SELECT COALESCE(step.parent_step_id, step.id, request.id) AS group_id,
                   request.id AS response_id,
                   request.created_by,
                   request.created_at
            FROM requests request
            LEFT JOIN response_steps step ON step.request_id = request.id
            WHERE request.batch_id IS NULL
        ), live_groups AS (
            SELECT group_id,
                   (ARRAY_AGG(response_id ORDER BY response_id))[1] AS response_id,
                   MIN(created_at) AS sort_at,
                   'live'::text AS location
            FROM live_members
            GROUP BY group_id
            HAVING COALESCE(BOOL_AND(created_by = $1), FALSE)
        ), retained_groups AS (
            SELECT object.group_id,
                   (ARRAY_AGG(object.object_id ORDER BY object.object_id))[1] AS response_id,
                   MIN(object.created_at) AS sort_at,
                   'retained'::text AS location
            FROM retained_response_objects object
            JOIN retained_response_group_routes route
              ON route.group_id = object.group_id
             AND route.delete_on = object.delete_on
            JOIN retained_response_buckets bucket
              ON bucket.delete_on = object.delete_on
             AND bucket.state = 'active'
            WHERE object.object_kind = 'request'
            GROUP BY object.group_id
            HAVING COALESCE(BOOL_AND(object.created_by = $1), FALSE)
        )
        SELECT group_id, response_id, sort_at, location
        FROM (
            SELECT * FROM live_groups
            UNION ALL
            SELECT * FROM retained_groups
        ) groups
        ORDER BY sort_at, group_id, location
        LIMIT $2
        "#,
    )
    .bind(creator_id)
    .bind(group_limit)
    .fetch_all(&mut **tx)
    .await
    .map_err(database_failure)?;

    let mut deleted_requests = 0_u64;
    for (_, response_id, _, _) in candidates {
        deleted_requests += delete_response_group_in_transaction(
            tx,
            response_id,
            Some(creator_id),
            max_late_writer_seconds,
        )
        .await?;
    }
    Ok(deleted_requests)
}

async fn selected_graph_is_absent(
    tx: &mut Transaction<'_, Postgres>,
    candidate: &Candidate,
) -> MovementResult<bool> {
    sqlx::query_scalar(
        r#"
        SELECT
            NOT EXISTS (SELECT 1 FROM requests WHERE id = ANY($1))
        AND NOT EXISTS (
            SELECT 1
            FROM response_steps
            WHERE id = ANY($2)
               OR request_id = ANY($1)
               OR id = $3
               OR parent_step_id = $3
               OR prev_step_id = $3
        )
        "#,
    )
    .bind(&candidate.discovered_topology.request_ids)
    .bind(&candidate.discovered_topology.step_ids)
    .bind(candidate.group_id)
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

async fn lock_steps(
    tx: &mut Transaction<'_, Postgres>,
    step_ids: &[Uuid],
) -> MovementResult<LockSetOutcome> {
    let locked: Vec<Uuid> = sqlx::query_scalar(
        r#"
        SELECT id
        FROM response_steps
        WHERE id = ANY($1)
        ORDER BY id
        FOR UPDATE SKIP LOCKED
        "#,
    )
    .bind(step_ids)
    .fetch_all(&mut **tx)
    .await
    .map_err(database_failure)?;
    if locked == step_ids {
        return Ok(LockSetOutcome::Locked);
    }
    let existing: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM response_steps WHERE id = ANY($1)")
            .bind(step_ids)
            .fetch_one(&mut **tx)
            .await
            .map_err(database_failure)?;
    if existing != step_ids.len() as i64 {
        return Ok(LockSetOutcome::Missing);
    }
    Ok(LockSetOutcome::Skipped)
}

fn exact_expiry(terminal_at: DateTime<Utc>, seconds: u64) -> MovementResult<DateTime<Utc>> {
    let seconds = i64::try_from(seconds).map_err(|_| incomplete_graph())?;
    let duration = chrono::Duration::try_seconds(seconds).ok_or_else(incomplete_graph)?;
    terminal_at
        .checked_add_signed(duration)
        .ok_or_else(incomplete_graph)
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
        && left.head_step_id == right.head_step_id
        && left.created_by == right.created_by
        && left.service_tier == right.service_tier
        && left.state == right.state
        && left.model == right.model
        && left.created_at == right.created_at
        && left.terminal_at == right.terminal_at
        && left.step_sequence == right.step_sequence
        && left.schema_version == right.schema_version
}

async fn sha256(tx: &mut Transaction<'_, Postgres>, bytes: &[u8]) -> MovementResult<Vec<u8>> {
    sqlx::query_scalar("SELECT sha256($1::bytea)")
        .bind(bytes)
        .fetch_one(&mut **tx)
        .await
        .map_err(database_failure)
}

async fn lock_active_partition(
    tx: &mut Transaction<'_, Postgres>,
    delete_on: NaiveDate,
) -> MovementResult<bool> {
    sqlx::query(
        r#"
        SELECT pg_advisory_xact_lock(
            hashtextextended(
                'retained_response_objects.partition:' || current_schema() || ':' || $1::text,
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
        FOR UPDATE OF bucket
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
    let step_ids = expected
        .iter()
        .filter(|row| row.object_kind == RetainedObjectKind::Step)
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
                 OR (object_kind = 'step' AND object_id = ANY($4))
              )
        )
        "#,
    )
    .bind(delete_on)
    .bind(group_id)
    .bind(&request_ids)
    .bind(&step_ids)
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
                head_step_id, created_by, service_tier, state, model,
                created_at, terminal_at, step_sequence, schema_version, payload
            ) VALUES (
                $1, $2, $3, $4, $5,
                $6, $7, $8, $9, $10,
                $11, $12, $13, $14, $15
            )
            ON CONFLICT (delete_on, object_kind, object_id) DO NOTHING
            "#,
        )
        .bind(row.delete_on)
        .bind(row.group_id)
        .bind(row.object_kind.as_str())
        .bind(row.object_id)
        .bind(row.request_id)
        .bind(row.head_step_id)
        .bind(&row.created_by)
        .bind(&row.service_tier)
        .bind(&row.state)
        .bind(&row.model)
        .bind(row.created_at)
        .bind(row.terminal_at)
        .bind(row.step_sequence)
        .bind(row.schema_version)
        .bind(&row.payload)
        .execute(&mut **tx)
        .await
        .map_err(database_failure)?;
    }

    let stored = sqlx::query(
        r#"
        SELECT delete_on, group_id, object_kind, object_id, request_id,
               head_step_id, created_by, service_tier, state, model,
               created_at, terminal_at, step_sequence, schema_version, payload
        FROM retained_response_objects
        WHERE delete_on = $1 AND group_id = $2
        ORDER BY CASE object_kind
                     WHEN 'group' THEN 0
                     WHEN 'request' THEN 1
                     WHEN 'step' THEN 2
                     ELSE 3
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
    if expected_bytes.len() != actual_bytes.len()
        || sha256(tx, &expected_bytes).await? != sha256(tx, &actual_bytes).await?
    {
        return Err(RetainedResponseMovementError::IntegrityMismatch.into_fusillade_error());
    }
    Ok(())
}

async fn insert_and_verify_routes(
    tx: &mut Transaction<'_, Postgres>,
    delete_on: NaiveDate,
    group_id: Uuid,
    request_ids: &[Uuid],
    step_ids: &[Uuid],
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
    for step_id in step_ids {
        sqlx::query(
            r#"
            INSERT INTO retained_response_step_routes (step_id, group_id, delete_on)
            VALUES ($1, $2, $3)
            ON CONFLICT (step_id) DO NOTHING
            "#,
        )
        .bind(step_id)
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
    let step_routes: Vec<(Uuid, Uuid, NaiveDate)> = sqlx::query_as(
        r#"
        SELECT step_id, group_id, delete_on
        FROM retained_response_step_routes
        WHERE step_id = ANY($1) OR group_id = $2
        ORDER BY step_id
        "#,
    )
    .bind(step_ids)
    .bind(group_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(database_failure)?;
    if step_routes.len() != step_ids.len()
        || step_routes.iter().zip(step_ids).any(
            |((route_step_id, route_group_id, route_delete_on), step_id)| {
                route_step_id != step_id
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
    policy: &RetentionSweepPolicy,
    archive_now: DateTime<Utc>,
    cancel_grace_before: DateTime<Utc>,
    remaining_bytes: u64,
    allow_oversized: bool,
) -> MovementResult<MoveGraphOutcome> {
    let mut tx = manager.begin_write().await.map_err(database_failure)?;
    let group_id = current_group_id(&mut tx, candidate.request_id).await?;
    let initial_topology = graph_topology(&mut tx, group_id, candidate.request_id).await?;

    match lock_requests(&mut tx, &initial_topology.request_ids).await? {
        LockSetOutcome::Locked => {}
        LockSetOutcome::Skipped => return Ok(MoveGraphOutcome::SkippedLocked),
        LockSetOutcome::Missing => {
            if selected_graph_is_absent(&mut tx, &candidate).await? {
                return Ok(MoveGraphOutcome::AlreadyGone);
            }
            return Err(incomplete_graph());
        }
    }
    let locked_group_id = current_group_id(&mut tx, candidate.request_id).await?;
    if locked_group_id != group_id {
        return Err(incomplete_graph());
    }
    if !initial_topology.step_ids.is_empty() {
        match lock_steps(&mut tx, &initial_topology.step_ids).await? {
            LockSetOutcome::Locked => {}
            LockSetOutcome::Skipped => return Ok(MoveGraphOutcome::SkippedLocked),
            LockSetOutcome::Missing => return Err(incomplete_graph()),
        }
    }

    let revalidated_topology = graph_topology(&mut tx, group_id, candidate.request_id).await?;
    if revalidated_topology != initial_topology
        || !graph_closure_is_exact(&mut tx, &revalidated_topology).await?
    {
        return Err(incomplete_graph());
    }
    let request_ids = revalidated_topology.request_ids;
    let revalidated_step_ids = revalidated_topology.step_ids;

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

    let step_rows = sqlx::query(
        r#"
        SELECT id, request_id, prev_step_id, parent_step_id, step_kind,
               step_sequence, request_payload, response_payload, state,
               started_at, completed_at, failed_at, canceled_at, retry_attempt,
               error, created_at, updated_at
        FROM response_steps
        WHERE id = ANY($1)
        ORDER BY id
        "#,
    )
    .bind(&revalidated_step_ids)
    .fetch_all(&mut *tx)
    .await
    .map_err(database_failure)?;
    if step_rows.len() != revalidated_step_ids.len() {
        return Err(incomplete_graph());
    }
    let steps = step_rows
        .iter()
        .map(step_payload)
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
    if templates.iter().any(|template| template.file_id.is_some()) {
        return Err(incomplete_graph());
    }
    let outside_template_references: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM requests
        WHERE template_id = ANY($1) AND NOT (id = ANY($2))
        "#,
    )
    .bind(&template_ids)
    .bind(&request_ids)
    .fetch_one(&mut *tx)
    .await
    .map_err(database_failure)?;
    if outside_template_references != 0 {
        return Err(incomplete_graph());
    }

    let mut max_retention_seconds = 0_u64;
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
            && terminal_at > cancel_grace_before
        {
            return Ok(MoveGraphOutcome::Deferred);
        }
        let retention_anchor = terminal_at.max(request.created_at);
        if exact_expiry(retention_anchor, seconds)? > archive_now {
            return Ok(MoveGraphOutcome::Deferred);
        }
        max_retention_seconds = max_retention_seconds.max(seconds);
        let request_delete_on = RetentionSweepPolicy::delete_on(retention_anchor, seconds)
            .map_err(|_| incomplete_graph())?;
        delete_on = Some(delete_on.map_or(request_delete_on, |current: NaiveDate| {
            current.max(request_delete_on)
        }));
    }
    for step in &steps {
        let Some(terminal_at) = step.step.terminal_at() else {
            return Err(incomplete_graph());
        };
        let retention_anchor = terminal_at.max(step.step.created_at);
        if exact_expiry(retention_anchor, max_retention_seconds)? > archive_now {
            return Ok(MoveGraphOutcome::Deferred);
        }
        let step_delete_on =
            RetentionSweepPolicy::delete_on(retention_anchor, max_retention_seconds)
                .map_err(|_| incomplete_graph())?;
        delete_on = Some(delete_on.map_or(step_delete_on, |current: NaiveDate| {
            current.max(step_delete_on)
        }));
    }
    let delete_on = delete_on.ok_or_else(incomplete_graph)?;

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
        head_step_id: (!revalidated_step_ids.is_empty()).then_some(group_id),
        request_ids: request_ids.clone(),
        step_ids: revalidated_step_ids.clone(),
    };
    let objects = RetainedGroup::compose_rows(delete_on, group, request_payloads, steps)
        .map_err(|_| incomplete_graph())?;
    let payload_bytes = objects.iter().try_fold(0_u64, |total, row| {
        let bytes = serde_json::to_vec(&row.payload)
            .map_err(|_| RetainedResponseMovementError::IntegrityMismatch.into_fusillade_error())?;
        total
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| RetainedResponseMovementError::IntegrityMismatch.into_fusillade_error())
    })?;
    if payload_bytes > remaining_bytes && !allow_oversized {
        return Ok(MoveGraphOutcome::Deferred);
    }
    if !lock_active_partition(&mut tx, delete_on).await? {
        return Err(RetainedResponseMovementError::PartitionUnavailable.into_fusillade_error());
    }

    insert_and_verify_objects(&mut tx, delete_on, group_id, &objects).await?;
    insert_and_verify_routes(
        &mut tx,
        delete_on,
        group_id,
        &request_ids,
        &revalidated_step_ids,
    )
    .await?;

    let fence_ids = std::iter::once(group_id)
        .chain(request_ids.iter().copied())
        .chain(revalidated_step_ids.iter().copied())
        .collect::<Vec<_>>();
    upsert_resurrection_fences(
        &mut tx,
        &fence_ids,
        "archived",
        policy.max_late_writer_seconds,
    )
    .await?;

    let deleted_steps = sqlx::query("DELETE FROM response_steps WHERE id = ANY($1)")
        .bind(&revalidated_step_ids)
        .execute(&mut *tx)
        .await
        .map_err(database_failure)?
        .rows_affected();
    if deleted_steps != revalidated_step_ids.len() as u64 {
        return Err(RetainedResponseMovementError::IntegrityMismatch.into_fusillade_error());
    }
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
            .bind(&template_ids)
            .execute(&mut *tx)
            .await
            .map_err(database_failure)?
            .rows_affected();
    if deleted_templates != template_ids.len() as u64 {
        return Err(RetainedResponseMovementError::IntegrityMismatch.into_fusillade_error());
    }

    let live_members: i64 = sqlx::query_scalar(
        r#"
        SELECT
            (SELECT COUNT(*) FROM requests WHERE id = ANY($1))
          + (SELECT COUNT(*) FROM response_steps WHERE id = ANY($2))
          + (SELECT COUNT(*) FROM request_templates WHERE id = ANY($3))
        "#,
    )
    .bind(&request_ids)
    .bind(&revalidated_step_ids)
    .bind(&template_ids)
    .fetch_one(&mut *tx)
    .await
    .map_err(database_failure)?;
    if live_members != 0 {
        return Err(RetainedResponseMovementError::IntegrityMismatch.into_fusillade_error());
    }
    tx.commit().await.map_err(database_failure)?;
    Ok(MoveGraphOutcome::Archived {
        requests: request_ids.len() as u64,
        steps: revalidated_step_ids.len() as u64,
        templates: template_ids.len() as u64,
        bytes: payload_bytes,
    })
}

// Seed selection and recursive membership must share one PostgreSQL statement
// snapshot so the immutable Candidate cannot omit a concurrently deleted head's siblings.
const CANDIDATE_DISCOVERY_SQL: &str = r#"
        WITH RECURSIVE
        policy(service_tier, expire_before) AS (
            SELECT * FROM UNNEST($1::text[], $2::timestamptz[])
        ),
        candidate_seed AS MATERIALIZED (
            SELECT candidate.id AS request_id,
                   COALESCE(direct.parent_step_id, direct.id, candidate.id) AS group_id,
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
                  AND NOT (request.id = ANY($3))
                  AND CASE request.state
                          WHEN 'completed' THEN request.completed_at
                          WHEN 'failed' THEN request.failed_at
                          WHEN 'canceled' THEN request.canceled_at
                      END <= policy.expire_before
                ORDER BY CASE request.state
                             WHEN 'completed' THEN request.completed_at
                             WHEN 'failed' THEN request.failed_at
                             WHEN 'canceled' THEN request.canceled_at
                         END,
                         request.id
                LIMIT 1
            ) candidate
            LEFT JOIN response_steps direct ON direct.request_id = candidate.id
            ORDER BY candidate.terminal_at, candidate.id
            LIMIT 1
        ),
        graph_steps(id, request_id) AS (
            SELECT step.id, step.request_id
            FROM candidate_seed candidate
            JOIN response_steps step ON step.id = candidate.group_id

            UNION

            SELECT child.id, child.request_id
            FROM response_steps child
            JOIN graph_steps member
              ON child.parent_step_id = member.id
              OR child.prev_step_id = member.id
              OR (
                  member.request_id IS NOT NULL
                  AND child.request_id = member.request_id
              )
        )
        SELECT candidate.request_id,
               candidate.group_id,
               step.id AS step_id,
               step.request_id AS step_request_id
        FROM candidate_seed candidate
        LEFT JOIN graph_steps step ON TRUE
        ORDER BY step.id
        "#;

async fn next_candidate<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    tiers: &[String],
    cutoffs: &[DateTime<Utc>],
    excluded_request_ids: &[Uuid],
) -> MovementResult<Option<Candidate>> {
    let rows: Vec<(Uuid, Uuid, Option<Uuid>, Option<Uuid>)> =
        sqlx::query_as(CANDIDATE_DISCOVERY_SQL)
            .bind(tiers)
            .bind(cutoffs)
            .bind(excluded_request_ids)
            .fetch_all(manager.read_executor())
            .await
            .map_err(database_failure)?;
    let Some((request_id, group_id, _, _)) = rows.first().copied() else {
        return Ok(None);
    };
    if rows.iter().any(|(row_request_id, row_group_id, _, _)| {
        *row_request_id != request_id || *row_group_id != group_id
    }) {
        return Err(incomplete_graph());
    }
    let step_ids = rows
        .iter()
        .filter_map(|(_, _, step_id, _)| *step_id)
        .collect();
    let mut request_ids = rows
        .into_iter()
        .filter_map(|(_, _, _, step_request_id)| step_request_id)
        .collect::<Vec<_>>();
    request_ids.sort_unstable();
    request_ids.dedup();
    if !request_ids.contains(&request_id) {
        request_ids.push(request_id);
        request_ids.sort_unstable();
    }
    Ok(Some(Candidate {
        request_id,
        group_id,
        discovered_topology: GraphTopology {
            request_ids,
            step_ids,
        },
    }))
}

pub(crate) async fn archive_terminal_batchless_responses<P: PoolProvider>(
    manager: &PostgresRequestManager<P>,
    policy: &RetentionSweepPolicy,
    cancel_grace_before: DateTime<Utc>,
    max_groups: i64,
    max_bytes: i64,
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
    let archive_now: DateTime<Utc> = sqlx::query_scalar("SELECT statement_timestamp()")
        .fetch_one(manager.read_executor())
        .await
        .map_err(database_failure)?;
    let mut tiers = Vec::with_capacity(policy.batchless_seconds_by_service_tier.len());
    let mut cutoffs = Vec::with_capacity(policy.batchless_seconds_by_service_tier.len());
    for (tier, seconds) in &policy.batchless_seconds_by_service_tier {
        tiers.push(tier.clone());
        let seconds = i64::try_from(*seconds).map_err(|_| {
            FusilladeError::ValidationError("automated retention period is too large".to_owned())
        })?;
        let duration = chrono::Duration::try_seconds(seconds).ok_or_else(|| {
            FusilladeError::ValidationError("automated retention period is too large".to_owned())
        })?;
        cutoffs.push(archive_now.checked_sub_signed(duration).ok_or_else(|| {
            FusilladeError::ValidationError("automated retention cutoff is out of range".to_owned())
        })?);
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
        let Some(candidate) =
            next_candidate(manager, &tiers, &cutoffs, &excluded_request_ids).await?
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
    for candidate in candidates {
        let remaining_bytes = (max_bytes as u64).saturating_sub(outcome.bytes_archived);
        match move_graph(
            manager,
            candidate,
            policy,
            archive_now,
            cancel_grace_before,
            remaining_bytes,
            outcome.groups_archived == 0,
        )
        .await?
        {
            MoveGraphOutcome::Archived {
                requests,
                steps,
                templates,
                bytes,
            } => {
                outcome.groups_archived += 1;
                outcome.requests_archived += requests;
                outcome.steps_archived += steps;
                outcome.templates_archived += templates;
                outcome.bytes_archived += bytes;
                if outcome.groups_archived == max_groups as u64 {
                    break;
                }
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

    use crate::request::RequestId;
    use crate::response_step::{ResponseStep, StepId, StepKind, StepState};

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
                head_step_id, created_by, service_tier, state, model,
                created_at, terminal_at, step_sequence, schema_version, payload
            ) VALUES (
                $1, $2, 'request', $3, $3,
                NULL, 'planner-owner', 'flex', 'completed', 'planner-model',
                '2026-08-11T09:00:00Z', '2026-08-12T10:00:00Z', NULL, 1, '{}'::jsonb
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
            .bind(vec![timestamp("2026-08-08T00:00:00Z")])
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

    fn model_step_id() -> Uuid {
        Uuid::from_u128(0x33333333333333333333333333333333)
    }

    fn tool_step_id() -> Uuid {
        Uuid::from_u128(0x44444444444444444444444444444444)
    }

    fn group_id() -> Uuid {
        model_step_id()
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

    fn model_step() -> ResponseStep {
        ResponseStep {
            id: StepId(model_step_id()),
            request_id: Some(RequestId(request_id())),
            prev_step_id: None,
            parent_step_id: None,
            step_kind: StepKind::ModelCall,
            step_sequence: 10,
            request_payload: json!({"input": "model-request-secret"}),
            response_payload: Some(json!({"output": "model-response-secret"})),
            state: StepState::Completed,
            started_at: Some(timestamp("2030-01-02T03:04:07.123456Z")),
            completed_at: Some(timestamp("2030-01-02T03:04:11.456789Z")),
            failed_at: None,
            canceled_at: None,
            retry_attempt: 2,
            error: None,
            created_at: timestamp("2030-01-02T03:04:06.000001Z"),
            updated_at: timestamp("2030-01-02T03:04:11.456790Z"),
        }
    }

    fn tool_step() -> ResponseStep {
        ResponseStep {
            id: StepId(tool_step_id()),
            request_id: None,
            prev_step_id: Some(StepId(model_step_id())),
            parent_step_id: Some(StepId(model_step_id())),
            step_kind: StepKind::ToolCall,
            step_sequence: 11,
            request_payload: json!({"input": "tool-request-secret"}),
            response_payload: Some(json!({"output": "tool-response-secret"})),
            state: StepState::Failed,
            started_at: Some(timestamp("2030-01-02T03:04:12.000001Z")),
            completed_at: None,
            failed_at: Some(timestamp("2030-01-02T03:04:13.000001Z")),
            canceled_at: Some(timestamp("2030-01-02T03:04:14.000001Z")),
            retry_attempt: 3,
            error: Some(json!({"message": "tool-error-secret"})),
            created_at: timestamp("2030-01-02T03:04:12.000000Z"),
            updated_at: timestamp("2030-01-02T03:04:14.000002Z"),
        }
    }

    fn branching_tool_step() -> ResponseStep {
        let mut step = tool_step();
        step.id = StepId(branching_tool_step_id());
        step.step_sequence = 12;
        step
    }

    fn other_request_id() -> Uuid {
        Uuid::from_u128(0x66666666666666666666666666666666)
    }

    fn other_template_id() -> Uuid {
        Uuid::from_u128(0x77777777777777777777777777777777)
    }

    fn other_step_id() -> Uuid {
        Uuid::from_u128(0x88888888888888888888888888888888)
    }

    fn branching_tool_step_id() -> Uuid {
        Uuid::from_u128(0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)
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

    fn complete_multi_step_group() -> (
        RetainedGroup,
        RetainedRequestPayloadV1,
        RetainedStepPayloadV1,
        RetainedStepPayloadV1,
    ) {
        (
            RetainedGroup {
                group_id: group_id(),
                head_step_id: Some(model_step_id()),
                request_ids: vec![request_id()],
                step_ids: vec![model_step_id(), tool_step_id()],
            },
            request_payload(),
            RetainedStepPayloadV1::from_response_step(&model_step()),
            RetainedStepPayloadV1::from_response_step(&tool_step()),
        )
    }

    #[test]
    fn request_payload_round_trip_preserves_detail_template_and_unknown_future_fields() {
        // Mutation caught: omitting a nullable request/template field, mapping the
        // template body to the wrong RequestDetail field, or rejecting a future
        // additive JSON key would lose archived API data during a V1 read.
        let payload = request_payload();
        let row = RetainedResponseObjectRow::request(delete_on(), group_id(), None, &payload)
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
        let mut row = RetainedResponseObjectRow::request(delete_on(), group_id(), None, &payload)
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
            RetainedResponseObjectRow::request(delete_on(), group_id(), None, &payload)
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
            RetainedResponseObjectRow::request(delete_on(), group_id(), None, &payload)
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
    fn model_and_tool_steps_round_trip_with_relationships_and_nullable_request_id() {
        // Mutation caught: treating every step as a model call drops the
        // request-less tool row or its parent/previous-step relationships.
        let model = model_step();
        let tool = tool_step();
        let model_payload = RetainedStepPayloadV1::from_response_step(&model);
        let tool_payload = RetainedStepPayloadV1::from_response_step(&tool);

        let decoded_model = RetainedResponseObjectRow::step(
            delete_on(),
            group_id(),
            Some(model_step_id()),
            &model_payload,
        )
        .expect("model fixture must encode")
        .decode_step()
        .expect("model V1 payload must decode");
        let decoded_tool = RetainedResponseObjectRow::step(
            delete_on(),
            group_id(),
            Some(model_step_id()),
            &tool_payload,
        )
        .expect("tool fixture must encode")
        .decode_step()
        .expect("tool V1 payload must decode");

        let restored_model = decoded_model.to_response_step();
        assert_eq!(restored_model.id, StepId(model_step_id()));
        assert_eq!(restored_model.request_id, Some(RequestId(request_id())));
        assert_eq!(restored_model.prev_step_id, None);
        assert_eq!(restored_model.parent_step_id, None);
        assert_eq!(restored_model.step_kind, StepKind::ModelCall);
        assert_eq!(restored_model.step_sequence, 10);
        assert_eq!(restored_model.request_payload, model.request_payload);
        assert_eq!(restored_model.response_payload, model.response_payload);
        assert_eq!(restored_model.state, StepState::Completed);
        assert_eq!(restored_model.started_at, model.started_at);
        assert_eq!(restored_model.completed_at, model.completed_at);
        assert_eq!(restored_model.failed_at, None);
        assert_eq!(restored_model.canceled_at, None);
        assert_eq!(restored_model.retry_attempt, 2);
        assert_eq!(restored_model.error, None);
        assert_eq!(restored_model.created_at, model.created_at);
        assert_eq!(restored_model.updated_at, model.updated_at);

        let restored_tool = decoded_tool.to_response_step();
        assert_eq!(restored_tool.id, StepId(tool_step_id()));
        assert_eq!(restored_tool.request_id, None);
        assert_eq!(restored_tool.prev_step_id, Some(StepId(model_step_id())));
        assert_eq!(restored_tool.parent_step_id, Some(StepId(model_step_id())));
        assert_eq!(restored_tool.step_kind, StepKind::ToolCall);
        assert_eq!(restored_tool.step_sequence, 11);
        assert_eq!(restored_tool.request_payload, tool.request_payload);
        assert_eq!(restored_tool.response_payload, tool.response_payload);
        assert_eq!(restored_tool.state, StepState::Failed);
        assert_eq!(restored_tool.started_at, tool.started_at);
        assert_eq!(restored_tool.completed_at, None);
        assert_eq!(
            restored_tool.failed_at,
            Some(timestamp("2030-01-02T03:04:13.000001Z"))
        );
        assert_eq!(
            restored_tool.canceled_at,
            Some(timestamp("2030-01-02T03:04:14.000001Z"))
        );
        assert_eq!(restored_tool.retry_attempt, 3);
        assert_eq!(restored_tool.error, tool.error);
        assert_eq!(restored_tool.created_at, tool.created_at);
        assert_eq!(restored_tool.updated_at, tool.updated_at);
    }

    #[test]
    fn tagged_group_composition_preserves_exact_member_lists_and_rejects_partial_lists() {
        // Mutation caught: producing a group header with a missing member list
        // would let a later mover commit a partial graph.
        let request = request_payload();
        let model = RetainedStepPayloadV1::from_response_step(&model_step());
        let tool = RetainedStepPayloadV1::from_response_step(&tool_step());
        let group = RetainedGroup {
            group_id: group_id(),
            head_step_id: Some(model_step_id()),
            request_ids: vec![request_id()],
            step_ids: vec![model_step_id(), tool_step_id()],
        };

        let rows = RetainedGroup::compose_rows(
            delete_on(),
            group.clone(),
            vec![request.clone()],
            vec![model.clone(), tool.clone()],
        )
        .expect("complete group must compose");
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].object_kind, RetainedObjectKind::Group);
        assert_eq!(rows[1].object_kind, RetainedObjectKind::Request);
        assert_eq!(rows[2].object_kind, RetainedObjectKind::Step);
        assert_eq!(rows[3].object_kind, RetainedObjectKind::Step);
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
        assert_eq!(
            rows[3]
                .decode_step()
                .expect("tool member must decode")
                .step
                .request_id,
            None
        );

        let error = RetainedGroup::compose_rows(
            delete_on(),
            RetainedGroup {
                step_ids: vec![model_step_id()],
                ..group
            },
            vec![request],
            vec![model, tool],
        )
        .expect_err("missing a step from the header must fail closed");
        assert_eq!(error, RetainedResponseSerializationError::IncompleteGroup);
    }

    #[test]
    fn group_decoder_rejects_duplicate_request_and_step_ids() {
        // Mutation caught: accepting duplicate IDs in an archived group header
        // can make one graph member appear as two independently routed rows.
        let (group, _, _, _) = complete_multi_step_group();
        let row = RetainedResponseObjectRow::group(delete_on(), &group)
            .expect("group fixture must encode");

        let mut duplicate_request = row.clone();
        duplicate_request.payload["request_ids"] = json!([request_id(), request_id()]);
        let request_error = duplicate_request
            .decode_group()
            .expect_err("duplicate request IDs must fail closed");
        assert_eq!(
            request_error,
            RetainedResponseSerializationError::IncompleteGroup
        );

        let mut duplicate_step = row;
        duplicate_step.payload["step_ids"] = json!([model_step_id(), model_step_id()]);
        let step_error = duplicate_step
            .decode_group()
            .expect_err("duplicate step IDs must fail closed");
        assert_eq!(
            step_error,
            RetainedResponseSerializationError::IncompleteGroup
        );
    }

    #[test]
    fn compose_rows_rejects_zero_or_multiple_logical_heads() {
        // Mutation caught: a group with no unique root cannot be reconstructed
        // as the public response chain identified by its head step.
        let (group, request, mut model, tool) = complete_multi_step_group();
        model.step.parent_step_id = Some(model_step_id());
        let no_head = RetainedGroup::compose_rows(
            delete_on(),
            group.clone(),
            vec![request.clone()],
            vec![model, tool.clone()],
        )
        .expect_err("a multi-step group must contain its declared root");
        assert_eq!(no_head, RetainedResponseSerializationError::IncompleteGroup);

        let mut second_head = tool;
        second_head.step.parent_step_id = None;
        let multiple_heads = RetainedGroup::compose_rows(
            delete_on(),
            group,
            vec![request],
            vec![
                RetainedStepPayloadV1::from_response_step(&model_step()),
                second_head,
            ],
        )
        .expect_err("a multi-step group must have exactly one root");
        assert_eq!(
            multiple_heads,
            RetainedResponseSerializationError::IncompleteGroup
        );
    }

    #[test]
    fn compose_rows_rejects_foreign_parent_and_predecessor_links() {
        // Mutation caught: accepting an edge that leaves the group permits a
        // partial archived graph while the linked member remains live.
        let (group, request, model, mut tool) = complete_multi_step_group();
        tool.step.parent_step_id = Some(other_step_id());
        let foreign_parent = RetainedGroup::compose_rows(
            delete_on(),
            group.clone(),
            vec![request.clone()],
            vec![model.clone(), tool],
        )
        .expect_err("every descendant must name the declared head as parent");
        assert_eq!(
            foreign_parent,
            RetainedResponseSerializationError::IncompleteGroup
        );

        let mut mismatched_parent = RetainedStepPayloadV1::from_response_step(&tool_step());
        mismatched_parent.step.parent_step_id = Some(tool_step_id());
        let nonhead_parent = RetainedGroup::compose_rows(
            delete_on(),
            group.clone(),
            vec![request.clone()],
            vec![model.clone(), mismatched_parent],
        )
        .expect_err("every nonhead must name the declared head as parent");
        assert_eq!(
            nonhead_parent,
            RetainedResponseSerializationError::IncompleteGroup
        );

        let mut tool = RetainedStepPayloadV1::from_response_step(&tool_step());
        tool.step.prev_step_id = Some(other_step_id());
        let foreign_predecessor =
            RetainedGroup::compose_rows(delete_on(), group, vec![request], vec![model, tool])
                .expect_err("a predecessor outside the group must fail closed");
        assert_eq!(
            foreign_predecessor,
            RetainedResponseSerializationError::IncompleteGroup
        );
    }

    #[test]
    fn compose_rows_rejects_a_head_predecessor_to_its_descendant() {
        // Mutation caught: a root with a predecessor is not a tree root, even
        // when that predecessor is an otherwise valid in-group descendant.
        let (group, request, mut head, tool) = complete_multi_step_group();
        head.step.prev_step_id = Some(tool_step_id());

        let error =
            RetainedGroup::compose_rows(delete_on(), group, vec![request], vec![head, tool])
                .expect_err("the declared head must not have a predecessor");

        assert_eq!(error, RetainedResponseSerializationError::IncompleteGroup);
    }

    #[test]
    fn compose_rows_rejects_a_nonhead_without_a_predecessor() {
        // Mutation caught: a descendant without its unique tree edge becomes
        // disconnected from the retained response despite sharing its parent.
        let (group, request, head, mut tool) = complete_multi_step_group();
        tool.step.prev_step_id = None;

        let error =
            RetainedGroup::compose_rows(delete_on(), group, vec![request], vec![head, tool])
                .expect_err("every nonhead must have a predecessor");

        assert_eq!(error, RetainedResponseSerializationError::IncompleteGroup);
    }

    #[test]
    fn compose_rows_rejects_an_in_group_predecessor_cycle() {
        // Mutation caught: membership checks alone cannot distinguish a cycle
        // disconnected from the declared head from a retained response tree.
        let (mut group, request, head, mut first_tool) = complete_multi_step_group();
        let mut second_tool = RetainedStepPayloadV1::from_response_step(&branching_tool_step());
        first_tool.step.prev_step_id = Some(branching_tool_step_id());
        second_tool.step.prev_step_id = Some(tool_step_id());
        group.step_ids.push(branching_tool_step_id());

        let error = RetainedGroup::compose_rows(
            delete_on(),
            group,
            vec![request],
            vec![head, first_tool, second_tool],
        )
        .expect_err("every predecessor walk must terminate at the declared head");

        assert_eq!(error, RetainedResponseSerializationError::IncompleteGroup);
    }

    #[test]
    fn compose_rows_accepts_a_branching_predecessor_tree() {
        // Regression caught: requiring a linear predecessor chain would reject
        // valid parallel tool calls that share the head as their predecessor.
        let (mut group, request, head, first_tool) = complete_multi_step_group();
        let second_tool = RetainedStepPayloadV1::from_response_step(&branching_tool_step());
        group.step_ids.push(branching_tool_step_id());

        let rows = RetainedGroup::compose_rows(
            delete_on(),
            group,
            vec![request],
            vec![head, first_tool, second_tool],
        )
        .expect("parallel children of one predecessor must remain valid");

        assert_eq!(rows.len(), 5);
    }

    #[test]
    fn compose_rows_requires_a_one_to_one_model_request_membership() {
        // Mutation caught: model steps can only represent their own backing
        // request, while tool steps intentionally carry no request ID.
        let (group, request, model, mut tool) = complete_multi_step_group();
        tool.step.step_kind = StepKind::ModelCall;
        tool.step.request_id = Some(request_id());
        let duplicate_model_request = RetainedGroup::compose_rows(
            delete_on(),
            group.clone(),
            vec![request.clone()],
            vec![model.clone(), tool],
        )
        .expect_err("one request must not back multiple model steps");
        assert_eq!(
            duplicate_model_request,
            RetainedResponseSerializationError::IncompleteGroup
        );

        let mut foreign_model = RetainedStepPayloadV1::from_response_step(&tool_step());
        foreign_model.step.step_kind = StepKind::ModelCall;
        foreign_model.step.request_id = Some(other_request_id());
        let missing_request = RetainedGroup::compose_rows(
            delete_on(),
            group.clone(),
            vec![request.clone()],
            vec![model.clone(), foreign_model],
        )
        .expect_err("every model request ID must be a retained request member");
        assert_eq!(
            missing_request,
            RetainedResponseSerializationError::IncompleteGroup
        );

        let mut extra_group = group;
        extra_group.request_ids.push(other_request_id());
        let unrepresented_request = RetainedGroup::compose_rows(
            delete_on(),
            extra_group,
            vec![request, other_request_payload()],
            vec![
                model,
                RetainedStepPayloadV1::from_response_step(&tool_step()),
            ],
        )
        .expect_err("every retained request must belong to a model step");
        assert_eq!(
            unrepresented_request,
            RetainedResponseSerializationError::IncompleteGroup
        );
    }

    #[test]
    fn request_and_step_decoders_reject_group_or_head_routing_tampering() {
        // Mutation caught: changing only indexed route columns must not attach
        // an otherwise valid content payload to another response graph.
        let (group, request, model, tool) = complete_multi_step_group();
        let rows =
            RetainedGroup::compose_rows(delete_on(), group, vec![request], vec![model, tool])
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

        let step_head_tamper = RetainedResponseObjectRow {
            head_step_id: Some(other_step_id()),
            ..rows[2].clone()
        }
        .decode_step()
        .expect_err("step rows must bind their head in payload");
        assert_eq!(
            step_head_tamper,
            RetainedResponseSerializationError::RowMetadataMismatch
        );

        let mut step_payload_tamper = rows[2].clone();
        step_payload_tamper.payload["head_step_id"] = json!(other_step_id());
        let step_payload_error = step_payload_tamper
            .decode_step()
            .expect_err("step payload routing must match its row");
        assert_eq!(
            step_payload_error,
            RetainedResponseSerializationError::RowMetadataMismatch
        );
    }

    #[test]
    fn decoders_distinguish_missing_nullable_fields_from_explicit_null() {
        // Mutation caught: derive-based Option deserialization turns a
        // truncated V1 field into None, making a malformed archive look valid.
        let request = request_payload();
        let request_row =
            RetainedResponseObjectRow::request(delete_on(), group_id(), None, &request)
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

        let step = RetainedStepPayloadV1::from_response_step(&tool_step());
        let step_row =
            RetainedResponseObjectRow::step(delete_on(), group_id(), Some(model_step_id()), &step)
                .expect("step fixture must encode");
        let mut explicit_step_null = step_row.clone();
        explicit_step_null.payload["step"]["prev_step_id"] = serde_json::Value::Null;
        explicit_step_null
            .decode_step()
            .expect("an explicit nullable step null must decode");
        let mut missing_step_predecessor = step_row.clone();
        missing_step_predecessor.payload["step"]
            .as_object_mut()
            .expect("step fixture is an object")
            .remove("prev_step_id");
        let missing_predecessor = missing_step_predecessor
            .decode_step()
            .expect_err("an omitted nullable predecessor must fail closed");
        assert_eq!(
            missing_predecessor,
            RetainedResponseSerializationError::MalformedPayload
        );

        let mut missing_step_response = step_row;
        missing_step_response.payload["step"]
            .as_object_mut()
            .expect("step fixture is an object")
            .remove("response_payload");
        let missing_response = missing_step_response
            .decode_step()
            .expect_err("an omitted nullable response payload must fail closed");
        assert_eq!(
            missing_response,
            RetainedResponseSerializationError::MalformedPayload
        );

        let mut missing_request_route =
            RetainedResponseObjectRow::request(delete_on(), group_id(), None, &request_payload())
                .expect("request fixture must encode");
        missing_request_route
            .payload
            .as_object_mut()
            .expect("request fixture is an object")
            .remove("head_step_id");
        let missing_route = missing_request_route
            .decode_request()
            .expect_err("an omitted nullable route field must fail closed");
        assert_eq!(
            missing_route,
            RetainedResponseSerializationError::MalformedPayload
        );

        let group = RetainedGroup {
            group_id: request_id(),
            head_step_id: None,
            request_ids: vec![request_id()],
            step_ids: Vec::new(),
        };
        let group_row = RetainedResponseObjectRow::group(delete_on(), &group)
            .expect("request-only group fixture must encode");
        group_row
            .decode_group()
            .expect("an explicit nullable group head must decode");
        let mut missing_group_head = group_row;
        missing_group_head
            .payload
            .as_object_mut()
            .expect("group fixture is an object")
            .remove("head_step_id");
        let missing_head = missing_group_head
            .decode_group()
            .expect_err("an omitted nullable group head must fail closed");
        assert_eq!(
            missing_head,
            RetainedResponseSerializationError::MalformedPayload
        );
    }

    #[test]
    fn decoders_reject_unknown_versions_malformed_payloads_and_object_kinds_without_content() {
        // Mutation caught: decoding a future version as V1, accepting malformed
        // JSON, or trusting a mismatched row tag would surface the wrong object.
        let payload = request_payload();
        let row = RetainedResponseObjectRow::request(delete_on(), group_id(), None, &payload)
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
            object_kind: RetainedObjectKind::Step,
            ..row.clone()
        }
        .decode_request()
        .expect_err("a step row must not decode through the request decoder");
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

        let unsupported_kind = RetainedObjectKind::try_from("untrusted-kind")
            .expect_err("unknown database object kinds must fail closed");
        assert_eq!(
            unsupported_kind,
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
