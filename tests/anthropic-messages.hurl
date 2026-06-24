# =============================================================================
# Anthropic /v1/messages ingress - realtime end-to-end
# =============================================================================
# The first realtime-inference hurl test: drives /ai/v1/messages against a
# running dwctl proxying to the mock-llm-server (zerob13/mock-openai-api).
# Validates the Anthropic request is translated, routed, and the response is
# reframed back into Anthropic shape - blocking and streaming.
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

# --- admin: endpoint -> model -> public group -------------------------------
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
  "model_name": "gpt-4",
  "alias": "anthropic-e2e",
  "hosted_on": "{{endpoint_id}}"
}
HTTP *
[Asserts]
status >= 200
status < 300
[Captures]
deployment_id: jsonpath "$.id"

# Associate the deployment with the public group (all-zeros UUID).
POST http://localhost:3001/admin/api/v1/groups/00000000-0000-0000-0000-000000000000/models/{{deployment_id}}
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

# --- blocking: Anthropic request via x-api-key ------------------------------
# Retries to absorb the onwards LISTEN/NOTIFY sync delay after model creation.
POST http://localhost:3001/ai/v1/messages
x-api-key: {{api_key}}
anthropic-version: 2023-06-01
[Options]
retry: 20
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
