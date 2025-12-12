# =============================================================================
# Daemons Permissions Test Suite
# =============================================================================
# Tests GET /ai/v1/daemons endpoint permissions
#
# Permission model:
# - PlatformManager: CAN list daemons (System::ReadAll permission)
# - StandardUser: CANNOT list daemons (403 Forbidden)
# - Unauthenticated: CANNOT list daemons (401 Unauthorized)
# =============================================================================

# -----------------------------------------------------------------------------
# GET /ai/v1/daemons - List Daemons Permissions
# -----------------------------------------------------------------------------

# StandardUser CANNOT list daemons
GET http://localhost:3001/ai/v1/daemons
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# PlatformManager CAN list daemons
GET http://localhost:3001/ai/v1/daemons
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.daemons" exists
jsonpath "$.daemons" isCollection

# PlatformManager CAN list daemons with status filter
GET http://localhost:3001/ai/v1/daemons?status=running
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.daemons" isCollection

# Unauthenticated request CANNOT list daemons
GET http://localhost:3001/ai/v1/daemons
HTTP 401