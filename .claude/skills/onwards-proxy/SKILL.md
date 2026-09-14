---
name: onwards-proxy
description: Use when changing onwards routing, middleware, fallback, provider trust, strict schemas, response sanitization, SSE streaming, or proxy shutdown in control-layer.
---

# Proxy and streaming changes

Trace both request and response paths. A handler returning headers is not the end
of a streaming request; behavior also depends on the provider actually selected.

## Middleware and protocol boundaries

Read the middleware-order commentary and construction in `dwctl/src/lib.rs`.
Tower's last-added layer runs first on requests and last on responses. Preserve
these reasons for the current order:

- Inference middleware sees raw Responses/background fields before translation.
- Outlet captures the original request and the translated, cache-enriched response
  used for persistence and billing.
- Cache keys use original image URLs before per-request signing mutates them.
- Body edits preserve model alias behavior and correct headers such as Content-Length.

Use current `dwctl/src/inference/translation/` and dispatch integration. Historical
Responses processor plans describe removed modules and are not implementation recipes.

## Trust, sanitization, and fallback

Use effective `ResolvedTrust` from the selected provider, including fallback/model
override. Do not reuse the first provider's trust or forward its privileged trace
context to a different provider.

Strict mode validates through its typed routes and sanitizes successful responses;
trusted errors bypass masking. Untrusted strict errors mask operator-account
statuses and genericize prose. Non-strict opt-in sanitization has different error
semantics. Strict mode skips the optional response-transform hook to avoid double
sanitization. Inspect the actual route/mode rather than assuming one error policy.

An HTTP 200 SSE stream can contain an error. Preserve standalone `{"error": ...}`
events through cleaning: strict mode applies its trust policy, while non-strict
opt-in sanitization forwards embedded errors verbatim. Do not discard them as
unknown success fields or equate HTTP 200 with completed inference.

## Stream lifetime and verification

Provider connection and in-flight metric guards remain attached to the response
body through completion/drop. Pool/key limiter guards are currently handler-local;
do not infer identical lifetimes for every limiter. Test cancellation and fallback
for leaked or prematurely released guards.

Exercise unary and fragmented/multiline SSE, embedded errors under each trust/mode,
fallback trust changes, malformed payloads, aliases/headers, and client disconnects.
Test real signal-driven shutdown for the standalone binary; embedded dwctl owns
its own server lifecycle and needs separate coverage.

Run `cargo test -p onwards` (including `--test graceful_shutdown` when relevant),
cross-layer dwctl tests, and required Rust lint/tests for changed Rust code.

## Sources

- [Proxy handlers](../../../onwards/src/handlers.rs), [strict routes/schemas](../../../onwards/src/strict/),
  and [optional sanitizer](../../../onwards/src/response_sanitizer.rs).
- [Middleware assembly](../../../dwctl/src/lib.rs) and [inference integration](../../../dwctl/src/inference/).
- [Strict-mode guide](../../../onwards/docs/src/strict-mode.md),
  [sanitization](../../../onwards/docs/src/sanitization.md), and
  [shutdown](../../../onwards/docs/src/shutdown.md). Check current routers for endpoint
  coverage; older prose lists omit supported routes and Responses streaming.
