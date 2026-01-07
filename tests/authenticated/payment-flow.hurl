# Test complete payment flow using dummy provider
#
# PREREQUISITES: This test requires a properly running dwctl server configured with a dummy payment provider.
#
# 1. Start the server with:
#    cargo run --bin dwctl
#
# 2. Ensure config.yaml has:
#    payment:
#      dummy:
#        host_url: "http://localhost:3001"
#        amount: 100.00
#
# 3. Or use environment variables when running the server:
#    export DWCTL_PAYMENT__DUMMY__HOST_URL="http://localhost:3001"
#    export DWCTL_PAYMENT__DUMMY__AMOUNT=100.00
#
# NOTE: The comprehensive unit tests in dwctl/src/api/handlers/payments.rs and
# dwctl/src/payment_providers/{stripe,dummy}.rs provide full test coverage without
# requiring a running server. Run: cargo test payment

# Get current user to check initial balance
GET http://localhost:3001/admin/api/v1/users/current?include=billing
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
session_id: regex "session_id=([^&]+)"

# Verify NO transaction was created yet (matches real payment flow)
GET http://localhost:3001/admin/api/v1/users/current?include=billing
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.credit_balance" == {{initial_balance}}

# Process payment manually (simulates frontend callback after redirect)
PATCH http://localhost:3001/admin/api/v1/payments/{{session_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.message" == "Payment processed successfully"

# Capture balance after payment processed
GET http://localhost:3001/admin/api/v1/users/current?include=billing
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Captures]
balance_after_checkout: jsonpath "$.credit_balance"

# Process payment again to verify idempotency
PATCH http://localhost:3001/admin/api/v1/payments/{{session_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.message" == "Payment processed successfully"

# Verify balance unchanged after duplicate processing (idempotent)
GET http://localhost:3001/admin/api/v1/users/current?include=billing
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.credit_balance" == {{balance_after_checkout}}

# List transactions to verify payment was recorded
GET http://localhost:3001/admin/api/v1/transactions
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" isCollection
jsonpath "$.data[0].source_id" exists
jsonpath "$.data[0].transaction_type" exists
jsonpath "$.data[0].description" exists
