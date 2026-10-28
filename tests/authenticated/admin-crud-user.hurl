# SSO staff cannot provision another identity or delete a customer's account.
POST http://localhost:3001/admin/api/v1/users
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
{
  "username": "test-user-{{newUuid}}",
  "email": "test-{{newUuid}}@example.org",
  "roles": ["StandardUser"]
}
HTTP 403

GET http://localhost:3001/admin/api/v1/users/current
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Captures]
user_id: jsonpath "$.id"

DELETE http://localhost:3001/admin/api/v1/users/{{user_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 403

GET http://localhost:3001/admin/api/v1/users/{{user_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
