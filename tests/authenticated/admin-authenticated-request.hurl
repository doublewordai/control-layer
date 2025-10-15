# Test authenticated request with JWT token
GET http://localhost:3001/admin/api/v1/users/current
[Cookies]
waycast_session: {{admin_jwt}}
HTTP 200 # starts the response body
