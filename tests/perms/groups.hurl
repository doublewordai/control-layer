# =============================================================================
# Groups Permissions Test Suite
# =============================================================================
# Tests all CRUD operations on /admin/api/v1/groups endpoints
#
# Permission model:
# - StandardUser: CANNOT interact with groups in any way
#   - Cannot create, read, update, or delete groups
#   - Cannot manage group memberships (users or models)
#   - Cannot list or view group information, even for groups they're in
# - PlatformManager: Full access to all group operations
#   - Can create, read, update, and delete any group
#   - Can manage all group memberships regardless of their own membership
#   - Can view all groups and their relationships
# =============================================================================

# -----------------------------------------------------------------------------
# Setup: Get user IDs and create test resources
# -----------------------------------------------------------------------------

# Get user IDs
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

# Create test endpoint for models
POST http://localhost:3001/admin/api/v1/endpoints
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "groups-test-endpoint-{{newUuid}}",
  "url": "http://mock-llm-server:3000",
  "sync": false
}
HTTP 201
[Captures]
groups_test_endpoint_id: jsonpath "$.id"

# Create test models for group assignments
POST http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "type": "standard",
  "model_name": "groups-test-model-1",
  "alias": "groups-test-model-1-alias",
  "hosted_on": "{{groups_test_endpoint_id}}"
}
HTTP 200
[Captures]
test_model_1_id: jsonpath "$.id"

POST http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "type": "standard",
  "model_name": "groups-test-model-2",
  "alias": "groups-test-model-2-alias",
  "hosted_on": "{{groups_test_endpoint_id}}"
}
HTTP 200
[Captures]
test_model_2_id: jsonpath "$.id"

# =============================================================================
# Group Creation Permissions (POST /groups)
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT create groups
# -----------------------------------------------------------------------------

# User1 tries to create a group - should be forbidden
POST http://localhost:3001/admin/api/v1/groups
[Cookies]
dwctl_session: {{user_jwt}}
{
  "name": "user1-unauthorized-group",
  "description": "Should not be created"
}
HTTP 403

# User2 tries to create a group - should be forbidden
POST http://localhost:3001/admin/api/v1/groups
[Cookies]
dwctl_session: {{user2_jwt}}
{
  "name": "user2-unauthorized-group",
  "description": "Should not be created"
}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN create groups
# -----------------------------------------------------------------------------

# Admin creates group 1 (will add user1 to this)
POST http://localhost:3001/admin/api/v1/groups
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "test-group-1-{{newUuid}}",
  "description": "Test group 1"
}
HTTP 201
[Asserts]
jsonpath "$.id" exists
jsonpath "$.name" startsWith "test-group-1-"
jsonpath "$.created_by" == "{{admin_user_id}}"
[Captures]
group1_id: jsonpath "$.id"

# Admin creates group 2 (will add user2 to this)
POST http://localhost:3001/admin/api/v1/groups
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "test-group-2-{{newUuid}}",
  "description": "Test group 2"
}
HTTP 201
[Asserts]
jsonpath "$.id" exists
[Captures]
group2_id: jsonpath "$.id"

# Admin creates group 3 (empty group, admin not in it)
POST http://localhost:3001/admin/api/v1/groups
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "test-group-3-{{newUuid}}",
  "description": "Empty test group"
}
HTTP 201
[Asserts]
jsonpath "$.id" exists
[Captures]
group3_id: jsonpath "$.id"

# =============================================================================
# Group Listing Permissions (GET /groups)
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT list groups (even ones they're in)
# -----------------------------------------------------------------------------

# Add user1 to group1 first (so they're a member)
POST http://localhost:3001/admin/api/v1/groups/{{group1_id}}/users/{{user1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# User1 tries to list groups - should be forbidden (even though they're in group1)
GET http://localhost:3001/admin/api/v1/groups
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User2 tries to list groups - should be forbidden
GET http://localhost:3001/admin/api/v1/groups
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN list all groups
# -----------------------------------------------------------------------------

# Admin lists all groups - should see all 3 groups
GET http://localhost:3001/admin/api/v1/groups
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count >= 3
jsonpath "$.data[*].id" contains "{{group1_id}}"
jsonpath "$.data[*].id" contains "{{group2_id}}"
jsonpath "$.data[*].id" contains "{{group3_id}}"

# =============================================================================
# Group Retrieval Permissions (GET /groups/{group_id})
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT fetch groups (even ones they're in)
# -----------------------------------------------------------------------------

# User1 tries to fetch group1 (which they're a member of) - should be forbidden
GET http://localhost:3001/admin/api/v1/groups/{{group1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User1 tries to fetch group2 (which they're NOT a member of) - should be forbidden
GET http://localhost:3001/admin/api/v1/groups/{{group2_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User2 tries to fetch any group - should be forbidden
GET http://localhost:3001/admin/api/v1/groups/{{group3_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN fetch any group
# -----------------------------------------------------------------------------

# Admin fetches group1 - should succeed
GET http://localhost:3001/admin/api/v1/groups/{{group1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{group1_id}}"
jsonpath "$.name" startsWith "test-group-1-"

# Admin fetches group2 - should succeed
GET http://localhost:3001/admin/api/v1/groups/{{group2_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{group2_id}}"

# Admin fetches group3 (empty group) - should succeed
GET http://localhost:3001/admin/api/v1/groups/{{group3_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{group3_id}}"

# =============================================================================
# Group Update Permissions (PATCH /groups/{group_id})
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT update groups (even ones they're in)
# -----------------------------------------------------------------------------

# User1 tries to update group1 (which they're in) - should be forbidden
PATCH http://localhost:3001/admin/api/v1/groups/{{group1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
{
  "name": "user1-unauthorized-update"
}
HTTP 403

# User1 tries to update group2 (which they're NOT in) - should be forbidden
PATCH http://localhost:3001/admin/api/v1/groups/{{group2_id}}
[Cookies]
dwctl_session: {{user_jwt}}
{
  "name": "user1-unauthorized-update"
}
HTTP 403

# User2 tries to update any group - should be forbidden
PATCH http://localhost:3001/admin/api/v1/groups/{{group3_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
{
  "description": "Unauthorized update"
}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN update any group
# -----------------------------------------------------------------------------

# Admin updates group1 - should succeed
PATCH http://localhost:3001/admin/api/v1/groups/{{group1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "description": "Updated by admin"
}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{group1_id}}"
jsonpath "$.description" == "Updated by admin"

# Admin updates group2 - should succeed
PATCH http://localhost:3001/admin/api/v1/groups/{{group2_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "test-group-2-renamed"
}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{group2_id}}"
jsonpath "$.name" == "test-group-2-renamed"

# =============================================================================
# Group Membership Permissions (POST/DELETE /groups/{group_id}/users/{user_id})
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT add users to groups
# -----------------------------------------------------------------------------

# User1 tries to add user2 to group1 (user1 is in this group) - should be forbidden
POST http://localhost:3001/admin/api/v1/groups/{{group1_id}}/users/{{user2_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User1 tries to add themselves to group2 - should be forbidden
POST http://localhost:3001/admin/api/v1/groups/{{group2_id}}/users/{{user1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User2 tries to add user1 to any group - should be forbidden
POST http://localhost:3001/admin/api/v1/groups/{{group3_id}}/users/{{user1_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# StandardUser CANNOT remove users from groups
# -----------------------------------------------------------------------------

# User1 tries to remove themselves from group1 - should be forbidden
DELETE http://localhost:3001/admin/api/v1/groups/{{group1_id}}/users/{{user1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User2 tries to remove user1 from group1 - should be forbidden
DELETE http://localhost:3001/admin/api/v1/groups/{{group1_id}}/users/{{user1_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN add users to groups
# -----------------------------------------------------------------------------

# Admin adds user2 to group2 - should succeed
POST http://localhost:3001/admin/api/v1/groups/{{group2_id}}/users/{{user2_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Admin adds user1 to group3 - should succeed
POST http://localhost:3001/admin/api/v1/groups/{{group3_id}}/users/{{user1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# -----------------------------------------------------------------------------
# PlatformManager CAN remove users from groups
# -----------------------------------------------------------------------------

# Admin removes user1 from group3 - should succeed
DELETE http://localhost:3001/admin/api/v1/groups/{{group3_id}}/users/{{user1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# =============================================================================
# User-Group Membership Permissions (POST/DELETE /users/{user_id}/groups/{group_id})
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT add groups to users
# -----------------------------------------------------------------------------

# User1 tries to add group2 to themselves - should be forbidden
POST http://localhost:3001/admin/api/v1/users/{{user1_id}}/groups/{{group2_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User1 tries to add group3 to user2 - should be forbidden
POST http://localhost:3001/admin/api/v1/users/{{user2_id}}/groups/{{group3_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User2 tries to add any group to themselves - should be forbidden
POST http://localhost:3001/admin/api/v1/users/{{user2_id}}/groups/{{group3_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# StandardUser CANNOT remove groups from users
# -----------------------------------------------------------------------------

# User1 tries to remove group1 from themselves - should be forbidden
DELETE http://localhost:3001/admin/api/v1/users/{{user1_id}}/groups/{{group1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User2 tries to remove group2 from themselves - should be forbidden
DELETE http://localhost:3001/admin/api/v1/users/{{user2_id}}/groups/{{group2_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN add groups to users
# -----------------------------------------------------------------------------

# Admin adds group3 to user1 via user endpoint - should succeed
POST http://localhost:3001/admin/api/v1/users/{{user1_id}}/groups/{{group3_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# -----------------------------------------------------------------------------
# PlatformManager CAN remove groups from users
# -----------------------------------------------------------------------------

# Admin removes group3 from user1 via user endpoint - should succeed
DELETE http://localhost:3001/admin/api/v1/users/{{user1_id}}/groups/{{group3_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# =============================================================================
# Group Users Listing Permissions (GET /groups/{group_id}/users)
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT list group users
# -----------------------------------------------------------------------------

# User1 tries to list users in group1 (which they're in) - should be forbidden
GET http://localhost:3001/admin/api/v1/groups/{{group1_id}}/users
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User2 tries to list users in group2 (which they're in) - should be forbidden
GET http://localhost:3001/admin/api/v1/groups/{{group2_id}}/users
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# User1 tries to list users in group3 (which they're NOT in) - should be forbidden
GET http://localhost:3001/admin/api/v1/groups/{{group3_id}}/users
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN list users in any group
# -----------------------------------------------------------------------------

# Admin lists users in group1 - should succeed
GET http://localhost:3001/admin/api/v1/groups/{{group1_id}}/users
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection
jsonpath "$" contains "{{user1_id}}"

# Admin lists users in group2 - should succeed
GET http://localhost:3001/admin/api/v1/groups/{{group2_id}}/users
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection
jsonpath "$" contains "{{user2_id}}"

# Admin lists users in group3 (empty) - should succeed
GET http://localhost:3001/admin/api/v1/groups/{{group3_id}}/users
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection

# =============================================================================
# User Groups Listing Permissions (GET /users/{user_id}/groups)
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT list other users' groups
# -----------------------------------------------------------------------------

# User1 tries to list user2's groups - should be forbidden
GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/groups
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User2 tries to list user1's groups - should be forbidden
GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/groups
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# User2 tries to list admin's groups - should be forbidden
GET http://localhost:3001/admin/api/v1/users/{{admin_user_id}}/groups
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN list any user's groups
# -----------------------------------------------------------------------------

# Admin lists user1's groups - should succeed
GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/groups
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection
jsonpath "$[*].id" contains "{{group1_id}}"

# Admin lists user2's groups - should succeed
GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/groups
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection
jsonpath "$[*].id" contains "{{group2_id}}"

# =============================================================================
# Group created_by Field Visibility
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT see created_by when viewing their own groups
# -----------------------------------------------------------------------------

# User1 fetches their own groups - should NOT see created_by
GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/groups
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection
jsonpath "$[*].id" contains "{{group1_id}}"
jsonpath "$[0].created_by" not exists

# User2 fetches their own groups - should NOT see created_by
GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/groups
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection
jsonpath "$[*].id" contains "{{group2_id}}"
jsonpath "$[0].created_by" not exists

# -----------------------------------------------------------------------------
# PlatformManager CAN see created_by for all groups
# -----------------------------------------------------------------------------

# Admin fetches user1's groups - should see created_by
GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/groups
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection
jsonpath "$[0].created_by" exists
jsonpath "$[0].created_by" == "{{admin_user_id}}"

# Admin fetches user2's groups - should see created_by
GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/groups
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection
jsonpath "$[0].created_by" exists
jsonpath "$[0].created_by" == "{{admin_user_id}}"

# =============================================================================
# Group-Model Association Permissions (POST/DELETE /groups/{group_id}/models/{deployment_id})
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT add models to groups
# -----------------------------------------------------------------------------

# User1 tries to add model to group1 (which they're in) - should be forbidden
POST http://localhost:3001/admin/api/v1/groups/{{group1_id}}/models/{{test_model_1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User1 tries to add model to group3 (which they're NOT in) - should be forbidden
POST http://localhost:3001/admin/api/v1/groups/{{group3_id}}/models/{{test_model_1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User2 tries to add model to any group - should be forbidden
POST http://localhost:3001/admin/api/v1/groups/{{group2_id}}/models/{{test_model_2_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# StandardUser CANNOT remove models from groups
# -----------------------------------------------------------------------------

# Add model first (as admin) so we can test removal
POST http://localhost:3001/admin/api/v1/groups/{{group1_id}}/models/{{test_model_1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# User1 tries to remove model from group1 - should be forbidden
DELETE http://localhost:3001/admin/api/v1/groups/{{group1_id}}/models/{{test_model_1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User2 tries to remove model from any group - should be forbidden
DELETE http://localhost:3001/admin/api/v1/groups/{{group1_id}}/models/{{test_model_1_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN add models to groups
# -----------------------------------------------------------------------------

# Admin adds test_model_2 to group2 - should succeed
POST http://localhost:3001/admin/api/v1/groups/{{group2_id}}/models/{{test_model_2_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Admin adds test_model_1 to group3 (empty group) - should succeed
POST http://localhost:3001/admin/api/v1/groups/{{group3_id}}/models/{{test_model_1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# -----------------------------------------------------------------------------
# PlatformManager CAN remove models from groups
# -----------------------------------------------------------------------------

# Admin removes test_model_1 from group3 - should succeed
DELETE http://localhost:3001/admin/api/v1/groups/{{group3_id}}/models/{{test_model_1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# =============================================================================
# Group Models Listing Permissions (GET /groups/{group_id}/models)
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT list group models
# -----------------------------------------------------------------------------

# User1 tries to list models in group1 (which they're in) - should be forbidden
GET http://localhost:3001/admin/api/v1/groups/{{group1_id}}/models
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User2 tries to list models in group2 (which they're in) - should be forbidden
GET http://localhost:3001/admin/api/v1/groups/{{group2_id}}/models
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# User1 tries to list models in group3 (which they're NOT in) - should be forbidden
GET http://localhost:3001/admin/api/v1/groups/{{group3_id}}/models
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN list models in any group
# -----------------------------------------------------------------------------

# Admin lists models in group1 - should succeed
GET http://localhost:3001/admin/api/v1/groups/{{group1_id}}/models
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection
jsonpath "$" contains "{{test_model_1_id}}"

# Admin lists models in group2 - should succeed
GET http://localhost:3001/admin/api/v1/groups/{{group2_id}}/models
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection
jsonpath "$" contains "{{test_model_2_id}}"

# Admin lists models in group3 (empty) - should succeed
GET http://localhost:3001/admin/api/v1/groups/{{group3_id}}/models
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection

# =============================================================================
# Model Groups Listing Permissions (GET /models/{deployment_id}/groups)
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT list model groups
# -----------------------------------------------------------------------------

# User1 tries to list groups for test_model_1 (in group1 which they're in) - should be forbidden
GET http://localhost:3001/admin/api/v1/models/{{test_model_1_id}}/groups
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User2 tries to list groups for test_model_2 - should be forbidden
GET http://localhost:3001/admin/api/v1/models/{{test_model_2_id}}/groups
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN list groups for any model
# -----------------------------------------------------------------------------

# Admin lists groups for test_model_1 - should succeed
GET http://localhost:3001/admin/api/v1/models/{{test_model_1_id}}/groups
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection
jsonpath "$" contains "{{group1_id}}"

# Admin lists groups for test_model_2 - should succeed
GET http://localhost:3001/admin/api/v1/models/{{test_model_2_id}}/groups
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection
jsonpath "$" contains "{{group2_id}}"

# =============================================================================
# Group Deletion Permissions (DELETE /groups/{group_id})
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT delete groups
# -----------------------------------------------------------------------------

# User1 tries to delete group1 (which they're in) - should be forbidden
DELETE http://localhost:3001/admin/api/v1/groups/{{group1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User2 tries to delete group2 (which they're in) - should be forbidden
DELETE http://localhost:3001/admin/api/v1/groups/{{group2_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# User1 tries to delete group3 (which they're NOT in) - should be forbidden
DELETE http://localhost:3001/admin/api/v1/groups/{{group3_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN delete any group
# -----------------------------------------------------------------------------

# Admin deletes group3 (empty group) - should succeed
DELETE http://localhost:3001/admin/api/v1/groups/{{group3_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Verify group3 is deleted
GET http://localhost:3001/admin/api/v1/groups/{{group3_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 404

# =============================================================================
# Cleanup
# =============================================================================

DELETE http://localhost:3001/admin/api/v1/groups/{{group1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

DELETE http://localhost:3001/admin/api/v1/groups/{{group2_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

DELETE http://localhost:3001/admin/api/v1/models/{{test_model_1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200

DELETE http://localhost:3001/admin/api/v1/models/{{test_model_2_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200

DELETE http://localhost:3001/admin/api/v1/endpoints/{{groups_test_endpoint_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204