# Neither native admin sessions nor trusted SSO may grant user roles.
GET http://localhost:3001/admin/api/v1/users/current
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Captures]
test_user_id: jsonpath "$.id"

PATCH http://localhost:3001/admin/api/v1/users/{{test_user_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
{"roles": ["PlatformManager"]}
HTTP 403

PATCH http://localhost:3001/admin/api/v1/users/{{test_user_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
{"roles": ["PlatformManager", "RequestViewer"]}
HTTP 403

GET http://localhost:3001/admin/api/v1/users/{{test_user_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 200
[Asserts]
jsonpath "$.roles" contains "StandardUser"
jsonpath "$.roles" contains "BatchAPIUser"
jsonpath "$.roles" contains "BackgroundInferenceUser"
jsonpath "$.roles" not contains "PlatformManager"
jsonpath "$.roles" not contains "RequestViewer"

# A password session cannot inherit the admin account's platform authority.
GET http://localhost:3001/admin/api/v1/users
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 403

GET http://localhost:3001/admin/api/v1/users
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 200
