# Test authenticated request with JWT token
GET http://localhost:3001/admin/api/v1/users/current
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 200 # starts the response body
