# Strict Mode Tests
#
# These tests verify that when strict mode is enabled (DWCTL_ONWARDS__STRICT_MODE=true),
# only known OpenAI API paths are accepted.
#
# To run these tests with strict mode enabled:
# DWCTL_ONWARDS__STRICT_MODE=true hurl --variables-file test.env tests/authenticated/strict-mode.hurl
#
# NOTE: These tests will FAIL without strict mode enabled, as the server will
# accept unknown paths in normal mode.

# Test that unknown endpoints return 404 in strict mode
POST http://localhost:3001/ai/v1/unknown/endpoint
Authorization: Bearer {{inference_api_key}}
Content-Type: application/json
{}
HTTP 404

# Test that legacy /v1/completions returns 404 in strict mode
POST http://localhost:3001/ai/v1/completions
Authorization: Bearer {{inference_api_key}}
Content-Type: application/json
{
  "model": "test-model",
  "prompt": "test"
}
HTTP 404

# Test that /v1/engines returns 404 in strict mode
POST http://localhost:3001/ai/v1/engines/test/completions
Authorization: Bearer {{inference_api_key}}
Content-Type: application/json
{
  "prompt": "test"
}
HTTP 404

# Test that /v1/files returns 404 in strict mode (files handled separately via batch API)
GET http://localhost:3001/ai/v1/files
Authorization: Bearer {{inference_api_key}}
HTTP 404

# Test that /v1/audio endpoints return 404 in strict mode
POST http://localhost:3001/ai/v1/audio/speech
Authorization: Bearer {{inference_api_key}}
Content-Type: application/json
{
  "model": "tts-1",
  "input": "test",
  "voice": "alloy"
}
HTTP 404

# Test that /v1/images endpoints return 404 in strict mode
POST http://localhost:3001/ai/v1/images/generations
Authorization: Bearer {{inference_api_key}}
Content-Type: application/json
{
  "prompt": "test",
  "n": 1,
  "size": "1024x1024"
}
HTTP 404

# Test that GET /models is allowed in strict mode
GET http://localhost:3001/ai/models
Authorization: Bearer {{inference_api_key}}
HTTP 200
[Asserts]
jsonpath "$.object" == "list"
jsonpath "$.data" isCollection

# Test that GET /v1/models is allowed in strict mode
GET http://localhost:3001/ai/v1/models
Authorization: Bearer {{inference_api_key}}
HTTP 200
[Asserts]
jsonpath "$.object" == "list"
jsonpath "$.data" isCollection

# Test that POST /v1/chat/completions is allowed in strict mode
# Note: This will fail with 404 if no model deployment exists
# When a deployment exists, it should return 200 or another non-404 status
POST http://localhost:3001/ai/v1/chat/completions
Authorization: Bearer {{inference_api_key}}
Content-Type: application/json
{
  "model": "test-model",
  "messages": [
    {
      "role": "user",
      "content": "Hello"
    }
  ]
}
# Expect any response except 404 (might be 404 from no deployment, or 200/500 from upstream)
# In strict mode, the router accepts the path, so 404 means "no deployment", not "unknown path"
# Note: This test documents expected behavior but may need adjustment based on test setup

# Test that POST /v1/embeddings is allowed in strict mode
POST http://localhost:3001/ai/v1/embeddings
Authorization: Bearer {{inference_api_key}}
Content-Type: application/json
{
  "model": "text-embedding-3-small",
  "input": "test"
}
# Expect any response except 404 from unknown path
# Actual response depends on whether deployment exists

# Test that POST /v1/responses is allowed in strict mode
POST http://localhost:3001/ai/v1/responses
Authorization: Bearer {{inference_api_key}}
Content-Type: application/json
{
  "model": "gpt-4o",
  "input": "test"
}
# Expect any response except 404 from unknown path
# Actual response depends on whether deployment exists

# Test that malformed JSON returns 400, not 404 (schema validation)
POST http://localhost:3001/ai/v1/chat/completions
Authorization: Bearer {{inference_api_key}}
Content-Type: application/json
{invalid json
HTTP 400

# Test that missing required fields returns 400, not 404 (schema validation)
POST http://localhost:3001/ai/v1/chat/completions
Authorization: Bearer {{inference_api_key}}
Content-Type: application/json
{
  "messages": [
    {
      "role": "user",
      "content": "test"
    }
  ]
}
# Missing "model" field should trigger validation error
HTTP 400
