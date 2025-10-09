# HTTPS auth login redirects to OAuth provider
GET https://localhost/authentication/login?url=https://localhost/
HTTP 302
[Asserts]
# Should have required OAuth parameters
header "Location" contains "client_id="
header "Location" contains "redirect_uri="
header "Location" contains "response_type="
