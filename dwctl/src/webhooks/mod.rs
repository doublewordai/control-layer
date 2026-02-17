//! Webhook notification system for batch events.
//!
//! - [`signing`]: HMAC-SHA256 signature generation per Standard Webhooks spec
//! - [`events`]: Event types and payload builders for batch events
//! - [`dispatcher`]: Claim/sign/send/result loop called by the notification poller

pub mod dispatcher;
pub mod events;
pub mod signing;

pub use dispatcher::WebhookDispatcher;
pub use events::{WebhookEvent, WebhookEventType};
pub use signing::{generate_secret, sign_payload};
