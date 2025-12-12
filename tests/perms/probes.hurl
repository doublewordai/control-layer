# =============================================================================
# Probes Permissions Test Suite
# =============================================================================
# Tests all CRUD operations on /admin/api/v1/probes endpoints
#
# Permission model:
# - StandardUser: CANNOT interact with probes in any way
#   - Cannot create, read, update, delete, activate, deactivate, or execute probes
#   - Cannot view probe results or statistics
# - PlatformManager: Full access to all probe operations
#   - Can create, read, update, and delete any probe
#   - Can activate, deactivate, and execute probes
#   - Can view probe results and statistics
#
# Note: Database has unique constraint on deployment_id, so each probe needs its own model
# =============================================================================

# -----------------------------------------------------------------------------
# Setup: Get user IDs and create test resources
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

# Create test endpoint for probes
POST http://localhost:3001/admin/api/v1/endpoints
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "test-probe-endpoint",
  "url": "http://mock-llm-server:3000",
  "sync": false
}
HTTP 201
[Captures]
probe_endpoint_id: jsonpath "$.id"

# Create model 1 for probe 1 (unique constraint requires one probe per model)
POST http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "model_name": "test-probe-model-1",
  "alias": "test-probe-model-1-alias",
  "hosted_on": "{{probe_endpoint_id}}",
  "requests_per_second": 100.0,
  "burst_size": 200,
  "pricing": {
    "input_price_per_token": "0.001",
    "output_price_per_token": "0.002"
  },
  "downstream_pricing": {
    "mode": "per_token",
    "input_price_per_token": "0.0005",
    "output_price_per_token": "0.001"
  }
}
HTTP 200
[Captures]
probe_model1_id: jsonpath "$.id"

# Create model 2 for probe 2
POST http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "model_name": "test-probe-model-2",
  "alias": "test-probe-model-2-alias",
  "hosted_on": "{{probe_endpoint_id}}",
  "requests_per_second": 100.0,
  "burst_size": 200,
  "pricing": {
    "input_price_per_token": "0.001",
    "output_price_per_token": "0.002"
  },
  "downstream_pricing": {
    "mode": "per_token",
    "input_price_per_token": "0.0005",
    "output_price_per_token": "0.001"
  }
}
HTTP 200
[Captures]
probe_model2_id: jsonpath "$.id"

# Create model 3 for probe 3
POST http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "model_name": "test-probe-model-3",
  "alias": "test-probe-model-3-alias",
  "hosted_on": "{{probe_endpoint_id}}",
  "requests_per_second": 100.0,
  "burst_size": 200,
  "pricing": {
    "input_price_per_token": "0.001",
    "output_price_per_token": "0.002"
  },
  "downstream_pricing": {
    "mode": "per_token",
    "input_price_per_token": "0.0005",
    "output_price_per_token": "0.001"
  }
}
HTTP 200
[Captures]
probe_model3_id: jsonpath "$.id"

# =============================================================================
# Probe Creation Permissions (POST /probes)
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT create probes
# -----------------------------------------------------------------------------

# User1 tries to create a probe - should be forbidden
POST http://localhost:3001/admin/api/v1/probes
[Cookies]
dwctl_session: {{user_jwt}}
{
  "name": "user1-probe",
  "deployment_id": "{{probe_model1_id}}",
  "interval_seconds": 60
}
HTTP 403

# User2 tries to create a probe - should be forbidden
POST http://localhost:3001/admin/api/v1/probes
[Cookies]
dwctl_session: {{user2_jwt}}
{
  "name": "user2-probe",
  "deployment_id": "{{probe_model2_id}}",
  "interval_seconds": 120
}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN create probes
# -----------------------------------------------------------------------------

# Admin creates probe 1
POST http://localhost:3001/admin/api/v1/probes
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "test-probe-1",
  "deployment_id": "{{probe_model1_id}}",
  "interval_seconds": 60,
  "http_method": "POST",
  "request_path": "/v1/chat/completions",
  "request_body": {
    "model": "test-probe-model-1",
    "messages": [{"role": "user", "content": "test"}]
  }
}
HTTP 201
[Asserts]
jsonpath "$.id" exists
jsonpath "$.name" == "test-probe-1"
jsonpath "$.deployment_id" == "{{probe_model1_id}}"
jsonpath "$.interval_seconds" == 60
jsonpath "$.active" == true
[Captures]
probe1_id: jsonpath "$.id"

# Admin creates probe 2
POST http://localhost:3001/admin/api/v1/probes
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "test-probe-2",
  "deployment_id": "{{probe_model2_id}}",
  "interval_seconds": 120
}
HTTP 201
[Asserts]
jsonpath "$.id" exists
jsonpath "$.name" == "test-probe-2"
[Captures]
probe2_id: jsonpath "$.id"

# Admin creates probe 3
POST http://localhost:3001/admin/api/v1/probes
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "test-probe-3",
  "deployment_id": "{{probe_model3_id}}",
  "interval_seconds": 300
}
HTTP 201
[Asserts]
jsonpath "$.id" exists
jsonpath "$.name" == "test-probe-3"
[Captures]
probe3_id: jsonpath "$.id"

# =============================================================================
# Probe Listing Permissions (GET /probes)
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT list probes
# -----------------------------------------------------------------------------

# User1 tries to list probes - should be forbidden
GET http://localhost:3001/admin/api/v1/probes
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User2 tries to list probes - should be forbidden
GET http://localhost:3001/admin/api/v1/probes
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# User1 tries to list active probes - should be forbidden
GET http://localhost:3001/admin/api/v1/probes?status=active
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN list all probes
# -----------------------------------------------------------------------------

# Admin lists all probes - should see all 3 probes
GET http://localhost:3001/admin/api/v1/probes
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection
jsonpath "$[*].id" contains "{{probe1_id}}"
jsonpath "$[*].id" contains "{{probe2_id}}"
jsonpath "$[*].id" contains "{{probe3_id}}"

# Admin lists active probes - should see all 3 (all are active by default)
GET http://localhost:3001/admin/api/v1/probes?status=active
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection
jsonpath "$[*].id" contains "{{probe1_id}}"

# =============================================================================
# Probe Retrieval Permissions (GET /probes/{id})
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT fetch probes
# -----------------------------------------------------------------------------

# User1 tries to fetch probe1 - should be forbidden
GET http://localhost:3001/admin/api/v1/probes/{{probe1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User1 tries to fetch probe2 - should be forbidden
GET http://localhost:3001/admin/api/v1/probes/{{probe2_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User2 tries to fetch any probe - should be forbidden
GET http://localhost:3001/admin/api/v1/probes/{{probe3_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN fetch any probe
# -----------------------------------------------------------------------------

# Admin fetches probe1 - should succeed
GET http://localhost:3001/admin/api/v1/probes/{{probe1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{probe1_id}}"
jsonpath "$.name" == "test-probe-1"
jsonpath "$.deployment_id" == "{{probe_model1_id}}"

# Admin fetches probe2 - should succeed
GET http://localhost:3001/admin/api/v1/probes/{{probe2_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{probe2_id}}"
jsonpath "$.name" == "test-probe-2"

# Admin fetches probe3 - should succeed
GET http://localhost:3001/admin/api/v1/probes/{{probe3_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{probe3_id}}"
jsonpath "$.name" == "test-probe-3"

# =============================================================================
# Probe Update Permissions (PATCH /probes/{id})
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT update probes
# -----------------------------------------------------------------------------

# User1 tries to update probe1 - should be forbidden
PATCH http://localhost:3001/admin/api/v1/probes/{{probe1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
{
  "interval_seconds": 90
}
HTTP 403

# User1 tries to update probe2 - should be forbidden
PATCH http://localhost:3001/admin/api/v1/probes/{{probe2_id}}
[Cookies]
dwctl_session: {{user_jwt}}
{
  "interval_seconds": 180
}
HTTP 403

# User2 tries to update any probe - should be forbidden
PATCH http://localhost:3001/admin/api/v1/probes/{{probe3_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
{
  "interval_seconds": 240
}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN update any probe
# -----------------------------------------------------------------------------

# Admin updates probe1 - should succeed
PATCH http://localhost:3001/admin/api/v1/probes/{{probe1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "interval_seconds": 90
}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{probe1_id}}"
jsonpath "$.interval_seconds" == 90

# Admin updates probe2 - should succeed
PATCH http://localhost:3001/admin/api/v1/probes/{{probe2_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "http_method": "GET"
}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{probe2_id}}"
jsonpath "$.http_method" == "GET"

# =============================================================================
# Probe Deactivation Permissions (PATCH /probes/{id}/deactivate)
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT deactivate probes
# -----------------------------------------------------------------------------

# User1 tries to deactivate probe1 - should be forbidden
PATCH http://localhost:3001/admin/api/v1/probes/{{probe1_id}}/deactivate
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User2 tries to deactivate probe2 - should be forbidden
PATCH http://localhost:3001/admin/api/v1/probes/{{probe2_id}}/deactivate
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN deactivate probes
# -----------------------------------------------------------------------------

# Admin deactivates probe3 - should succeed
PATCH http://localhost:3001/admin/api/v1/probes/{{probe3_id}}/deactivate
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{probe3_id}}"
jsonpath "$.active" == false

# =============================================================================
# Probe Activation Permissions (PATCH /probes/{id}/activate)
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT activate probes
# -----------------------------------------------------------------------------

# User1 tries to activate probe3 - should be forbidden
PATCH http://localhost:3001/admin/api/v1/probes/{{probe3_id}}/activate
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User2 tries to activate probe3 - should be forbidden
PATCH http://localhost:3001/admin/api/v1/probes/{{probe3_id}}/activate
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN activate probes
# -----------------------------------------------------------------------------

# Admin activates probe3 - should succeed
PATCH http://localhost:3001/admin/api/v1/probes/{{probe3_id}}/activate
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{probe3_id}}"
jsonpath "$.active" == true

# =============================================================================
# Probe Execution Permissions (POST /probes/{id}/execute)
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT execute probes
# -----------------------------------------------------------------------------

# User1 tries to execute probe1 - should be forbidden
POST http://localhost:3001/admin/api/v1/probes/{{probe1_id}}/execute
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User2 tries to execute probe2 - should be forbidden
POST http://localhost:3001/admin/api/v1/probes/{{probe2_id}}/execute
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN execute probes
# -----------------------------------------------------------------------------

# Admin executes probe1 - should succeed
POST http://localhost:3001/admin/api/v1/probes/{{probe1_id}}/execute
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 201
[Asserts]
jsonpath "$.probe_id" == "{{probe1_id}}"
jsonpath "$.executed_at" exists

# =============================================================================
# Probe Testing Permissions (POST /probes/test/{deployment_id})
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT test probes
# -----------------------------------------------------------------------------

# User1 tries to test probe configuration - should be forbidden
POST http://localhost:3001/admin/api/v1/probes/test/{{probe_model1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
{
  "http_method": "POST",
  "request_path": "/v1/chat/completions"
}
HTTP 403

# User2 tries to test probe configuration - should be forbidden
POST http://localhost:3001/admin/api/v1/probes/test/{{probe_model2_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
{}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN test probes
# -----------------------------------------------------------------------------

# Admin tests probe configuration - should succeed
POST http://localhost:3001/admin/api/v1/probes/test/{{probe_model1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "http_method": "POST",
  "request_path": "/v1/chat/completions",
  "request_body": {
    "model": "test-probe-model-1",
    "messages": [{"role": "user", "content": "test"}]
  }
}
HTTP 200
[Asserts]
jsonpath "$.executed_at" exists

# =============================================================================
# Probe Results Permissions (GET /probes/{id}/results)
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT view probe results
# -----------------------------------------------------------------------------

# User1 tries to view probe1 results - should be forbidden
GET http://localhost:3001/admin/api/v1/probes/{{probe1_id}}/results
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User2 tries to view probe2 results - should be forbidden
GET http://localhost:3001/admin/api/v1/probes/{{probe2_id}}/results
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# User1 tries to view probe results with filters - should be forbidden
GET http://localhost:3001/admin/api/v1/probes/{{probe1_id}}/results?limit=10
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN view probe results
# -----------------------------------------------------------------------------

# Admin views probe1 results - should succeed
GET http://localhost:3001/admin/api/v1/probes/{{probe1_id}}/results
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection

# Admin views probe1 results with limit - should succeed
GET http://localhost:3001/admin/api/v1/probes/{{probe1_id}}/results?limit=5
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$" isCollection

# =============================================================================
# Probe Statistics Permissions (GET /probes/{id}/statistics)
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT view probe statistics
# -----------------------------------------------------------------------------

# User1 tries to view probe1 statistics - should be forbidden
GET http://localhost:3001/admin/api/v1/probes/{{probe1_id}}/statistics
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User2 tries to view probe2 statistics - should be forbidden
GET http://localhost:3001/admin/api/v1/probes/{{probe2_id}}/statistics
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN view probe statistics
# -----------------------------------------------------------------------------

# Admin views probe1 statistics - should succeed
GET http://localhost:3001/admin/api/v1/probes/{{probe1_id}}/statistics
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.total_executions" exists
jsonpath "$.success_rate" exists

# Admin views probe2 statistics - should succeed
GET http://localhost:3001/admin/api/v1/probes/{{probe2_id}}/statistics
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.total_executions" exists

# =============================================================================
# Probe Deletion Permissions (DELETE /probes/{id})
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT delete probes
# -----------------------------------------------------------------------------

# User1 tries to delete probe1 - should be forbidden
DELETE http://localhost:3001/admin/api/v1/probes/{{probe1_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User2 tries to delete probe2 - should be forbidden
DELETE http://localhost:3001/admin/api/v1/probes/{{probe2_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN delete probes
# -----------------------------------------------------------------------------

# Admin deletes probe3 - should succeed
DELETE http://localhost:3001/admin/api/v1/probes/{{probe3_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Verify probe3 is deleted - should get 404
GET http://localhost:3001/admin/api/v1/probes/{{probe3_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 404

# Admin deletes probe2 - should succeed
DELETE http://localhost:3001/admin/api/v1/probes/{{probe2_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Admin deletes probe1 - should succeed
DELETE http://localhost:3001/admin/api/v1/probes/{{probe1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# =============================================================================
# Cleanup: Delete test models and endpoint
# =============================================================================

# Delete model 3
DELETE http://localhost:3001/admin/api/v1/models/{{probe_model3_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200

# Delete model 2
DELETE http://localhost:3001/admin/api/v1/models/{{probe_model2_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200

# Delete model 1
DELETE http://localhost:3001/admin/api/v1/models/{{probe_model1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200

# Delete endpoint
DELETE http://localhost:3001/admin/api/v1/endpoints/{{probe_endpoint_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# =============================================================================
# Cleanup Complete
# =============================================================================