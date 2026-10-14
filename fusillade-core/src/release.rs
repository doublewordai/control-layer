//! Release of the daemon's scheduling tolerations.
//!
//! The daemon sends batch requests with scheduling tolerations that the
//! upstream inference backend understands; a backend may, for example, use
//! them to keep batch work off capacity reserved for other traffic. Near SLA
//! failure, meeting the deadline matters more than where the request runs, so
//! the claim releases the tolerations of a request past its deadline or inside
//! the batch-claim deadline ramp.

/// Settings the daemon hands its storage for the claim-time decision.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TolerationsReleaseSettings {
    /// The daemon sends scheduling tolerations; without them the claim makes
    /// no decision.
    pub tolerations_enabled: bool,
}
