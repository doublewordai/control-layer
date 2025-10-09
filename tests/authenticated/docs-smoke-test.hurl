# Smoke test for documentation endpoints
# Both public and internal docs require authentication (public docs temporarily)

# Test public docs endpoint - should return HTML content
GET http://localhost:3001/admin/docs/
[Cookies]
clay_session: {{admin_jwt}}
HTTP 200
[Asserts]
header "Content-Type" contains "text/html"
body contains "Doubleword Control Layer"
