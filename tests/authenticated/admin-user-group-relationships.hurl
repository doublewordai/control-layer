# Test comprehensive user-group relationship management
# This test covers adding/removing users from groups and verifying the relationships

# Create test users
POST http://localhost:3001/admin/api/v1/users
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "username": "test-user-1-{{newUuid}}",
  "email": "test-user-1-{{newUuid}}@example.org",
  "roles": ["StandardUser"]
}
HTTP 201
[Captures]
user1_id: jsonpath "$.id"

POST http://localhost:3001/admin/api/v1/users
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "username": "test-user-2-{{newUuid}}",
  "email": "test-user-2-{{newUuid}}@example.org", 
  "roles": ["StandardUser"]
}
HTTP 201
[Captures]
user2_id: jsonpath "$.id"

# Create test groups
POST http://localhost:3001/admin/api/v1/groups
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "test-group-1-{{newUuid}}",
  "description": "Test group 1 for user relationships"
}
HTTP 201
[Captures]
group1_id: jsonpath "$.id"

POST http://localhost:3001/admin/api/v1/groups
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "test-group-2-{{newUuid}}",
  "description": "Test group 2 for user relationships"
}
HTTP 201
[Captures]
group2_id: jsonpath "$.id"

# Verify groups start empty
GET http://localhost:3001/admin/api/v1/groups/{{group1_id}}/users
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" count == 0

GET http://localhost:3001/admin/api/v1/groups/{{group2_id}}/users
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" count == 0

# Verify users start with only the Everyone group (nil UUID)
GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/groups
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" count == 1
jsonpath "$[0].id" == "00000000-0000-0000-0000-000000000000"

GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/groups
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" count == 1
jsonpath "$[0].id" == "00000000-0000-0000-0000-000000000000"

# Add user1 to group1 using POST /groups/{group_id}/users/{user_id}
POST http://localhost:3001/admin/api/v1/groups/{{group1_id}}/users/{{user1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Verify user1 is now in group1
GET http://localhost:3001/admin/api/v1/groups/{{group1_id}}/users
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" count == 1
jsonpath "$[0]" == "{{user1_id}}"

# Verify user1 shows group1 in their groups (plus Everyone group)
GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/groups
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" count == 2

# Add user2 to group1 using POST /users/{user_id}/groups/{group_id}
POST http://localhost:3001/admin/api/v1/users/{{user2_id}}/groups/{{group1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Verify both users are now in group1
GET http://localhost:3001/admin/api/v1/groups/{{group1_id}}/users
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" count == 2

# Add user1 to group2 as well (testing multiple group membership)
POST http://localhost:3001/admin/api/v1/groups/{{group2_id}}/users/{{user1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Verify user1 is now in both groups (plus Everyone group = 3 total)
GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/groups
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" count == 3

# Verify user2 is in group1 (plus Everyone group = 2 total)
GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/groups
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" count == 2

# Verify group2 has only user1
GET http://localhost:3001/admin/api/v1/groups/{{group2_id}}/users
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" count == 1
jsonpath "$[0]" == "{{user1_id}}"

# Test that regular users cannot access group membership endpoints
GET http://localhost:3001/admin/api/v1/groups/{{group1_id}}/users
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/groups
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

POST http://localhost:3001/admin/api/v1/groups/{{group1_id}}/users/{{user2_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# Remove user1 from group1 using DELETE /groups/{group_id}/users/{user_id}
DELETE http://localhost:3001/admin/api/v1/groups/{{group1_id}}/users/{{user1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Verify user1 was removed from group1
GET http://localhost:3001/admin/api/v1/groups/{{group1_id}}/users
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" count == 1
jsonpath "$[0]" == "{{user2_id}}"

# Verify user1 still has group2 (plus Everyone group = 2 total)
GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/groups
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" count == 2

# Remove user2 from group1 using DELETE /users/{user_id}/groups/{group_id}
DELETE http://localhost:3001/admin/api/v1/users/{{user2_id}}/groups/{{group1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Verify group1 is now empty
GET http://localhost:3001/admin/api/v1/groups/{{group1_id}}/users
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" count == 0

# Verify user2 has only Everyone group
GET http://localhost:3001/admin/api/v1/users/{{user2_id}}/groups
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" count == 1
jsonpath "$[0].id" == "00000000-0000-0000-0000-000000000000"

# Test adding user to non-existent group
POST http://localhost:3001/admin/api/v1/groups/00000000-0000-0000-0000-000000000001/users/{{user1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 404

# Test adding non-existent user to group
POST http://localhost:3001/admin/api/v1/groups/{{group1_id}}/users/00000000-0000-0000-0000-000000000001
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 404

# Test double-adding user to same group (should be idempotent)
POST http://localhost:3001/admin/api/v1/groups/{{group2_id}}/users/{{user1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Verify user1 is still in group2 (plus Everyone group = 2 total, not duplicated)
GET http://localhost:3001/admin/api/v1/users/{{user1_id}}/groups
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" count == 2

# Clean up: Remove remaining user-group relationships
DELETE http://localhost:3001/admin/api/v1/users/{{user1_id}}/groups/{{group2_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Clean up: Delete test groups
DELETE http://localhost:3001/admin/api/v1/groups/{{group1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

DELETE http://localhost:3001/admin/api/v1/groups/{{group2_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Clean up: Delete test users
DELETE http://localhost:3001/admin/api/v1/users/{{user1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

DELETE http://localhost:3001/admin/api/v1/users/{{user2_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204
