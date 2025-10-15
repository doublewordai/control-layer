# Successfully creating a group with unique name
POST http://localhost:3001/admin/api/v1/groups
[Cookies]
waycast_session: {{admin_jwt}}
{
  "name": "test-group-{{newUuid}}",
  "description": "Test group created by e2e tests"
}
HTTP 201
[Captures]
group_id: jsonpath "$.id"
group_name: jsonpath "$.name"

# Creating a group with the same name gives 409 error
POST http://localhost:3001/admin/api/v1/groups
[Cookies]
waycast_session: {{admin_jwt}}
{
  "name": "{{group_name}}",
  "description": "Duplicate group"
}
HTTP 409

# Get the created group by ID
GET http://localhost:3001/admin/api/v1/groups/{{group_id}}
[Cookies]
waycast_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.name" == "{{group_name}}"
jsonpath "$.description" == "Test group created by e2e tests"

# Update group description
PATCH http://localhost:3001/admin/api/v1/groups/{{group_id}}
[Cookies]
waycast_session: {{admin_jwt}}
{
  "description": "Updated description"
}
HTTP 200
[Asserts]
jsonpath "$.description" == "Updated description"

# List all groups (should include our test group)
GET http://localhost:3001/admin/api/v1/groups
[Cookies]
waycast_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection
jsonpath "$[?(@.name == '{{group_name}}')]" count == 1

# Delete the group
DELETE http://localhost:3001/admin/api/v1/groups/{{group_id}}
[Cookies]
waycast_session: {{admin_jwt}}
HTTP 204

# Verify group is deleted (should return 404)
GET http://localhost:3001/admin/api/v1/groups/{{group_id}}
[Cookies]
waycast_session: {{admin_jwt}}
HTTP 404
