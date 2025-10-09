# Users can't create other users
POST http://localhost:3001/admin/api/v1/users
[Cookies]
clay_session: {{user_jwt}}
{
  "username": "test-user-{{newUuid}}",
  "email": "test-{{newUuid}}@example.org",
  "roles": ["StandardUser"]
}
HTTP 403
