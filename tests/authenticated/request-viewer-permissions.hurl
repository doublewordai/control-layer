# Request metadata visibility is an SSO-only role, independent of platform management.
# These identities are provisioned by scripts/seed-auth-test-users.sql.
GET http://localhost:3001/admin/api/v1/requests
X-Doubleword-User: hurl-platform-manager
X-Doubleword-Email: hurl-platform-manager@example.org
HTTP 403

GET http://localhost:3001/admin/api/v1/requests/aggregate
X-Doubleword-User: hurl-platform-manager
X-Doubleword-Email: hurl-platform-manager@example.org
HTTP 200

GET http://localhost:3001/admin/api/v1/requests
X-Doubleword-User: hurl-request-viewer
X-Doubleword-Email: hurl-request-viewer@example.org
HTTP 200

GET http://localhost:3001/admin/api/v1/users/current
X-Doubleword-User: hurl-request-viewer
X-Doubleword-Email: hurl-request-viewer@example.org
HTTP 200
[Captures]
viewer_id: jsonpath "$.id"

POST http://localhost:3001/admin/api/v1/users/{{viewer_id}}/api-keys
X-Doubleword-User: hurl-request-viewer
X-Doubleword-Email: hurl-request-viewer@example.org
{"name":"viewer ordinary key", "purpose":"platform"}
HTTP 201
[Captures]
key: jsonpath "$.key"
key_id: jsonpath "$.id"

GET http://localhost:3001/admin/api/v1/requests
Authorization: Bearer {{key}}
HTTP 403

GET http://localhost:3001/admin/api/v1/requests/aggregate
Authorization: Bearer {{key}}
HTTP 403

DELETE http://localhost:3001/admin/api/v1/users/{{viewer_id}}/api-keys/{{key_id}}
Authorization: Bearer {{key}}
HTTP 204
