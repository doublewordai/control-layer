//! Versioned retained-response object snapshots.
//!
//! This module only defines the representation and decoding boundary for
//! retained response graphs. It does not select, move, route, or retire data.

use std::fmt;

use chrono::{DateTime, NaiveDate, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::error::{FusilladeError, Result};
use crate::request::{RequestDetail, RequestId};
use crate::response_step::{ResponseStep, StepId, StepKind, StepState};

/// The only retained response payload version this binary knows how to read.
pub(crate) const RETAINED_RESPONSE_SCHEMA_V1: i16 = 1;

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
        self.request.terminal_at()?;
        Ok(())
    }

    /// Reconstruct the existing batchless request-detail model from its V1 snapshot.
    pub(crate) fn to_request_detail(
        &self,
    ) -> std::result::Result<RequestDetail, RetainedResponseSerializationError> {
        self.validate()?;
        let duration_ms = match (self.request.started_at, self.request.completed_at) {
            (Some(started_at), Some(completed_at)) => Some(
                (completed_at.timestamp_micros() - started_at.timestamp_micros()) as f64 / 1_000.0,
            ),
            _ => None,
        };
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
        payload.validate()?;
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
        payload.validate()?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, NaiveDate, Utc};
    use serde_json::json;
    use uuid::Uuid;

    use crate::request::RequestId;
    use crate::response_step::{ResponseStep, StepId, StepKind, StepState};

    fn timestamp(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .expect("fixture timestamp must be valid")
            .to_utc()
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
