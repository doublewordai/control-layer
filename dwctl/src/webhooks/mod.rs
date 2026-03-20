//! Webhook notification system for batch and platform events.
//!
//! - [`signing`]: HMAC-SHA256 signature generation per Standard Webhooks spec
//! - [`events`]: Event types, scopes, and payload builders
//! - [`dispatcher`]: Claim/sign/send/result loop called by the notification poller
//! - [`emit`]: Fire-and-forget platform event delivery helper

pub mod dispatcher;
pub mod emit;
pub mod events;
pub mod signing;

pub use dispatcher::WebhookDispatcher;
pub use events::{WebhookEvent, WebhookEventType, WebhookScope};
pub use signing::{generate_secret, sign_payload};
