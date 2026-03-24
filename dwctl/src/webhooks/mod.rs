//! Webhook notification system for batch and platform events.
//!
//! - [`signing`]: HMAC-SHA256 signature generation per Standard Webhooks spec
//! - [`events`]: Event types, scopes, and payload builders
//! - [`dispatcher`]: Claim/sign/send/result loop called by the notification poller
//!
//! All webhook delivery creation is centralized in the notification poller
//! (`crate::notifications`). Platform events are detected via PG LISTEN/NOTIFY
//! triggers and polling; batch completion events via fusillade polling.

pub mod dispatcher;
pub mod events;
pub mod signing;

pub use dispatcher::WebhookDispatcher;
pub use events::{WebhookEvent, WebhookEventType, WebhookScope};
pub use signing::{generate_secret, sign_payload};
