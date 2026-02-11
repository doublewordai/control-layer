//! Webhook notification system for batch events.
//!
//! This module provides Standard Webhooks-compliant notifications for batch terminal states
//! (completed, failed, cancelled). It includes:
//!
//! - [`signing`]: HMAC-SHA256 signature generation per Standard Webhooks spec
//! - [`events`]: Event types and payload builders for batch events
//! - [`service`]: Delivery function called by the notification poller

pub mod events;
pub mod service;
pub mod signing;

pub use events::{WebhookEvent, WebhookEventType};
pub use service::WebhookService;
pub use signing::{generate_secret, sign_payload};
