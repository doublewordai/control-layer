//! Additive storage for canonical serving classes, separate from realtime/flex/batch tiers.
//!
//! These rows are not yet consumed by request resolution or catalog reconciliation.
//! The initial offerings use `standard` and `fast`; public entry points are the
//! model's existing alias and that alias with `:fast`. Additional registered names
//! select the same class. Upstream selection is configuration, not class identity.

use chrono::{DateTime, Utc};
use sqlx::FromRow;

use crate::types::{DeploymentId, InferenceEndpointId, ModelServingClassId};

/// One stable service identity and its default upstream within an existing model.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct ModelServingClass {
    pub id: ModelServingClassId,
    pub deployed_model_id: DeploymentId,
    /// Stable catalog key (`standard` or `fast` initially), independent of display names.
    pub class_key: String,
    pub display_name: String,
    pub inference_endpoint_id: InferenceEndpointId,
    /// Configured upstream name; independent of the public alias and class key.
    pub upstream_model_name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
