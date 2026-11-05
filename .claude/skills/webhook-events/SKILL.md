---
name: webhook-events
description: Use when adding outbound webhook event types, changing event detection, delivery retries, signing, scope checks, or notification processing in control-layer.
---

# Outbound webhook events

Separate event detection from durable delivery. This skill covers outgoing
platform/user events; incoming payment callbacks follow
[billing-accounting](../billing-accounting/SKILL.md).

## Add an event through the existing pipeline

1. Extend `WebhookEventType`, scope assignment, parsing/display/validation, and
   the deliberate payload builder in `dwctl/src/webhooks/events.rs`. Include
   only intended fields; never serialize a whole database row containing secrets.
2. Choose detection in `notifications.rs`: managed-table events use PostgreSQL
   notification handling; external-schema events may use polling. Respect the
   webhook feature gate. Management CRUD availability is separate from delivery.
3. Create durable delivery rows through the webhook repository. Preserve the
   resource/event deduplication rule and event identity across retries.
4. Reuse the dispatcher and signing implementation. Keep atomic claiming,
   lease-based recovery, bounded attempts/backoff, circuit breaking, and drain
   behavior; do not send HTTP inline in resource mutation handlers.

## Authorization and delivery semantics

Keep scope validation in the API, database enforcement, runtime `accepts_event`
matching, and current platform-role checks when selecting eligible webhooks.
`event_types = null` means events within the webhook's scope, not all scopes.
Demotion must stop new platform delivery creation.

Durable rows support retry after creation. They do not make event capture durable:
platform NOTIFY events currently enter an in-memory buffer, and new-batch polling
has a bounded lookback/page. Disconnected listeners or downtime can lose events
before any delivery row exists. Do not promise exactly-once or lossless capture;
a feature requiring replay needs an explicitly persisted detection/reconciliation
design, not merely another dispatcher retry.

For example, replaying a resource event should not create duplicate delivery rows;
retrying a failed delivery should retain its event ID and valid signature. A lost
NOTIFY is a different failure and cannot be repaired by retrying nonexistent rows.

## Verification

Test scope validation, role demotion, duplicate events, concurrent claimers,
abandoned-lease recovery, retry identity/signatures, disabled processing, and
shutdown with work in flight. Exercise detection gaps separately from delivery
recovery. Use dispatcher/signing/repository tests and required Rust lint/tests.

## Sources

- [Webhook guide](../../../docs/webhooks.md) and
  [event capture guide](../../../docs/event-capture.md). The latter's claim that
  triggers prevent missed events across restarts does not describe current code.
- [Events/signing/dispatcher](../../../dwctl/src/webhooks/),
  [notification poller](../../../dwctl/src/notifications.rs), and
  [delivery repository](../../../dwctl/src/db/handlers/webhooks.rs).
- [Scope matching](../../../dwctl/src/db/models/webhooks.rs) and
  [management API](../../../dwctl/src/api/handlers/webhooks.rs).
