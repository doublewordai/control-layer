---
name: billing-accounting
description: Use when changing credits, payment callbacks, token or cache pricing, balance reads, usage aggregates, or provisioned tariffs in control-layer.
---

# Billing and accounting changes

Keep money exact, ledger writes idempotent, and derived views consistent with
their source. Usage analytics and charged amounts answer different questions.

## Preserve the accounting path

- Use `Decimal` and existing conversion helpers, never floating-point arithmetic
  for monetary values. Current credit precision is `DECIMAL(24,15)`; do not copy
  the older billing guide's eight-decimal claim.
- Low-volume purchases/grants/adjustments go through `Credits::create_transaction`.
  Ledger insertion, balance folding, and batch aggregation are atomic. High-volume
  usage uses the request-logging batcher. Do not insert ledger rows through a new
  path that omits the corresponding folds.
- Keep enclosing transactions short and database-local: holding the balance row
  lock across an external payment call can stall usage flushes.
- `get_user_balance` point-reads `user_balance_checkpoints`. Do not restore the
  historical ledger-tail scan or use replica reads for immediate payment visibility.
- Preserve append-only accounting and database `source_id` uniqueness. An earlier
  duplicate check is an optimization, not protection against concurrent callbacks.

## Payments and prices

Verify payment state through the existing provider integration and derive the
credited owner from validated payment/session data. Stripe checkout credits the
pre-tax subtotal, not the tax-inclusive total. Retain the stable payment source ID
across callback retries and manual completion processing.

Keep tariff history and cache-aware charging intact. Batch billed totals come
from ledger-derived aggregates, not a recomputation from a simplified token formula.
Daily usage refresh concerns successful analytics and UTC windows, not a replacement
for the ledger. Provisioned prices use exact per-million conversion; unchanged
tariffs retain their identity/history, and changed tariffs close the old version.

For example, two simultaneous callbacks for a taxed purchase must produce one
purchase of the pre-tax amount and one balance increase. A callback retry must
not double-credit the user. Separately, reapplying an unchanged provisioning
catalog must not create another tariff version.

## Regression coverage

Test duplicate/concurrent callbacks, tiny decimal amounts, rollback, balance
visibility, owner attribution, cache pricing, and zero-balance transitions as
applicable. Test repeat usage refresh/provisioning for idempotence. Usage charging
and routing-cache updates are asynchronous; do not promise a zero-overspend bound.
Run relevant Rust tests and required lint/tests; use [sqlx-queries](../sqlx-queries/SKILL.md).

## Sources

- [Credit repository](../../../dwctl/src/db/handlers/credits.rs),
  [usage batcher](../../../dwctl/src/request_logging/batcher.rs),
  [Stripe integration/tests](../../../dwctl/src/payment_providers/stripe.rs).
- [Analytics](../../../dwctl/src/db/handlers/analytics.rs) and
  [model provisioning](../../../docs/src/reference/model-provisioning.md).
- [Billing overview](../../../docs/src/conceptual-guides/how-billing-works.md),
  [tariffs](../../../docs/src/how-to/tariffs.md), and
  [payments](../../../docs/src/how-to/payments.md) explain user flows; current
  code supersedes the overview's balance algorithm, precision, and immediate-blocking claims.
