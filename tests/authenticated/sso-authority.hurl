# Platform-purpose keys retain PlatformManager configuration authority.
# Invalid credentials cannot fall back to SSO; customer ownership still applies.
GET http://localhost:3001/admin/api/v1/users/current
X-Doubleword-User: hurl-support-customer
X-Doubleword-Email: hurl-support-customer@example.org
HTTP 200
[Captures]
customer_id: jsonpath "$.id"

# Manager platform keys retain platform authority without a legacy admin flag.
GET http://localhost:3001/admin/api/v1/users/current
X-Doubleword-User: hurl-platform-manager
X-Doubleword-Email: hurl-platform-manager@example.org
HTTP 200
[Captures]
manager_id: jsonpath "$.id"

POST http://localhost:3001/admin/api/v1/users/{{manager_id}}/api-keys
X-Doubleword-User: hurl-platform-manager
X-Doubleword-Email: hurl-platform-manager@example.org
{"name":"SSO boundary regression", "purpose":"platform"}
HTTP 201
[Captures]
manager_key: jsonpath "$.key"
manager_key_id: jsonpath "$.id"

GET http://localhost:3001/admin/api/v1/users/current
Authorization: Bearer {{manager_key}}
HTTP 200
[Asserts]
jsonpath "$.is_admin" == false
jsonpath "$.roles" contains "StandardUser"
jsonpath "$.roles" contains "PlatformManager"

GET http://localhost:3001/admin/api/v1/users
Authorization: Bearer {{manager_key}}
X-Doubleword-User: hurl-platform-manager
X-Doubleword-Email: hurl-platform-manager@example.org
HTTP 200

GET http://localhost:3001/admin/api/v1/users
Authorization: Bearer invalid-key
X-Doubleword-User: hurl-platform-manager
X-Doubleword-Email: hurl-platform-manager@example.org
HTTP 401

POST http://localhost:3001/admin/api/v1/transactions
Authorization: Bearer {{manager_key}}
{"user_id":"{{customer_id}}", "transaction_type":"admin_grant", "amount":"1", "source_id":"{{manager_id}}-api"}
HTTP 201

PATCH http://localhost:3001/admin/api/v1/users/{{customer_id}}
Authorization: Bearer {{manager_key}}
{"zero_data_retention":true}
HTTP 200

POST http://localhost:3001/admin/api/v1/transactions
X-Doubleword-User: hurl-platform-manager
X-Doubleword-Email: hurl-platform-manager@example.org
{"user_id":"{{customer_id}}", "transaction_type":"admin_grant", "amount":"1", "source_id":"{{manager_id}}"}
HTTP 201
[Asserts]
jsonpath "$.user_id" == "{{customer_id}}"

PATCH http://localhost:3001/admin/api/v1/users/{{customer_id}}
X-Doubleword-User: hurl-platform-manager
X-Doubleword-Email: hurl-platform-manager@example.org
{"zero_data_retention":true}
HTTP 200
[Asserts]
jsonpath "$.zero_data_retention" == true

PATCH http://localhost:3001/admin/api/v1/users/{{customer_id}}
X-Doubleword-User: hurl-platform-manager
X-Doubleword-Email: hurl-platform-manager@example.org
{"zero_data_retention":false}
HTTP 403

PATCH http://localhost:3001/admin/api/v1/users/{{customer_id}}
X-Doubleword-User: hurl-platform-manager
X-Doubleword-Email: hurl-platform-manager@example.org
{"zero_data_retention":true, "roles":["PlatformManager"]}
HTTP 403

DELETE http://localhost:3001/admin/api/v1/users/{{manager_id}}/api-keys/{{manager_key_id}}
Authorization: Bearer {{manager_key}}
HTTP 204

# An admin account with PlatformManager retains that role, not its admin flag.
GET http://localhost:3001/admin/api/v1/users/current
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 200
[Captures]
admin_id: jsonpath "$.id"

POST http://localhost:3001/admin/api/v1/users/{{admin_id}}/api-keys
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
{"name":"SSO boundary regression", "purpose":"platform"}
HTTP 201
[Captures]
admin_key: jsonpath "$.key"
admin_key_id: jsonpath "$.id"

GET http://localhost:3001/admin/api/v1/users/current
Authorization: Bearer {{admin_key}}
HTTP 200
[Asserts]
jsonpath "$.is_admin" == false
jsonpath "$.roles" contains "StandardUser"
jsonpath "$.roles" contains "PlatformManager"

GET http://localhost:3001/admin/api/v1/users
Authorization: Bearer {{admin_key}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 200

GET http://localhost:3001/admin/api/v1/users
Authorization: Bearer invalid-key
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
HTTP 401

POST http://localhost:3001/admin/api/v1/transactions
Authorization: Bearer {{admin_key}}
{"user_id":"{{customer_id}}", "transaction_type":"admin_grant", "amount":"1", "source_id":"{{admin_id}}-api"}
HTTP 201

PATCH http://localhost:3001/admin/api/v1/users/{{customer_id}}
Authorization: Bearer {{admin_key}}
{"zero_data_retention":true}
HTTP 200

POST http://localhost:3001/admin/api/v1/transactions
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
{"user_id":"{{customer_id}}", "transaction_type":"admin_grant", "amount":"1", "source_id":"{{admin_id}}"}
HTTP 201
[Asserts]
jsonpath "$.user_id" == "{{customer_id}}"

PATCH http://localhost:3001/admin/api/v1/users/{{customer_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
{"zero_data_retention":true}
HTTP 200
[Asserts]
jsonpath "$.zero_data_retention" == true

PATCH http://localhost:3001/admin/api/v1/users/{{customer_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
{"zero_data_retention":false}
HTTP 403

PATCH http://localhost:3001/admin/api/v1/users/{{customer_id}}
X-Doubleword-User: {{admin_email}}
X-Doubleword-Email: {{admin_email}}
{"zero_data_retention":true, "roles":["PlatformManager"]}
HTTP 403

DELETE http://localhost:3001/admin/api/v1/users/{{admin_id}}/api-keys/{{admin_key_id}}
Authorization: Bearer {{admin_key}}
HTTP 204
