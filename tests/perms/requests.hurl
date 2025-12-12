# =============================================================================
# Requests Permissions Test Suite
# =============================================================================
# Tests all permission requirements for /admin/api/v1/requests endpoints
#
# Permission model:
# - StandardUser: CANNOT access any request data (raw or aggregated)
# - PlatformManager: CAN access Analytics (aggregated), CANNOT access Requests (raw logs)
#   - Has Analytics::ReadAll - can use /requests/aggregate and /requests/aggregate-by-user
#   - Does NOT have Requests::ReadAll - cannot use /requests
# - RequestViewer: CAN access both Requests (raw logs) and Analytics (aggregated)
#   - Has Requests::ReadAll - can use /requests
#   - Has Analytics::ReadAll - can use /requests/aggregate and /requests/aggregate-by-user
#
# Security principle: Platform managers can see aggregated metrics but not sensitive request details
# =============================================================================

# -----------------------------------------------------------------------------
# Setup: Get user IDs and grant roles for testing
# -----------------------------------------------------------------------------

# Get user IDs
GET http://localhost:3001/admin/api/v1/users/current
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Captures]
admin_user_id: jsonpath "$.id"

GET http://localhost:3001/admin/api/v1/users/current
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Captures]
user1_id: jsonpath "$.id"

GET http://localhost:3001/admin/api/v1/users/current
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Captures]
user2_id: jsonpath "$.id"

# Temporarily grant user2 PlatformManager role (without is_admin flag)
# This allows us to test PlatformManager permissions properly
PATCH http://localhost:3001/admin/api/v1/users/{{user2_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "roles": ["PlatformManager"]
}
HTTP 200

# =============================================================================
# Raw Request Logs (GET /requests) - Requires Requests::ReadAll
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT access raw request logs
# -----------------------------------------------------------------------------

# User1 tries to list requests - should be forbidden
GET http://localhost:3001/admin/api/v1/requests
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User1 tries with pagination - should be forbidden
GET http://localhost:3001/admin/api/v1/requests?skip=0&limit=10
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User1 tries with filters - should be forbidden
GET http://localhost:3001/admin/api/v1/requests?method=POST&status_code=200
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CANNOT access raw request logs (lacks Requests::ReadAll)
# -----------------------------------------------------------------------------

# User2 (PlatformManager) tries to list requests - should be forbidden
GET http://localhost:3001/admin/api/v1/requests
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# User2 tries with pagination - should be forbidden
GET http://localhost:3001/admin/api/v1/requests?skip=0&limit=10
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# User2 tries with time filters - should be forbidden
GET http://localhost:3001/admin/api/v1/requests?timestamp_after=2024-01-01T00:00:00Z
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# User2 tries with URI pattern - should be forbidden
GET http://localhost:3001/admin/api/v1/requests?uri_pattern=/ai/%
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# =============================================================================
# Aggregated Analytics (GET /requests/aggregate) - Requires Analytics::ReadAll
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT access aggregated analytics
# -----------------------------------------------------------------------------

# User1 tries to get aggregate - should be forbidden
GET http://localhost:3001/admin/api/v1/requests/aggregate
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User1 tries with model filter - should be forbidden
GET http://localhost:3001/admin/api/v1/requests/aggregate?model=gpt-4
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User1 tries with time range - should be forbidden
GET http://localhost:3001/admin/api/v1/requests/aggregate?timestamp_after=2024-01-01T00:00:00Z&timestamp_before=2024-12-31T23:59:59Z
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN access aggregated analytics (has Analytics::ReadAll)
# -----------------------------------------------------------------------------

# User2 (PlatformManager) gets aggregate without filters - should succeed
GET http://localhost:3001/admin/api/v1/requests/aggregate
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.total_requests" exists
jsonpath "$.status_codes" exists
jsonpath "$.time_series" exists

# User2 gets aggregate with model filter - should succeed
GET http://localhost:3001/admin/api/v1/requests/aggregate?model=gpt-4
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.total_requests" exists
jsonpath "$.model" == "gpt-4"

# User2 gets aggregate with time range - should succeed
GET http://localhost:3001/admin/api/v1/requests/aggregate?timestamp_after=2024-01-01T00:00:00Z&timestamp_before=2024-12-31T23:59:59Z
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.total_requests" exists

# =============================================================================
# User Aggregation (GET /requests/aggregate-by-user) - Requires Analytics::ReadAll
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT access user aggregation
# -----------------------------------------------------------------------------

# User1 tries to get user aggregation - should be forbidden (even without model)
GET http://localhost:3001/admin/api/v1/requests/aggregate-by-user
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User1 tries with model parameter - should be forbidden
GET http://localhost:3001/admin/api/v1/requests/aggregate-by-user?model=gpt-4
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User1 tries with time range - should be forbidden
GET http://localhost:3001/admin/api/v1/requests/aggregate-by-user?model=gpt-4&start_date=2024-01-01T00:00:00Z&end_date=2024-12-31T23:59:59Z
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN access user aggregation (has Analytics::ReadAll)
# -----------------------------------------------------------------------------

# User2 (PlatformManager) gets user aggregation without model - should fail (model required)
GET http://localhost:3001/admin/api/v1/requests/aggregate-by-user
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 400

# User2 gets user aggregation with model - should succeed
GET http://localhost:3001/admin/api/v1/requests/aggregate-by-user?model=gpt-4
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.model" == "gpt-4"
jsonpath "$.start_date" exists
jsonpath "$.end_date" exists
jsonpath "$.users" exists

# User2 gets user aggregation with time range - should succeed
GET http://localhost:3001/admin/api/v1/requests/aggregate-by-user?model=claude-3&start_date=2024-01-01T00:00:00Z&end_date=2024-12-31T23:59:59Z
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.model" == "claude-3"

# User2 can query models they don't have access to (Analytics is platform-wide)
GET http://localhost:3001/admin/api/v1/requests/aggregate-by-user?model=some-other-model
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.model" == "some-other-model"

# =============================================================================
# Verification: PlatformManager has NO access to raw logs
# =============================================================================

# Verify user2 (PlatformManager) cannot bypass with query parameters
GET http://localhost:3001/admin/api/v1/requests?skip=0&limit=1&method=POST&status_code=200
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# Verify user2 cannot get specific time-filtered requests
GET http://localhost:3001/admin/api/v1/requests?timestamp_after=2024-01-01T00:00:00Z&timestamp_before=2024-01-02T00:00:00Z
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# =============================================================================
# Security Verification: Analytics endpoints don't leak sensitive data
# =============================================================================

# Verify aggregate response doesn't contain request/response bodies
GET http://localhost:3001/admin/api/v1/requests/aggregate
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
# Should only have aggregated metrics, not raw data
jsonpath "$.total_requests" exists
jsonpath "$.status_codes" isCollection
jsonpath "$.time_series" isCollection
# Should NOT have request bodies, headers, or other sensitive data
jsonpath "$[?(@.body)]" count == 0
jsonpath "$[?(@.headers)]" count == 0
jsonpath "$[?(@.request)]" count == 0
jsonpath "$[?(@.response)]" count == 0

# Verify user aggregation doesn't contain request/response bodies
GET http://localhost:3001/admin/api/v1/requests/aggregate-by-user?model=gpt-4
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
# Should only have user statistics, not raw data
jsonpath "$.users" isCollection
jsonpath "$.total_requests" exists
# Should NOT have request bodies or sensitive details
jsonpath "$.users[*][?(@.request)]" count == 0
jsonpath "$.users[*][?(@.response)]" count == 0
jsonpath "$.users[*][?(@.body)]" count == 0
jsonpath "$.users[*][?(@.headers)]" count == 0

# =============================================================================
# Cleanup: Restore user2 to original roles
# =============================================================================

# Restore user2 to StandardUser + BatchAPIUser
PATCH http://localhost:3001/admin/api/v1/users/{{user2_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "roles": ["StandardUser", "BatchAPIUser"]
}
HTTP 200

# =============================================================================
# Cleanup Complete
# =============================================================================