//! Webhook notification system for batch events.
//!
//! This module provides Standard Webhooks-compliant notifications for batch terminal states
//! (completed, failed, cancelled). It includes:
//!
//! - [`signing`]: HMAC-SHA256 signature generation per Standard Webhooks spec
//! - [`events`]: Event types and payload builders for batch events
//! - [`service`]: Background delivery service with retry logic and circuit breaker

pub mod events;
pub mod service;
pub mod signing;

pub use events::{BatchEventData, RequestCounts, WebhookEvent, WebhookEventType};
pub use service::{BatchWebhookEvent, WebhookDeliveryService};
pub use signing::{generate_secret, sign_payload};
