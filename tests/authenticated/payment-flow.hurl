# Test complete payment flow using dummy provider
#
# PREREQUISITES: This test requires the server to be configured with a dummy payment provider.
#
# Add to config.yaml:
#   payment:
#     dummy:
#       host_url: "http://localhost:3001"
#       amount: 100.00
#
# Or via environment variables when running the server:
#   export DWCTL_PAYMENT__DUMMY__HOST_URL="http://localhost:3001"
#   export DWCTL_PAYMENT__DUMMY__AMOUNT=100.00
#
# Without this configuration, the test will fail with 503 Service Unavailable.

# Get current user to check initial balance
GET http://localhost:3001/admin/api/v1/users/current
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Captures]
initial_balance: jsonpath "$.credit_balance"

# Create payment checkout session
POST http://localhost:3001/admin/api/v1/payments
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.url" exists
jsonpath "$.url" contains "payment=success"
jsonpath "$.url" contains "session_id=dummy_session_"
[Captures]
checkout_url: jsonpath "$.url"

# Extract session_id from checkout URL
# In a real flow, the user would be redirected to the payment provider
# The dummy provider immediately creates the transaction, so we can process it
GET {{checkout_url}}
HTTP 200
[Captures]
session_id: query "session_id"

# Verify the transaction was created by the dummy provider
GET http://localhost:3001/admin/api/v1/users/current
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.credit_balance" > {{initial_balance}}
[Captures]
balance_after_checkout: jsonpath "$.credit_balance"

# Process payment manually (simulates frontend fallback path)
# This should be idempotent - the transaction already exists
PATCH http://localhost:3001/admin/api/v1/payments/{{session_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.message" == "Payment processed successfully"

# Verify balance hasn't changed (idempotent - no duplicate transaction)
GET http://localhost:3001/admin/api/v1/users/current
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.credit_balance" == {{balance_after_checkout}}

# Process payment again to verify idempotency
PATCH http://localhost:3001/admin/api/v1/payments/{{session_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.message" == "Payment processed successfully"

# Verify balance still hasn't changed (still idempotent)
GET http://localhost:3001/admin/api/v1/users/current
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.credit_balance" == {{balance_after_checkout}}

# List transactions to verify only one payment was recorded
GET http://localhost:3001/admin/api/v1/cost/transactions
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data[?(@.source_id=='{{session_id}}')]" count == 1
jsonpath "$.data[?(@.source_id=='{{session_id}}')].transaction_type" == "purchase"
jsonpath "$.data[?(@.source_id=='{{session_id}}')].description" == "Dummy payment (test)"
