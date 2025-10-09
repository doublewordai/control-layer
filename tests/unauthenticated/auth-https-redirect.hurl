# HTTPS root redirects to auth login  
GET https://localhost/
HTTP 302
[Asserts]
header "Location" contains "/authentication/sign_in"