# Successfully creating a user with unique identifier
POST http://localhost:3001/admin/api/v1/users
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "username": "test-user-{{newUuid}}",
  "email": "test-{{newUuid}}@example.org",
  "roles": ["StandardUser"]
}
HTTP 201
[Captures]
user_id: jsonpath "$.id"
user_email: jsonpath "$.email"

# Creating a user w/ the same email gives 409 error
POST http://localhost:3001/admin/api/v1/users
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "username": "test-user-duplicate",
  "email": "{{user_email}}",
  "roles": ["StandardUser"]
}
HTTP 409

# Delete the user
DELETE http://localhost:3001/admin/api/v1/users/{{user_id}}
[Cookies]
dwctl_session: {{admin_jwt}}

HTTP 204
