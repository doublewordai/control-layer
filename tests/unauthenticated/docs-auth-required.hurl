# Test that documentation endpoints require authentication
# Both public and internal docs should redirect unauthenticated users to login

# Test public docs requires auth
GET https://localhost/admin/docs/
HTTP 302
[Asserts]
header "Location" contains "/authentication/sign_in"
header "Location" contains "rd=https://localhost/admin/docs/"
