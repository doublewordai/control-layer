# Durable Data Erasure

Deleting a user starts a durable, creator-scoped erasure workflow. The API does
not treat deletion as a best-effort side effect:

1. A persistent replay guard serializes with creator-attributed content writes.
   Writes that committed first are included in the subsequent drain, and later
   writes are rejected.
2. The account is scrubbed; access keys, connection credentials, webhook
   secrets, reset tokens and access grants are removed; request-level analytics
   identifiers are cleared; an erasure receipt is inserted; and the background
   job is enqueued in one database transaction.
3. The worker removes creator-attributed requests, responses, files, batches,
   and request-log captures in bounded, retryable chunks.
4. Parent records that must remain for referential integrity are reduced to
   non-content tombstones. The worker verifies absence from each applicable
   content store before completing the receipt.
5. Completion clears the raw subject from the receipt. A one-way fingerprint,
   timestamps, and aggregate metrics remain as evidence that the workflow ran.

Queue payloads contain only the receipt identifier. Errors and progress logs do
not include the raw subject identifier.

## Operational signals

The worker emits `data_erasure_completed_total` on completion and
`data_erasure_retry_total{target}` when a store needs another attempt. Operators
should alert on incomplete receipts approaching their deployment's erasure
deadline and periodically reconcile completed receipts against the relevant
stores.

Replay guards are intentionally permanent because creator identifiers must be
opaque and non-reused. Retention and backup handling outside the online stores
remain deployment policy and should be configured separately.

Deployments with historical request captures must explicitly keep
`data_erasure.erase_request_captures` enabled even if new request logging is
disabled. When capture erasure is enabled, startup opens the capture store and
fails closed unless every existing range/default partition has a verified
online subject index; the configurable index timeouts bound that preparation.
