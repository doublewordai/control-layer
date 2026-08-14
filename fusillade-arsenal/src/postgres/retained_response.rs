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

/// A complete immutable copy of a `requests` row.
///
/// The request-to-template relationship remains explicit even though the
/// graph mover only accepts the dedicated batchless-template form.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Clone, PartialEq, Serialize, Deserialize)]
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

/// A complete immutable copy of a `response_steps` row.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
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

/// Content-free group header that proves the exact retained graph membership.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RetainedGroup {
    pub(crate) group_id: Uuid,
    pub(crate) head_step_id: Option<Uuid>,
    pub(crate) request_ids: Vec<Uuid>,
    pub(crate) step_ids: Vec<Uuid>,
}

impl RetainedGroup {
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
        let request_ids = requests
            .iter()
            .map(|payload| payload.request.id)
            .collect::<Vec<_>>();
        let step_ids = steps
            .iter()
            .map(|payload| payload.step.id)
            .collect::<Vec<_>>();
        let complete = group.request_ids == request_ids
            && group.step_ids == step_ids
            && !has_duplicate_ids(&group.request_ids)
            && !has_duplicate_ids(&group.step_ids)
            && match group.head_step_id {
                Some(head_step_id) => {
                    group.group_id == head_step_id && group.step_ids.contains(&head_step_id)
                }
                None => {
                    group.step_ids.is_empty()
                        && group.request_ids.len() == 1
                        && group.request_ids[0] == group.group_id
                }
            };
        if !complete {
            return Err(RetainedResponseSerializationError::IncompleteGroup);
        }

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
            payload: to_payload(payload)?,
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
            payload: to_payload(payload)?,
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
        Ok(group)
    }

    pub(crate) fn decode_request(
        &self,
    ) -> std::result::Result<RetainedRequestPayloadV1, RetainedResponseSerializationError> {
        self.require_kind(RetainedObjectKind::Request)?;
        let payload: RetainedRequestPayloadV1 = self.decode_payload()?;
        payload.validate()?;
        if self.object_id != payload.request.id
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
        let payload: RetainedStepPayloadV1 = self.decode_payload()?;
        if self.object_id != payload.step.id
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
