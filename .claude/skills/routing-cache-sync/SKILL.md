---
name: routing-cache-sync
description: Use when changing models, groups, API keys, credits, routing eligibility, or the PostgreSQL notifications that update control-layer's onwards cache.
---

# Routing cache synchronization

Management API writes and inference routing are separate paths. A committed row
is not sufficient: the relevant cache must receive and apply the change.

## Trace the complete change

1. Identify the database mutation and its notification producer. Check the current
   migrations/triggers, including scoped API-key notifications and credit balance
   threshold notifications; do not add an unconditional notification to every write.
2. Follow `auth_config_changed` into `OnwardsConfigSync`, then the full target
   loader, capacity updates, and watch-channel publication. Include related keys,
   group membership, deleted/disabled models, composite components, and tariff or
   balance-dependent eligibility when changing those dependencies.
3. Preserve the dedicated direct `listener_db` for `PgListener`. Target loading
   uses the primary query pool. A transaction-pooled query connection cannot own
   the LISTEN session; see [sqlx-queries](../sqlx-queries/SKILL.md).
4. Preserve the coalesced trailing reload: a burst inside the debounce interval
   must still cause a later reload. The periodic fallback and reconnection path
   provide recovery; they do not justify dropping the final notification.

Notifications are not a durable event log. PostgreSQL emits transactional NOTIFY
at commit, and disconnected listeners can miss it. Every serving instance needs
fresh routing state; do not make cache refresh leader-only. Local notification
rate limiting is not a fleet-wide rate limiter or a zero-lag balance guarantee.

## Declarative model changes

Use the existing catalog loader and transactional provisioning path. Validate the
complete catalog before applying it, preserve stable alias identities, and keep
endpoint synchronization from overwriting YAML-owned rows. Omitted models become
manually managed rather than being deleted; an empty/backend-only catalog is a
no-op that preserves ownership markers. Test these separately from cache reloads.
See [model provisioning](../../../docs/src/reference/model-provisioning.md) and
the current [loader](../../../dwctl/src/model_provisioning.rs).

## Regression examples

For key revocation, assert access initially works, commit the revocation, then
poll until routing denies it. Disable/postpone fallback in the notification test
or control its next scheduled tick so it cannot occur within the observation
window; a deadline shorter than the fallback period alone is insufficient.
Also test rollback (no committed routing change), a burst's final update,
reconnection, and fallback independently.

For model/group changes, inspect the generated target shape as well as HTTP
behavior: public/private access, duplicate membership paths, disabled/deleted
components, effective trust, and capacity updates are distinct assertions.
Avoid arbitrary test sleeps; poll the expected transition with a bounded deadline.

Run the relevant sync tests and both shared/scoped pooled E2Es for connection
routing changes, followed by the required Rust lint/tests.

## Sources

- [Sync explainer](../../../docs/onwards-sync-notification-system.md).
- [Current service and target loader](../../../dwctl/src/sync/onwards_config/mod.rs)
  and [fixtures/regressions](../../../dwctl/src/sync/onwards_config/tests.rs).
- [Scoped key trigger](../../../dwctl/migrations/101_scope_api_keys_notify.sql).
- [Pooled E2Es](../../../scripts/tests/pooled/README.md).

The explainer's sample loop omits newer coalescing behavior. Follow the current
service; do not copy the sample as a replacement implementation.
