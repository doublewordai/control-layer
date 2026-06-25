# =============================================================================
# Anthropic /v1/messages ingress - black-box wiring check
# =============================================================================
# Proves the /ai/v1/messages edge translator is wired and that failures come
# back in Anthropic error shape, without depending on a routable backend model.
# (A successful blocking + streaming round-trip is covered end to end by the
# Rust e2e in dwctl/src/test/anthropic.rs, which force-syncs the route; the
# docker stack does not populate onwards' route cache from DB-created models.)
# =============================================================================

# A malformed Anthropic request (missing the required `max_tokens`) is rejected
# by the edge translator itself, before any routing or auth, as an Anthropic
# error envelope. This proves the /messages translation layer is in the path.
POST http://localhost:3001/ai/v1/messages
x-api-key: sk-not-a-real-key
anthropic-version: 2023-06-01
{
  "model": "anthropic-e2e",
  "messages": [ { "role": "user", "content": "hello" } ]
}
HTTP 400
[Asserts]
jsonpath "$.type" == "error"
jsonpath "$.error.type" == "invalid_request_error"

# A well-formed request that cannot be routed (no valid key / unknown model)
# still comes back reframed into the Anthropic error shape, not the raw
# OpenAI/onwards shape - proving the response path is translated too.
POST http://localhost:3001/ai/v1/messages
x-api-key: sk-not-a-real-key
anthropic-version: 2023-06-01
{
  "model": "definitely-not-a-real-model-xyz",
  "max_tokens": 16,
  "messages": [ { "role": "user", "content": "hello" } ]
}
HTTP *
[Asserts]
status >= 400
status < 500
jsonpath "$.type" == "error"
