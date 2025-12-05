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
#
# Test data files:
# - gpt4-batch.jsonl: References model "gpt-4-files-permtest"
# - restricted-batch.jsonl: References model "restricted-model-permtest"
#
# NOTE: Model access for file uploads is controlled via group membership.
# Even PlatformManager users must be in groups to access models for file uploads.
# This is a known limitation - admins should bypass model restrictions.
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
  "name": "test-endpoint-{{newUuid}}-permtest",
  "url": "https://api.test.com/v1",
  "sync": false
}
HTTP 201
[Captures]
test_endpoint_id: jsonpath "$.id"

# Create a test group for user1 (admin will be added later to demonstrate restriction)
POST http://localhost:3001/admin/api/v1/groups
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "test-group-files-{{newUuid}}-permtest",
  "description": "Test group for files permissions"
}
HTTP 201
[Captures]
group_id: jsonpath "$.id"

# Add user1 to the group
POST http://localhost:3001/admin/api/v1/groups/{{group_id}}/users/{{user1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Create deployment accessible via group membership
# This model is referenced in gpt4-batch.jsonl
POST http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "model_name": "test-model-gpt4-permtest",
  "alias": "gpt-4-files-permtest",
  "hosted_on": "{{test_endpoint_id}}"
}
HTTP 200
[Captures]
gpt4_deployment_id: jsonpath "$.id"

# Add deployment to group so user1 can access it
POST http://localhost:3001/admin/api/v1/groups/{{group_id}}/models/{{gpt4_deployment_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Create another deployment that no one has access to (not in any group)
# This model is referenced in restricted-batch.jsonl
POST http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "model_name": "test-model-restricted-permtest",
  "alias": "restricted-model-permtest",
  "hosted_on": "{{test_endpoint_id}}"
}
HTTP 200
[Captures]
restricted_deployment_id: jsonpath "$.id"

# =============================================================================
# POST /files - Upload Files with Model Access Validation
# =============================================================================

# -----------------------------------------------------------------------------
# User1 CAN upload file with model they have access to (gpt4-batch.jsonl)
# -----------------------------------------------------------------------------

POST http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{user_jwt}}
[MultipartFormData]
purpose: batch
file: file,gpt4-batch.jsonl;
HTTP 201
[Captures]
user1_file_id: jsonpath "$.id"
[Asserts]
jsonpath "$.filename" == "gpt4-batch.jsonl"

# -----------------------------------------------------------------------------
# User2 CANNOT upload gpt4-batch.jsonl (no access to gpt-4-files-permtest)
# -----------------------------------------------------------------------------

POST http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{user2_jwt}}
[MultipartFormData]
purpose: batch
file: file,gpt4-batch.jsonl;
HTTP 400
[Asserts]
body contains "Model"
body contains "has not been configured or is not available to user"

# -----------------------------------------------------------------------------
# User1 CANNOT upload restricted-batch.jsonl (no access to restricted-model-permtest)
# -----------------------------------------------------------------------------

POST http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{user_jwt}}
[MultipartFormData]
purpose: batch
file: file,restricted-batch.jsonl;
HTTP 400

# -----------------------------------------------------------------------------
# Platform manager CANNOT upload without group access (demonstrates restriction)
# Admin is not yet in the group, so they cannot upload files for gpt-4-files-permtest
# -----------------------------------------------------------------------------

POST http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{admin_jwt}}
[MultipartFormData]
purpose: batch
file: file,gpt4-batch.jsonl;
HTTP 400
[Asserts]
body contains "Model"
body contains "has not been configured or is not available to user"

# -----------------------------------------------------------------------------
# Add admin to group to grant model access
# -----------------------------------------------------------------------------

POST http://localhost:3001/admin/api/v1/groups/{{group_id}}/users/{{admin_user_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# -----------------------------------------------------------------------------
# Platform manager CAN now upload gpt4-batch.jsonl (has group access)
# -----------------------------------------------------------------------------

POST http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{admin_jwt}}
[MultipartFormData]
purpose: batch
file: file,gpt4-batch.jsonl;
HTTP 201
[Captures]
admin_gpt4_file_id: jsonpath "$.id"
[Asserts]
jsonpath "$.filename" == "gpt4-batch.jsonl"

# -----------------------------------------------------------------------------
# Platform manager still CANNOT upload restricted-batch.jsonl (no group has access)
# Model exists but no group has been granted access to restricted-model-permtest
# -----------------------------------------------------------------------------

POST http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{admin_jwt}}
[MultipartFormData]
purpose: batch
file: file,restricted-batch.jsonl;
HTTP 400
[Asserts]
body contains "Model"
body contains "has not been configured or is not available to user"

# =============================================================================
# POST /files - Mixed Model Access Test
# =============================================================================
# Tests that users need access to ALL models referenced in a batch file
# mixed-batch.jsonl references both gpt-4-files-permtest and restricted-model-permtest

# -----------------------------------------------------------------------------
# User1 CANNOT upload mixed-batch.jsonl (only has access to gpt-4-files-permtest)
# User needs access to ALL models - fails because restricted-model-permtest not accessible
# -----------------------------------------------------------------------------

POST http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{user_jwt}}
[MultipartFormData]
purpose: batch
file: file,mixed-batch.jsonl;
HTTP 400
[Asserts]
body contains "Model"
body contains "has not been configured or is not available to user"

# -----------------------------------------------------------------------------
# Setup: Grant admin access to restricted model via second group
# -----------------------------------------------------------------------------

POST http://localhost:3001/admin/api/v1/groups
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "test-group-2-{{newUuid}}-permtest",
  "description": "Second test group with access to restricted model"
}
HTTP 201
[Captures]
group2_id: jsonpath "$.id"

# Add restricted model to second group
POST http://localhost:3001/admin/api/v1/groups/{{group2_id}}/models/{{restricted_deployment_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Add admin to second group
POST http://localhost:3001/admin/api/v1/groups/{{group2_id}}/users/{{admin_user_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# -----------------------------------------------------------------------------
# Platform manager CAN now upload mixed-batch.jsonl (has access to BOTH models)
# Admin is in both groups, so has access to both gpt-4-files-permtest and restricted-model-permtest
# -----------------------------------------------------------------------------

POST http://localhost:3001/ai/v1/files
[Cookies]
dwctl_session: {{admin_jwt}}
[MultipartFormData]
purpose: batch
file: file,mixed-batch.jsonl;
HTTP 201
[Captures]
admin_mixed_file_id: jsonpath "$.id"
[Asserts]
jsonpath "$.filename" == "mixed-batch.jsonl"

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
# Platform manager sees all files (even those uploaded by other users)
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
jsonpath "$.data[*].id" contains "{{admin_mixed_file_id}}"

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
jsonpath "$.filename" == "gpt4-batch.jsonl"

# -----------------------------------------------------------------------------
# User1 CANNOT read admin's files
# -----------------------------------------------------------------------------

GET http://localhost:3001/ai/v1/files/{{admin_gpt4_file_id}}
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
body contains "gpt-4-files-permtest"

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
body contains "gpt-4-files-permtest"

GET http://localhost:3001/ai/v1/files/{{admin_gpt4_file_id}}/content
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
header "content-type" == "application/x-ndjson"
body contains "gpt-4-files-permtest"

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

DELETE http://localhost:3001/ai/v1/files/{{admin_mixed_file_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{admin_mixed_file_id}}"
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
# Delete models by their aliases (from test data files)
# -----------------------------------------------------------------------------

DELETE http://localhost:3001/admin/api/v1/groups/{{group_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

DELETE http://localhost:3001/admin/api/v1/groups/{{group2_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Delete models referenced in test data files
DELETE http://localhost:3001/admin/api/v1/models/{{gpt4_deployment_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200

DELETE http://localhost:3001/admin/api/v1/models/{{restricted_deployment_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200

DELETE http://localhost:3001/admin/api/v1/endpoints/{{test_endpoint_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204