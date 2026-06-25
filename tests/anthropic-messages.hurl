# =============================================================================
# Anthropic /v1/messages ingress - realtime end-to-end
# =============================================================================
# Drives /ai/v1/messages against dwctl proxying to the mock-llm-server. Uses the
# upstream model name the mock actually serves (gpt-4-mock, same as the batch
# tests in tests/files/*.jsonl), a dedicated access group, and an explicit user
# membership (mirrors tests/perms/models.hurl). Validates the Anthropic request
# is translated, routed, and the response reframed to Anthropic shape - blocking
# and streaming.
#
# Assumes the standard test harness env (admin_jwt, user_jwt, user1 created).
# =============================================================================

# --- identities -------------------------------------------------------------
GET http://localhost:3001/admin/api/v1/users/current
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Captures]
admin_user_id: jsonpath "$.id"

GET http://localhost:3001/admin/api/v1/users/current
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Captures]
user1_id: jsonpath "$.id"

# --- admin: endpoint -> model (mock-served name) -> group -> membership ------
POST http://localhost:3001/admin/api/v1/endpoints
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "anthropic-e2e-mock",
  "url": "http://mock-llm-server:3000",
  "auto_sync_models": false
}
HTTP *
[Asserts]
status >= 200
status < 300
[Captures]
endpoint_id: jsonpath "$.id"

POST http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "type": "standard",
  "model_name": "gpt-4-mock",
  "alias": "anthropic-e2e",
  "hosted_on": "{{endpoint_id}}"
}
HTTP *
[Asserts]
status >= 200
status < 300
[Captures]
deployment_id: jsonpath "$.id"

POST http://localhost:3001/admin/api/v1/groups
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "anthropic-e2e-group-{{newUuid}}",
  "description": "anthropic e2e access"
}
HTTP 201
[Captures]
group_id: jsonpath "$.id"

POST http://localhost:3001/admin/api/v1/groups/{{group_id}}/users/{{user1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

POST http://localhost:3001/admin/api/v1/groups/{{group_id}}/models/{{deployment_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP *
[Asserts]
status >= 200
status < 300

# Grant the user credits so paid-model checks pass.
POST http://localhost:3001/admin/api/v1/transactions
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "user_id": "{{user1_id}}",
  "transaction_type": "admin_grant",
  "amount": 1000,
  "source_id": "{{admin_user_id}}"
}
HTTP *
[Asserts]
status >= 200
status < 300

# A realtime API key for the user.
POST http://localhost:3001/admin/api/v1/users/{{user1_id}}/api-keys
[Cookies]
dwctl_session: {{user_jwt}}
{
  "purpose": "realtime",
  "name": "anthropic e2e key"
}
HTTP *
[Asserts]
status >= 200
status < 300
[Captures]
api_key: jsonpath "$.key"

# --- sanity: the SHARED routing path via a plain OpenAI request --------------
# Same model + key, no Anthropic translation. This isolates the model-routing
# (sync) from the Anthropic-specific path: if this 404s, the model never reached
# onwards' route cache; if it succeeds, routing works and any /messages failure
# is in the translation layer. Retries absorb the onwards sync delay.
POST http://localhost:3001/ai/v1/chat/completions
Authorization: Bearer {{api_key}}
[Options]
retry: 40
retry-interval: 500
{
  "model": "anthropic-e2e",
  "max_tokens": 64,
  "messages": [ { "role": "user", "content": "hello" } ]
}
HTTP 200
[Asserts]
jsonpath "$.choices[0].message" exists

# --- blocking: Anthropic request via x-api-key ------------------------------
POST http://localhost:3001/ai/v1/messages
x-api-key: {{api_key}}
anthropic-version: 2023-06-01
[Options]
retry: 10
retry-interval: 500
{
  "model": "anthropic-e2e",
  "max_tokens": 64,
  "system": "be terse",
  "messages": [ { "role": "user", "content": "hello" } ]
}
HTTP 200
[Asserts]
jsonpath "$.type" == "message"
jsonpath "$.role" == "assistant"
jsonpath "$.content[0].type" == "text"
jsonpath "$.stop_reason" exists
jsonpath "$.usage.input_tokens" exists
jsonpath "$.usage.output_tokens" exists

# --- streaming: typed Anthropic event sequence ------------------------------
POST http://localhost:3001/ai/v1/messages
x-api-key: {{api_key}}
anthropic-version: 2023-06-01
[Options]
retry: 10
retry-interval: 500
{
  "model": "anthropic-e2e",
  "max_tokens": 64,
  "stream": true,
  "messages": [ { "role": "user", "content": "hello" } ]
}
HTTP 200
[Asserts]
header "Content-Type" contains "text/event-stream"
body contains "event: message_start"
body contains "event: message_stop"
