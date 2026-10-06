//! Exact ingress names, independent of upstream names and serving-class identity.

use chrono::{DateTime, Utc};
use sqlx::FromRow;

use crate::types::{DeploymentId, ModelServingClassId};

/// An optional exact synonym for a class of the canonical model.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct ModelAlias {
    pub alias: String,
    pub deployed_model_id: DeploymentId,
    pub serving_class_id: ModelServingClassId,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A synonym's stable identity, with the canonical name used by existing paths.
/// No routing, authorization or tariff state belongs in this startup snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelAliasTarget {
    pub alias: String,
    pub deployed_model_id: DeploymentId,
    pub serving_class_id: ModelServingClassId,
    pub canonical_alias: String,
    pub class_key: String,
}
