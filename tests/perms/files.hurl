# =============================================================================
# Files Permissions Test Suite
# =============================================================================
# Tests CRUD operations on /files endpoints
# Covers: StandardUser + BatchAPIUser permissions (own resources only) and 
# PlatformManager permissions (all resources)
#
# Test users:
# - admin_jwt: Platform Manager
# - user_jwt: Standard User 1 (user@example.org) with BatchAPIUser role
# - user2_jwt: Standard User 2 (user2@example.org) with BatchAPIUser role
# =============================================================================

# -----------------------------------------------------------------------------
# Setup: Get user IDs, create endpoint and models
# -----------------------------------------------------------------------------

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

GET http://localhost:3001/admin/api/v1/users/current
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Captures]
user2_id: jsonpath "$.id"

# Create test endpoint (without syncing)
POST http://localhost:3001/admin/api/v1/endpoints
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "test-endpoint-files-{{newUuid}}",
  "url": "https://api.test.com/v1",
  "sync": false
}
HTTP 201
[Captures]
test_endpoint_id: jsonpath "$.id"

# Create a test group for user1 only (not user2)
POST http://localhost:3001/admin/api/v1/groups
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "test-group-files-{{newUuid}}",
  "description": "Test group for files permissions"
}
HTTP 201
[Captures]
group_id: jsonpath "$.id"

# Add only user1 to the group
POST http://localhost:3001/admin/api/v1/groups/{{group_id}}/users/{{user1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Create deployment accessible to user1 only (via group)
POST http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "model_name": "test-model-gpt4-{{newUuid}}",
  "alias": "gpt-4-files-{{newUuid}}",
  "hosted_on": "{{test_endpoint_id}}"
}
HTTP 200
[Captures]
gpt4_deployment_id: jsonpath "$.id"
gpt4_alias: jsonpath "$.alias"

# Add deployment to group so user1 can access it
POST http://localhost:3001/admin/api/v1/groups/{{group_id}}/models/{{gpt4_deployment_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Create another deployment that no one has access to (not in any group)
POST http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "model_name": "test-model-restricted-{{newUuid}}",
  "alias": "restricted-model-files-{{newUuid}}",
  "hosted_on": "{{test_endpoint_id}}"
}
HTTP 200
[Captures]
restricted_deployment_id: jsonpath "$.id"
restricted_alias: jsonpath "$.alias"

# =============================================================================
# POST /files - Upload Files with Model Access Validation
# =============================================================================

# -----------------------------------------------------------------------------
# User1 CAN upload file with model they have access to
# -----------------------------------------------------------------------------

POST http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{user_jwt}}
[MultipartFormData]
purpose: batch
file: bytes,user1-valid.jsonl; {"custom_id":"req-1","method":"POST","url":"/v1/chat/completions","body":{"model":"{{gpt4_alias}}","messages":[{"role":"user","content":"Hello"}]}}
HTTP 201
[Captures]
user1_file_id: jsonpath "$.id"
[Asserts]
jsonpath "$.filename" == "user1-valid.jsonl"
jsonpath "$.purpose" == "batch"
jsonpath "$.bytes" > 0

# -----------------------------------------------------------------------------
# User2 CANNOT upload file with model they don't have access to
# -----------------------------------------------------------------------------

POST http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{user2_jwt}}
[MultipartFormData]
purpose: batch
file: bytes,user2-invalid.jsonl; {"custom_id":"req-1","method":"POST","url":"/v1/chat/completions","body":{"model":"{{gpt4_alias}}","messages":[{"role":"user","content":"Should fail"}]}}
HTTP 400
[Asserts]
body contains "Model"
body contains "has not been configured or is not available to user"

# -----------------------------------------------------------------------------
# User1 CANNOT upload file with model not in any group
# -----------------------------------------------------------------------------

POST http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{user_jwt}}
[MultipartFormData]
purpose: batch
file: bytes,user1-restricted.jsonl; {"custom_id":"req-1","method":"POST","url":"/v1/chat/completions","body":{"model":"{{restricted_alias}}","messages":[{"role":"user","content":"Should fail"}]}}
HTTP 400
[Asserts]
body contains "Model"
body contains "has not been configured or is not available to user"

# -----------------------------------------------------------------------------
# Platform manager CAN upload file with any model (even restricted ones)
# -----------------------------------------------------------------------------

POST http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{admin_jwt}}
[MultipartFormData]
purpose: batch
file: bytes,admin-gpt4.jsonl; {"custom_id":"req-1","method":"POST","url":"/v1/chat/completions","body":{"model":"{{gpt4_alias}}","messages":[{"role":"user","content":"Admin with gpt-4"}]}}
HTTP 201
[Captures]
admin_gpt4_file_id: jsonpath "$.id"
[Asserts]
jsonpath "$.filename" == "admin-gpt4.jsonl"

POST http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{admin_jwt}}
[MultipartFormData]
purpose: batch
file: bytes,admin-restricted.jsonl; {"custom_id":"req-1","method":"POST","url":"/v1/chat/completions","body":{"model":"{{restricted_alias}}","messages":[{"role":"user","content":"Admin with restricted"}]}}
HTTP 201
[Captures]
admin_restricted_file_id: jsonpath "$.id"
[Asserts]
jsonpath "$.filename" == "admin-restricted.jsonl"

# =============================================================================
# GET /files - List Files
# =============================================================================

# -----------------------------------------------------------------------------
# User1 sees only their own files
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.object" == "list"
jsonpath "$.data" count == 1
jsonpath "$.data[0].id" == "{{user1_file_id}}"

# -----------------------------------------------------------------------------
# User2 sees no files (they have none)
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.object" == "list"
jsonpath "$.data" count == 0

# -----------------------------------------------------------------------------
# Platform manager sees all files
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.object" == "list"
jsonpath "$.data" count == 3
jsonpath "$.data[*].id" contains "{{user1_file_id}}"
jsonpath "$.data[*].id" contains "{{admin_gpt4_file_id}}"
jsonpath "$.data[*].id" contains "{{admin_restricted_file_id}}"

# =============================================================================
# GET /files/{file_id} - Get Specific File Metadata
# =============================================================================

# -----------------------------------------------------------------------------
# User1 can read their own file metadata
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user1_file_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_file_id}}"
jsonpath "$.filename" == "user1-valid.jsonl"

# -----------------------------------------------------------------------------
# User1 CANNOT read admin's files
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{admin_gpt4_file_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 404

GET http://localhost:3001/ai/v1/files/{{admin_restricted_file_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 404

# -----------------------------------------------------------------------------
# User2 CANNOT read user1's files
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user1_file_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 404

# -----------------------------------------------------------------------------
# Platform manager can read all file metadata
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user1_file_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_file_id}}"

GET http://localhost:3001/ai/v1/files/{{admin_gpt4_file_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{admin_gpt4_file_id}}"

GET http://localhost:3001/ai/v1/files/{{admin_restricted_file_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{admin_restricted_file_id}}"

# =============================================================================
# GET /files/{file_id}/content - Get File Content
# =============================================================================

# -----------------------------------------------------------------------------
# User1 can read their own file content
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user1_file_id}}/content
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
header "content-type" == "application/x-ndjson"
body contains "Hello"

# -----------------------------------------------------------------------------
# User1 CANNOT read admin's file content
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{admin_gpt4_file_id}}/content
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 404

# -----------------------------------------------------------------------------
# User2 CANNOT read user1's file content
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user1_file_id}}/content
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 404

# -----------------------------------------------------------------------------
# Platform manager can read all file content
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user1_file_id}}/content
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
header "content-type" == "application/x-ndjson"
body contains "Hello"

GET http://localhost:3001/ai/v1/files/{{admin_gpt4_file_id}}/content
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
header "content-type" == "application/x-ndjson"
body contains "Admin with gpt-4"

GET http://localhost:3001/ai/v1/files/{{admin_restricted_file_id}}/content
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
header "content-type" == "application/x-ndjson"
body contains "Admin with restricted"

# =============================================================================
# DELETE /files/{file_id} - Delete Files
# =============================================================================

# -----------------------------------------------------------------------------
# Test blocked scenarios first (no files deleted)
# -----------------------------------------------------------------------------

# User2 CANNOT delete user1's files
DELETE http://localhost:3001/ai/v1/files/{{user1_file_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 404

# User1 CANNOT delete admin's files
DELETE http://localhost:3001/ai/v1/files/{{admin_gpt4_file_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 404

# -----------------------------------------------------------------------------
# User1 CAN delete their own file
# -----------------------------------------------------------------------------

DELETE http://localhost:3001/ai/v1/files/{{user1_file_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_file_id}}"
jsonpath "$.deleted" == true

# Verify it's deleted
GET http://localhost:3001/ai/v1/files/{{user1_file_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 404

# -----------------------------------------------------------------------------
# Platform manager CAN delete all files
# -----------------------------------------------------------------------------

DELETE http://localhost:3001/ai/v1/files/{{admin_gpt4_file_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{admin_gpt4_file_id}}"
jsonpath "$.deleted" == true

DELETE http://localhost:3001/ai/v1/files/{{admin_restricted_file_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{admin_restricted_file_id}}"
jsonpath "$.deleted" == true

# -----------------------------------------------------------------------------
# Final verification: All files should be deleted
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 0

# -----------------------------------------------------------------------------
# Cleanup: Delete test resources
# -----------------------------------------------------------------------------

DELETE http://localhost:3001/admin/api/v1/groups/{{group_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

DELETE http://localhost:3001/admin/api/v1/models/{{gpt4_deployment_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

DELETE http://localhost:3001/admin/api/v1/models/{{restricted_deployment_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

DELETE http://localhost:3001/admin/api/v1/endpoints/{{test_endpoint_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204