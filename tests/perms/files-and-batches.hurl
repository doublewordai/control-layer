# =============================================================================
# Files and Batches Permissions Test Suite
# =============================================================================
# Tests CRUD operations on /files and /batches endpoints
# Covers: StandardUser + BatchAPIUser permissions (own resources only) and 
# PlatformManager permissions (all resources)
#
# Test users:
# - admin_jwt: Platform Manager
# - user_jwt: Standard User 1 (user@example.org) with BatchAPIUser role
# - user2_jwt: Standard User 2 (user2@example.org) with BatchAPIUser role
#
# Test data files (all reference models with -permtest suffix):
# - gpt4.jsonl: References "gpt-4-permtest"
# - gemini.jsonl: References "gemini-pro-permtest"
# - restricted.jsonl: References "restricted-permtest"
# - mixed.jsonl: References both "gpt-4-permtest" and "restricted-permtest"
#
# Test structure:
# 1. Setup: Create endpoint, groups, and 3 models
# 2. Files tests: Upload, list, read, delete permissions
# 3. Batches tests: Create, read, cancel, list permissions
# 4. Cleanup: Delete all resources
# =============================================================================

# -----------------------------------------------------------------------------
# Setup: Get user IDs, create endpoint, models, and groups
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

# Create test endpoint pointing to mock LLM server
POST http://localhost:3001/admin/api/v1/endpoints
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "test-endpoint-{{newUuid}}-permtest",
  "url": "http://mock-llm-server:8000/v1",
  "sync": false
}
HTTP 201
[Captures]
test_endpoint_id: jsonpath "$.id"

# Create main test group (user1 and user2 will have access to gpt-4 and gemini models)
POST http://localhost:3001/admin/api/v1/groups
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "test-group-main-{{newUuid}}-permtest",
  "description": "Main test group with access to gpt-4 and gemini models"
}
HTTP 201
[Captures]
main_group_id: jsonpath "$.id"

# Add user1 and user2 to main group
POST http://localhost:3001/admin/api/v1/groups/{{main_group_id}}/users/{{user1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

POST http://localhost:3001/admin/api/v1/groups/{{main_group_id}}/users/{{user2_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Create gpt-4 model deployment
POST http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "model_name": "gpt-4-mock",
  "alias": "gpt-4-permtest",
  "hosted_on": "{{test_endpoint_id}}"
}
HTTP 200
[Captures]
gpt4_model_id: jsonpath "$.id"

# Add gpt-4 model to main group
POST http://localhost:3001/admin/api/v1/groups/{{main_group_id}}/models/{{gpt4_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Create gemini-pro model deployment
POST http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "model_name": "gemini-pro",
  "alias": "gemini-pro-permtest",
  "hosted_on": "{{test_endpoint_id}}"
}
HTTP 200
[Captures]
gemini_model_id: jsonpath "$.id"

# Add gemini-pro model to main group
POST http://localhost:3001/admin/api/v1/groups/{{main_group_id}}/models/{{gemini_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Create restricted model (no one has access initially)
POST http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "model_name": "gemini-1.5-flash",
  "alias": "restricted-permtest",
  "hosted_on": "{{test_endpoint_id}}"
}
HTTP 200
[Captures]
restricted_model_id: jsonpath "$.id"

# Create restricted group for permission revocation test (only user1 initially)
POST http://localhost:3001/admin/api/v1/groups
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "test-group-restricted-{{newUuid}}-permtest",
  "description": "Restricted group for permission revocation test"
}
HTTP 201
[Captures]
restricted_group_id: jsonpath "$.id"

# Add user1 to restricted group
POST http://localhost:3001/admin/api/v1/groups/{{restricted_group_id}}/users/{{user1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Add restricted model to restricted group
POST http://localhost:3001/admin/api/v1/groups/{{restricted_group_id}}/models/{{restricted_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# =============================================================================
# FILES PERMISSIONS TESTS
# =============================================================================

# -----------------------------------------------------------------------------
# POST /files - Upload Files with Model Access Validation
# -----------------------------------------------------------------------------

# User1 CAN upload file with gpt-4 model (has access via main group)
POST http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{user_jwt}}
[MultipartFormData]
purpose: batch
file: file,gpt4.jsonl;
HTTP 201
[Captures]
user1_gpt4_file_id: jsonpath "$.id"
[Asserts]
jsonpath "$.filename" == "gpt4.jsonl"

# User2 CAN upload file with gemini model (has access via main group)
POST http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{user2_jwt}}
[MultipartFormData]
purpose: batch
file: file,gemini.jsonl;
HTTP 201
[Captures]
user2_gemini_file_id: jsonpath "$.id"
[Asserts]
jsonpath "$.filename" == "gemini.jsonl"

# User1 CAN upload file with restricted model (has access via restricted group)
POST http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{user_jwt}}
[MultipartFormData]
purpose: batch
file: file,restricted.jsonl;
HTTP 201
[Captures]
user1_restricted_file_id: jsonpath "$.id"
[Asserts]
jsonpath "$.filename" == "restricted.jsonl"

# User2 CANNOT upload file with restricted model (no access)
POST http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{user2_jwt}}
[MultipartFormData]
purpose: batch
file: file,restricted.jsonl;
HTTP 400
[Asserts]
body contains "Model"
body contains "has not been configured or is not available to user"

# Admin CANNOT upload without group access (demonstrates restriction)
POST http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{admin_jwt}}
[MultipartFormData]
purpose: batch
file: file,gpt4.jsonl;
HTTP 400
[Asserts]
body contains "Model"
body contains "has not been configured or is not available to user"

# Add admin to main group to grant access
POST http://localhost:3001/admin/api/v1/groups/{{main_group_id}}/users/{{admin_user_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Verify admin is in the group (this also helps ensure the cache is updated)
GET http://localhost:3001/admin/api/v1/groups/{{main_group_id}}/users
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$[*]" contains "{{admin_user_id}}"

# Admin CAN now upload gpt4.jsonl (has group access)
POST http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{admin_jwt}}
[MultipartFormData]
purpose: batch
file: file,gpt4.jsonl;
HTTP 201
[Captures]
admin_gpt4_file_id: jsonpath "$.id"
[Asserts]
jsonpath "$.filename" == "gpt4.jsonl"

# Add admin to restricted group for mixed file test
POST http://localhost:3001/admin/api/v1/groups/{{restricted_group_id}}/users/{{admin_user_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# User1 CANNOT upload mixed.jsonl (only has access to gpt-4 and restricted, but needs both)
# This actually SHOULD work since user1 is in both groups - this tests that ALL models are checked
POST http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{user_jwt}}
[MultipartFormData]
purpose: batch
file: file,mixed.jsonl;
HTTP 201
[Captures]
user1_mixed_file_id: jsonpath "$.id"
[Asserts]
jsonpath "$.filename" == "mixed.jsonl"

# Admin CAN upload mixed.jsonl (has access to both models)
POST http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{admin_jwt}}
[MultipartFormData]
purpose: batch
file: file,mixed.jsonl;
HTTP 201
[Captures]
admin_mixed_file_id: jsonpath "$.id"
[Asserts]
jsonpath "$.filename" == "mixed.jsonl"

# -----------------------------------------------------------------------------
# GET /files - List Files
# -----------------------------------------------------------------------------

# User1 sees only their own files (3 files: gpt4, restricted, mixed)
GET http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.object" == "list"
jsonpath "$.data" count == 3
jsonpath "$.data[*].id" contains "{{user1_gpt4_file_id}}"
jsonpath "$.data[*].id" contains "{{user1_restricted_file_id}}"
jsonpath "$.data[*].id" contains "{{user1_mixed_file_id}}"

# User2 sees only their own files (1 file: gemini)
GET http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.object" == "list"
jsonpath "$.data" count == 1
jsonpath "$.data[0].id" == "{{user2_gemini_file_id}}"

# Admin sees all files (6 total: 3 from user1, 1 from user2, 2 from admin)
GET http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.object" == "list"
jsonpath "$.data" count == 6
jsonpath "$.data[*].id" contains "{{user1_gpt4_file_id}}"
jsonpath "$.data[*].id" contains "{{user2_gemini_file_id}}"
jsonpath "$.data[*].id" contains "{{admin_gpt4_file_id}}"

# -----------------------------------------------------------------------------
# GET /files/{file_id} - Get Specific File Metadata
# -----------------------------------------------------------------------------

# User1 CAN read their own file metadata
GET http://localhost:3001/ai/v1/files/{{user1_gpt4_file_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_gpt4_file_id}}"
jsonpath "$.filename" == "gpt4.jsonl"

# User1 CANNOT read user2's files
GET http://localhost:3001/ai/v1/files/{{user2_gemini_file_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 404

# User2 CANNOT read user1's files
GET http://localhost:3001/ai/v1/files/{{user1_gpt4_file_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 404

# Admin CAN read all file metadata
GET http://localhost:3001/ai/v1/files/{{user1_gpt4_file_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_gpt4_file_id}}"

GET http://localhost:3001/ai/v1/files/{{user2_gemini_file_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user2_gemini_file_id}}"

# -----------------------------------------------------------------------------
# GET /files/{file_id}/content - Get File Content
# -----------------------------------------------------------------------------

# User1 CAN read their own file content
GET http://localhost:3001/ai/v1/files/{{user1_gpt4_file_id}}/content
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
header "content-type" == "application/x-ndjson"
body contains "gpt-4-permtest"

# User1 CANNOT read user2's file content
GET http://localhost:3001/ai/v1/files/{{user2_gemini_file_id}}/content
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 404

# User2 CANNOT read user1's file content
GET http://localhost:3001/ai/v1/files/{{user1_gpt4_file_id}}/content
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 404

# Admin CAN read all file content
GET http://localhost:3001/ai/v1/files/{{user1_gpt4_file_id}}/content
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
header "content-type" == "application/x-ndjson"
body contains "gpt-4-permtest"

GET http://localhost:3001/ai/v1/files/{{user2_gemini_file_id}}/content
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
header "content-type" == "application/x-ndjson"
body contains "gemini-pro-permtest"

# =============================================================================
# BATCHES PERMISSIONS TESTS
# =============================================================================
# These tests reuse the files uploaded in the previous section

# -----------------------------------------------------------------------------
# POST /batches - Create Batch Permissions
# -----------------------------------------------------------------------------

# User1 CAN create batch from their own file
POST http://localhost:3001/ai/v1/batches
[Cookies]
dwctl_session: {{user_jwt}}
{
  "input_file_id": "{{user1_gpt4_file_id}}",
  "endpoint": "/v1/chat/completions",
  "completion_window": "24h"
}
HTTP 201
[Captures]
user1_batch_id: jsonpath "$.id"
[Asserts]
jsonpath "$.input_file_id" == "{{user1_gpt4_file_id}}"
jsonpath "$.status" == "in_progress"
jsonpath "$.endpoint" == "/v1/chat/completions"

# User1 CANNOT create batch from user2's file
POST http://localhost:3001/ai/v1/batches
[Cookies]
dwctl_session: {{user_jwt}}
{
  "input_file_id": "{{user2_gemini_file_id}}",
  "endpoint": "/v1/chat/completions",
  "completion_window": "24h"
}
HTTP 403
[Asserts]
body contains "Insufficient permissions"

# User2 CAN create batch from their own file
POST http://localhost:3001/ai/v1/batches
[Cookies]
dwctl_session: {{user2_jwt}}
{
  "input_file_id": "{{user2_gemini_file_id}}",
  "endpoint": "/v1/chat/completions",
  "completion_window": "24h"
}
HTTP 201
[Captures]
user2_batch_id: jsonpath "$.id"
[Asserts]
jsonpath "$.input_file_id" == "{{user2_gemini_file_id}}"

# User2 CANNOT create batch from user1's file
POST http://localhost:3001/ai/v1/batches
[Cookies]
dwctl_session: {{user2_jwt}}
{
  "input_file_id": "{{user1_gpt4_file_id}}",
  "endpoint": "/v1/chat/completions",
  "completion_window": "24h"
}
HTTP 403

# Admin CAN create batch from their own file
POST http://localhost:3001/ai/v1/batches
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "input_file_id": "{{admin_gpt4_file_id}}",
  "endpoint": "/v1/chat/completions",
  "completion_window": "24h"
}
HTTP 201
[Captures]
admin_batch_id: jsonpath "$.id"
[Asserts]
jsonpath "$.input_file_id" == "{{admin_gpt4_file_id}}"

# Admin CAN create batch from user1's file (has ReadAll permission)
POST http://localhost:3001/ai/v1/batches
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "input_file_id": "{{user1_gpt4_file_id}}",
  "endpoint": "/v1/chat/completions",
  "completion_window": "24h"
}
HTTP 201
[Captures]
admin_batch_from_user1_file: jsonpath "$.id"
[Asserts]
jsonpath "$.input_file_id" == "{{user1_gpt4_file_id}}"

# Permission revocation test: Remove user1 from restricted group, then create batch
DELETE http://localhost:3001/admin/api/v1/groups/{{restricted_group_id}}/users/{{user1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# User1 can still create batch from previously uploaded restricted file
# (permissions checked at file upload time, not batch creation time)
POST http://localhost:3001/ai/v1/batches
[Cookies]
dwctl_session: {{user_jwt}}
{
  "input_file_id": "{{user1_restricted_file_id}}",
  "endpoint": "/v1/chat/completions",
  "completion_window": "24h"
}
HTTP 201
[Captures]
user1_restricted_batch_id: jsonpath "$.id"
[Asserts]
jsonpath "$.input_file_id" == "{{user1_restricted_file_id}}"

# -----------------------------------------------------------------------------
# GET /batches/{batch_id} - Get Batch Permissions
# -----------------------------------------------------------------------------

# User1 CAN get their own batch
GET http://localhost:3001/ai/v1/batches/{{user1_batch_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_batch_id}}"
jsonpath "$.input_file_id" == "{{user1_gpt4_file_id}}"

# User1 CANNOT get user2's batch
GET http://localhost:3001/ai/v1/batches/{{user2_batch_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 404

# User2 CAN get their own batch
GET http://localhost:3001/ai/v1/batches/{{user2_batch_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user2_batch_id}}"

# User2 CANNOT get user1's batch
GET http://localhost:3001/ai/v1/batches/{{user1_batch_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 404

# Admin CAN get all batches
GET http://localhost:3001/ai/v1/batches/{{user1_batch_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_batch_id}}"

GET http://localhost:3001/ai/v1/batches/{{user2_batch_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user2_batch_id}}"

# =============================================================================
# GET /files/{file_id} - Batch Output/Error File Permissions
# =============================================================================
# When batches complete, they create output_file_id and error_file_id
# Users should only be able to access these files for their own batches
# Platform managers can access all batch output/error files

# -----------------------------------------------------------------------------
# Capture output/error file IDs from user1's batch
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/batches/{{user1_batch_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Captures]
user1_batch_output_file_id: jsonpath "$.output_file_id"
user1_batch_error_file_id: jsonpath "$.error_file_id"

# -----------------------------------------------------------------------------
# Capture output/error file IDs from user2's batch
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/batches/{{user2_batch_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Captures]
user2_batch_output_file_id: jsonpath "$.output_file_id"
user2_batch_error_file_id: jsonpath "$.error_file_id"

# -----------------------------------------------------------------------------
# User1 CAN read their own batch output file metadata
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user1_batch_output_file_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_batch_output_file_id}}"

# -----------------------------------------------------------------------------
# User1 CAN read their own batch output file content
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user1_batch_output_file_id}}/content
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200

# -----------------------------------------------------------------------------
# User1 CAN read their own batch error file metadata
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user1_batch_error_file_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_batch_error_file_id}}"

# -----------------------------------------------------------------------------
# User1 CAN read their own batch error file content
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user1_batch_error_file_id}}/content
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200

# -----------------------------------------------------------------------------
# User1 CANNOT read user2's batch output file metadata
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user2_batch_output_file_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 404

# -----------------------------------------------------------------------------
# User1 CANNOT read user2's batch output file content
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user2_batch_output_file_id}}/content
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 404

# -----------------------------------------------------------------------------
# User1 CANNOT read user2's batch error file metadata
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user2_batch_error_file_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 404

# -----------------------------------------------------------------------------
# User1 CANNOT read user2's batch error file content
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user2_batch_error_file_id}}/content
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 404

# -----------------------------------------------------------------------------
# User2 CAN read their own batch output file metadata
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user2_batch_output_file_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user2_batch_output_file_id}}"

# -----------------------------------------------------------------------------
# User2 CAN read their own batch output file content
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user2_batch_output_file_id}}/content
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200

# -----------------------------------------------------------------------------
# User2 CAN read their own batch error file metadata
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user2_batch_error_file_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user2_batch_error_file_id}}"

# -----------------------------------------------------------------------------
# User2 CAN read their own batch error file content
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user2_batch_error_file_id}}/content
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200

# -----------------------------------------------------------------------------
# User2 CANNOT read user1's batch output file metadata
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user1_batch_output_file_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 404

# -----------------------------------------------------------------------------
# User2 CANNOT read user1's batch output file content
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user1_batch_output_file_id}}/content
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 404

# -----------------------------------------------------------------------------
# User2 CANNOT read user1's batch error file metadata
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user1_batch_error_file_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 404

# -----------------------------------------------------------------------------
# User2 CANNOT read user1's batch error file content
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user1_batch_error_file_id}}/content
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 404

# -----------------------------------------------------------------------------
# Admin CAN read user1's batch output file metadata
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user1_batch_output_file_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_batch_output_file_id}}"

# -----------------------------------------------------------------------------
# Admin CAN read user1's batch output file content
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user1_batch_output_file_id}}/content
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200

# -----------------------------------------------------------------------------
# Admin CAN read user1's batch error file metadata
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user1_batch_error_file_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_batch_error_file_id}}"

# -----------------------------------------------------------------------------
# Admin CAN read user1's batch error file content
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user1_batch_error_file_id}}/content
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200

# -----------------------------------------------------------------------------
# Admin CAN read user2's batch output file metadata
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user2_batch_output_file_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user2_batch_output_file_id}}"

# -----------------------------------------------------------------------------
# Admin CAN read user2's batch output file content
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user2_batch_output_file_id}}/content
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200

# -----------------------------------------------------------------------------
# Admin CAN read user2's batch error file metadata
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user2_batch_error_file_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user2_batch_error_file_id}}"

# -----------------------------------------------------------------------------
# Admin CAN read user2's batch error file content
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{user2_batch_error_file_id}}/content
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200

# -----------------------------------------------------------------------------
# GET /batches/{batch_id}/analytics - Get Batch Analytics Permissions
# -----------------------------------------------------------------------------

# User1 CAN get analytics for their own batch
GET http://localhost:3001/ai/v1/batches/{{user1_batch_id}}/analytics
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.total_requests" exists

# User1 CANNOT get analytics for user2's batch
GET http://localhost:3001/ai/v1/batches/{{user2_batch_id}}/analytics
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 404

# User2 CAN get analytics for their own batch
GET http://localhost:3001/ai/v1/batches/{{user2_batch_id}}/analytics
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.total_requests" exists

# User2 CANNOT get analytics for user1's batch
GET http://localhost:3001/ai/v1/batches/{{user1_batch_id}}/analytics
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 404

# Admin CAN get analytics for all batches
GET http://localhost:3001/ai/v1/batches/{{user1_batch_id}}/analytics
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.total_requests" exists

GET http://localhost:3001/ai/v1/batches/{{user2_batch_id}}/analytics
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.total_requests" exists

# -----------------------------------------------------------------------------
# POST /batches/{batch_id}/cancel - Cancel Batch Permissions
# -----------------------------------------------------------------------------

# User1 CAN cancel their own batch
POST http://localhost:3001/ai/v1/batches/{{user1_batch_id}}/cancel
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_batch_id}}"
jsonpath "$.status" matches "^(cancelling|cancelled)$"

# User1 CANNOT cancel user2's batch
POST http://localhost:3001/ai/v1/batches/{{user2_batch_id}}/cancel
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 404

# User2 CAN cancel their own batch
POST http://localhost:3001/ai/v1/batches/{{user2_batch_id}}/cancel
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user2_batch_id}}"
jsonpath "$.status" matches "^(cancelling|cancelled)$"

# Admin CAN cancel all batches
POST http://localhost:3001/ai/v1/batches/{{admin_batch_id}}/cancel
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{admin_batch_id}}"

POST http://localhost:3001/ai/v1/batches/{{admin_batch_from_user1_file}}/cancel
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{admin_batch_from_user1_file}}"

# -----------------------------------------------------------------------------
# GET /batches - List Batches Permissions
# -----------------------------------------------------------------------------

# Create fresh batches for list test (previous ones are cancelled)
POST http://localhost:3001/ai/v1/batches
[Cookies]
dwctl_session: {{user_jwt}}
{
  "input_file_id": "{{user1_gpt4_file_id}}",
  "endpoint": "/v1/chat/completions",
  "completion_window": "24h"
}
HTTP 201
[Captures]
user1_list_batch: jsonpath "$.id"

POST http://localhost:3001/ai/v1/batches
[Cookies]
dwctl_session: {{user2_jwt}}
{
  "input_file_id": "{{user2_gemini_file_id}}",
  "endpoint": "/v1/chat/completions",
  "completion_window": "24h"
}
HTTP 201
[Captures]
user2_list_batch: jsonpath "$.id"

POST http://localhost:3001/ai/v1/batches
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "input_file_id": "{{admin_gpt4_file_id}}",
  "endpoint": "/v1/chat/completions",
  "completion_window": "24h"
}
HTTP 201
[Captures]
admin_list_batch: jsonpath "$.id"

# User1 sees only their own batches
GET http://localhost:3001/ai/v1/batches
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.object" == "list"
jsonpath "$.data[*].id" contains "{{user1_list_batch}}"
jsonpath "$.data[*].id" not contains "{{user2_list_batch}}"
jsonpath "$.data[*].id" not contains "{{admin_list_batch}}"

# User2 sees only their own batches
GET http://localhost:3001/ai/v1/batches
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.object" == "list"
jsonpath "$.data[*].id" contains "{{user2_list_batch}}"
jsonpath "$.data[*].id" not contains "{{user1_list_batch}}"
jsonpath "$.data[*].id" not contains "{{admin_list_batch}}"

# Admin sees ALL batches
GET http://localhost:3001/ai/v1/batches
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.object" == "list"
jsonpath "$.data[*].id" contains "{{user1_list_batch}}"
jsonpath "$.data[*].id" contains "{{user2_list_batch}}"
jsonpath "$.data[*].id" contains "{{admin_list_batch}}"

# -----------------------------------------------------------------------------
# DELETE /files - Delete Files Permissions
# -----------------------------------------------------------------------------

# User2 CANNOT delete user1's files
DELETE http://localhost:3001/ai/v1/files/{{user1_gpt4_file_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 404

# User1 CANNOT delete admin's files
DELETE http://localhost:3001/ai/v1/files/{{admin_gpt4_file_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 404

# User1 CAN delete their own files
DELETE http://localhost:3001/ai/v1/files/{{user1_gpt4_file_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_gpt4_file_id}}"
jsonpath "$.deleted" == true

# Verify it's deleted
GET http://localhost:3001/ai/v1/files/{{user1_gpt4_file_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 404

# Admin CAN delete all files
DELETE http://localhost:3001/ai/v1/files/{{user2_gemini_file_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.deleted" == true

DELETE http://localhost:3001/ai/v1/files/{{admin_gpt4_file_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.deleted" == true

# =============================================================================
# Cleanup: Delete test resources
# =============================================================================

DELETE http://localhost:3001/admin/api/v1/groups/{{main_group_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

DELETE http://localhost:3001/admin/api/v1/groups/{{restricted_group_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

DELETE http://localhost:3001/admin/api/v1/models/{{gpt4_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200

DELETE http://localhost:3001/admin/api/v1/models/{{gemini_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200

DELETE http://localhost:3001/admin/api/v1/models/{{restricted_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200

DELETE http://localhost:3001/admin/api/v1/endpoints/{{test_endpoint_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204