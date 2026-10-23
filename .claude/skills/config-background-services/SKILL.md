---
name: config-background-services
description: Use when adding configuration options, changing configuration loading, starting background workers, modifying leader-only services, or instrumenting off-request-path failures in control-layer.
---

# Configuration and background services

Treat configuration, worker ownership, shutdown, and observability as one feature
contract. Follow the current configuration types and startup wiring; older
deployment examples are not a complete schema.

## Configuration changes

1. Add the typed field and backward-compatible defaults in `dwctl/src/config.rs`.
   Update deserialization, normalization, and `Config::validate` where relevant.
2. Document the option and its units in root `config.yaml`; update the reference
   for user-facing changes. Use YAML for ordinary settings and the existing
   `DWCTL_` overrides (`__` between nested keys), not a separate env-only path.
3. Test missing/old config, an explicit YAML value, the nested override, and invalid
   combinations. Preserve unrelated pool/component settings when normalizing
   database URL overrides. Keep secrets out of public config responses and logs.
4. Trace consumers in `dwctl/src/lib.rs`: adding a field alone does not wire it
   into a service. Check whether changes require restart or are actually watched.

## Worker lifecycle

Distinguish always-running work from leader-only work and disabled services.
Use existing cancellation tokens, task ownership, leadership transition handling,
and bounded drain behavior. A leadership loss must stop the old leader's work;
reacquisition must not leave duplicate workers. Make retries bounded/backed off
and define what persistent state allows recovery after cancellation or crash.

Session advisory locks and listeners use direct connections. Query pools use the
existing routing abstractions; see [sqlx-queries](../sqlx-queries/SKILL.md).
Do not infer fleet-wide limits from a worker's process-local settings.

## Failures must be observable

For off-request-path failures use `background_error!`, which emits both a log
and `dwctl_background_errors_total`. Choose `Critical`, `Error`, or `Warning`
deliberately. Use bounded static component/reason labels; request IDs, error text,
and customer identifiers do not belong in metric labels.

Ordinary returned HTTP 5xx failures already have request metrics. Swallowed
request-path failures may need explicit instrumentation. Avoid counting the same
failure twice at adjacent boundaries.

For example, a poller's database failure should produce the component/reason
counter, retry according to policy, and still react promptly to cancellation.
Test all three, plus disabled mode and leadership loss/reacquisition where used.
Run the relevant config/lifecycle tests and `just lint rust` / `just test rust`.

## Sources

- [Configuration reference](../../../docs/src/reference/configuration.md),
  [current types/loading](../../../dwctl/src/config.rs), and [defaults](../../../config.yaml).
- [Service wiring](../../../dwctl/src/lib.rs) and [leader election](../../../dwctl/src/leader_election.rs).
- [Background error contract and examples](../../../dwctl/src/metrics/errors.rs).
