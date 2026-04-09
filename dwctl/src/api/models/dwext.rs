//! Doubleword API extensions (`dwext`).
//!
//! Following NVIDIA's `nvext` pattern, all Doubleword-specific fields live under
//! a top-level `dwext` key. This preserves OpenAI API compatibility — any standard
//! client ignores unknown top-level keys, while Doubleword clients can opt in to
//! extended functionality.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Doubleword extension fields on batch responses.
///
/// Returned as `"dwext": { ... }` at the top level of batch objects.
/// Only present when there is Doubleword-specific data to surface.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct BatchDwExtResponse {
    /// How the batch was created: "api", "frontend", or "sync".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,

    /// Name of the source connection (when source = "sync").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_name: Option<String>,

    /// Source connection ID (when source = "sync").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,

    /// Original external file key (when source = "sync").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_file: Option<String>,

    /// Sync operation ID that created this batch (when source = "sync").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_id: Option<String>,
}

impl BatchDwExtResponse {
    /// Returns true if this extension has any data worth serializing.
    pub fn is_empty(&self) -> bool {
        self.source.is_none()
            && self.source_name.is_none()
            && self.source_id.is_none()
            && self.source_file.is_none()
            && self.sync_id.is_none()
    }
}
