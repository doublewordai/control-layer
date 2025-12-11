# =============================================================================
# Models (Deployments) Permissions Test Suite
# =============================================================================
# Tests all CRUD operations on /admin/api/v1/models endpoint
#
# Permission model:
# - StandardUser: Can only see models they have group access to
#   - Cannot create, update, or delete models
#   - No enriched fields (groups, pricing, rate_limits, metrics) even if requested
# - PlatformManager: Can see ALL models regardless of groups
#   - Can create, update, and delete models
#   - Gets all enriched fields with include parameter
# =============================================================================

# -----------------------------------------------------------------------------
# Setup: Create test endpoint and groups
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

# Create test endpoint for models
POST http://localhost:3001/admin/api/v1/endpoints
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "models-test-endpoint-{{newUuid}}",
  "url": "http://mock-llm-server:3000",
  "sync": false
}
HTTP 201
[Captures]
models_test_endpoint_id: jsonpath "$.id"

# Create user1's group (user1 will have access to models in this group)
POST http://localhost:3001/admin/api/v1/groups
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "models-user1-group-{{newUuid}}",
  "description": "Group for user1 model access"
}
HTTP 201
[Captures]
user1_group_id: jsonpath "$.id"

# Add user1 to their group
POST http://localhost:3001/admin/api/v1/groups/{{user1_group_id}}/users/{{user1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Create user2's group (user2 will have access to models in this group)
POST http://localhost:3001/admin/api/v1/groups
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "models-user2-group-{{newUuid}}",
  "description": "Group for user2 model access"
}
HTTP 201
[Captures]
user2_group_id: jsonpath "$.id"

# Add user2 to their group
POST http://localhost:3001/admin/api/v1/groups/{{user2_group_id}}/users/{{user2_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Create "Everyone" group for public models
POST http://localhost:3001/admin/api/v1/groups
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "name": "models-everyone-group-{{newUuid}}",
  "description": "Public models accessible to all users"
}
HTTP 201
[Captures]
everyone_group_id: jsonpath "$.id"

# Add all users to everyone group
POST http://localhost:3001/admin/api/v1/groups/{{everyone_group_id}}/users/{{user1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

POST http://localhost:3001/admin/api/v1/groups/{{everyone_group_id}}/users/{{user2_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# =============================================================================
# Model Creation Permissions Tests
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT create models
# -----------------------------------------------------------------------------

# User1 tries to create a model - should be forbidden
POST http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{user_jwt}}
{
  "model_name": "unauthorized-model",
  "alias": "unauthorized-alias",
  "hosted_on": "{{models_test_endpoint_id}}"
}
HTTP 403

# User2 tries to create a model - should be forbidden
POST http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{user2_jwt}}
{
  "model_name": "unauthorized-model-2",
  "alias": "unauthorized-alias-2",
  "hosted_on": "{{models_test_endpoint_id}}"
}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN create models
# -----------------------------------------------------------------------------

# Create model 1: Accessible to user1 only
POST http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "model_name": "user1-model",
  "alias": "user1-model-alias",
  "hosted_on": "{{models_test_endpoint_id}}",
  "requests_per_second": 100.0,
  "burst_size": 200
}
HTTP 200
[Asserts]
jsonpath "$.id" exists
jsonpath "$.model_name" == "user1-model"
jsonpath "$.alias" == "user1-model-alias"
[Captures]
user1_model_id: jsonpath "$.id"

# Add model 1 to user1's group
POST http://localhost:3001/admin/api/v1/groups/{{user1_group_id}}/models/{{user1_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Create model 2: Accessible to user2 only
POST http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "model_name": "user2-model",
  "alias": "user2-model-alias",
  "hosted_on": "{{models_test_endpoint_id}}",
  "requests_per_second": 100.0,
  "burst_size": 200
}
HTTP 200
[Asserts]
jsonpath "$.id" exists
[Captures]
user2_model_id: jsonpath "$.id"

# Add model 2 to user2's group
POST http://localhost:3001/admin/api/v1/groups/{{user2_group_id}}/models/{{user2_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Create model 3: Accessible to everyone
POST http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "model_name": "public-model",
  "alias": "public-model-alias",
  "hosted_on": "{{models_test_endpoint_id}}",
  "requests_per_second": 100.0,
  "burst_size": 200
}
HTTP 200
[Asserts]
jsonpath "$.id" exists
[Captures]
public_model_id: jsonpath "$.id"

# Add model 3 to everyone group
POST http://localhost:3001/admin/api/v1/groups/{{everyone_group_id}}/models/{{public_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Create model 4: No group access (admin only)
POST http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "model_name": "admin-only-model",
  "alias": "admin-only-model-alias",
  "hosted_on": "{{models_test_endpoint_id}}",
  "requests_per_second": 100.0,
  "burst_size": 200
}
HTTP 200
[Asserts]
jsonpath "$.id" exists
[Captures]
admin_only_model_id: jsonpath "$.id"

# Create model 5: Will be deleted (for deleted model tests)
POST http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "model_name": "deleted-model",
  "alias": "deleted-model-alias",
  "hosted_on": "{{models_test_endpoint_id}}",
  "requests_per_second": 100.0,
  "burst_size": 200
}
HTTP 200
[Asserts]
jsonpath "$.id" exists
[Captures]
deleted_model_id: jsonpath "$.id"

# Add deleted model to user1's group (so user1 could see it if not deleted)
POST http://localhost:3001/admin/api/v1/groups/{{user1_group_id}}/models/{{deleted_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Delete model 5
DELETE http://localhost:3001/admin/api/v1/models/{{deleted_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200

# =============================================================================
# StandardUser Permissions Tests (LIST)
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT create models
# -----------------------------------------------------------------------------

# User1 tries to create a model - should be forbidden
POST http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{user_jwt}}
{
  "model_name": "unauthorized-model",
  "alias": "unauthorized-alias",
  "hosted_on": "{{models_test_endpoint_id}}"
}
HTTP 403

# User2 tries to create a model - should be forbidden
POST http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{user2_jwt}}
{
  "model_name": "unauthorized-model-2",
  "alias": "unauthorized-alias-2",
  "hosted_on": "{{models_test_endpoint_id}}"
}
HTTP 403

# Delete model 5
DELETE http://localhost:3001/admin/api/v1/models/{{deleted_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200

# =============================================================================
# Model Update Permissions Tests (PATCH)
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT update models
# -----------------------------------------------------------------------------

# User1 tries to update their own accessible model - should be forbidden
PATCH http://localhost:3001/admin/api/v1/models/{{user1_model_id}}
[Cookies]
dwctl_session: {{user_jwt}}
{
  "alias": "user1-updated-alias"
}
HTTP 403

# User1 tries to update a model they can't see - should be forbidden
PATCH http://localhost:3001/admin/api/v1/models/{{admin_only_model_id}}
[Cookies]
dwctl_session: {{user_jwt}}
{
  "alias": "unauthorized-update"
}
HTTP 403

# User2 tries to update a model - should be forbidden
PATCH http://localhost:3001/admin/api/v1/models/{{user2_model_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
{
  "alias": "user2-updated-alias"
}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN update models
# -----------------------------------------------------------------------------

# Admin updates a model - should succeed
PATCH http://localhost:3001/admin/api/v1/models/{{user1_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "alias": "user1-model-updated"
}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_model_id}}"
jsonpath "$.alias" == "user1-model-updated"
jsonpath "$.model_name" == "user1-model"

# Admin updates rate limits - should succeed
PATCH http://localhost:3001/admin/api/v1/models/{{user2_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "requests_per_second": 500.0,
  "burst_size": 1000
}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user2_model_id}}"
jsonpath "$.requests_per_second" == 500.0
jsonpath "$.burst_size" == 1000

# -----------------------------------------------------------------------------
# PlatformManager CAN update deleted models (has system access)
# -----------------------------------------------------------------------------

# Admin can update deleted model (has is_admin/system access)
PATCH http://localhost:3001/admin/api/v1/models/{{deleted_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "alias": "deleted-model-updated"
}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{deleted_model_id}}"
jsonpath "$.alias" == "deleted-model-updated"

# =============================================================================
# Model Retrieval Permissions Tests (GET by ID)
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser can only fetch models they have access to
# -----------------------------------------------------------------------------

# User1 fetches their accessible model - should succeed
GET http://localhost:3001/admin/api/v1/models/{{user1_model_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_model_id}}"
jsonpath "$.alias" == "user1-model-updated"
jsonpath "$.model_name" == "user1-model"

# User1 fetches public model - should succeed
GET http://localhost:3001/admin/api/v1/models/{{public_model_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{public_model_id}}"

# User1 tries to fetch user2's model - should get 404
GET http://localhost:3001/admin/api/v1/models/{{user2_model_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 404

# User1 tries to fetch admin-only model - should get 404
GET http://localhost:3001/admin/api/v1/models/{{admin_only_model_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 404

# User1 tries to fetch deleted model - should get 404
GET http://localhost:3001/admin/api/v1/models/{{deleted_model_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 404

# -----------------------------------------------------------------------------
# StandardUser doesn't get enriched fields even when fetching by ID
# -----------------------------------------------------------------------------

# User1 fetches their model with include - should NOT get groups/metrics/rate_limits
GET http://localhost:3001/admin/api/v1/models/{{user1_model_id}}?include=groups,metrics,pricing,status,endpoints
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_model_id}}"
jsonpath "$.groups" not exists
jsonpath "$.metrics" not exists
jsonpath "$.requests_per_second" not exists
jsonpath "$.burst_size" not exists
# Endpoints are public
jsonpath "$.endpoint" exists

# -----------------------------------------------------------------------------
# PlatformManager can fetch ANY model (even without group access)
# -----------------------------------------------------------------------------

# Admin fetches user1's model - should succeed (even though admin not in group)
GET http://localhost:3001/admin/api/v1/models/{{user1_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_model_id}}"
jsonpath "$.alias" == "user1-model-updated"

# Admin fetches user2's model - should succeed
GET http://localhost:3001/admin/api/v1/models/{{user2_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user2_model_id}}"

# Admin fetches admin-only model - should succeed
GET http://localhost:3001/admin/api/v1/models/{{admin_only_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{admin_only_model_id}}"

# -----------------------------------------------------------------------------
# PlatformManager can fetch deleted models with deleted=true
# -----------------------------------------------------------------------------

# Admin fetches deleted model without deleted=true - should get 404
GET http://localhost:3001/admin/api/v1/models/{{deleted_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 404

# Admin fetches deleted model with deleted=true - should succeed
GET http://localhost:3001/admin/api/v1/models/{{deleted_model_id}}?deleted=true
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{deleted_model_id}}"
jsonpath "$.alias" == "deleted-model-updated"

# -----------------------------------------------------------------------------
# PlatformManager gets enriched fields with include parameter
# -----------------------------------------------------------------------------

# Admin fetches with all includes - should get everything
GET http://localhost:3001/admin/api/v1/models/{{user1_model_id}}?include=groups,metrics,pricing,status,endpoints
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_model_id}}"
jsonpath "$.groups" exists
jsonpath "$.metrics" exists
jsonpath "$.status" exists
jsonpath "$.endpoint" exists
# Rate limits visible to admin
jsonpath "$.requests_per_second" exists
jsonpath "$.burst_size" exists

# Admin fetches model with just endpoints (public info) - should work
GET http://localhost:3001/admin/api/v1/models/{{public_model_id}}?include=endpoints
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.endpoint" exists
jsonpath "$.endpoint.name" exists
jsonpath "$.endpoint.url" exists

# =============================================================================
# Pricing Enrichment Permission Tests
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser gets customer pricing only (no downstream pricing)
# -----------------------------------------------------------------------------

# User1 fetches model with pricing - should get pricing but NOT downstream_pricing
GET http://localhost:3001/admin/api/v1/models/{{user1_model_id}}?include=pricing
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_model_id}}"
jsonpath "$.pricing" exists
jsonpath "$.downstream_pricing" not exists

# User1 lists models with pricing - should get pricing but NOT downstream_pricing
GET http://localhost:3001/admin/api/v1/models?include=pricing
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data[0].pricing" exists
jsonpath "$.data[0].downstream_pricing" not exists

# -----------------------------------------------------------------------------
# PlatformManager gets BOTH customer and downstream pricing
# -----------------------------------------------------------------------------

# Admin fetches model with pricing - should get BOTH pricing fields
GET http://localhost:3001/admin/api/v1/models/{{user1_model_id}}?include=pricing
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_model_id}}"
jsonpath "$.pricing" exists
jsonpath "$.downstream_pricing" exists

# Admin lists models with pricing - should get BOTH pricing fields
GET http://localhost:3001/admin/api/v1/models?include=pricing
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data[0].pricing" exists
jsonpath "$.data[0].downstream_pricing" exists

# =============================================================================
# Rate Limits Visibility Confirmation
# =============================================================================

# -----------------------------------------------------------------------------
# Explicit confirmation: PlatformManager CAN see rate limits
# -----------------------------------------------------------------------------

# Admin GET by ID - should see rate limits (has ModelRateLimits::ReadAll)
GET http://localhost:3001/admin/api/v1/models/{{user1_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{user1_model_id}}"
jsonpath "$.requests_per_second" == 100.0
jsonpath "$.burst_size" == 200

# Admin LIST - should see rate limits on all models
GET http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data[0].requests_per_second" exists
jsonpath "$.data[0].burst_size" exists

# =============================================================================
# StandardUser Permissions Tests (LIST)
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser can only see models they have group access to
# -----------------------------------------------------------------------------

# User1 should see: user1_model + public_model (2 models)
GET http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" isCollection
jsonpath "$.data" count == 2
jsonpath "$.data[*].id" contains "{{user1_model_id}}"
jsonpath "$.data[*].id" contains "{{public_model_id}}"
jsonpath "$.data[*].id" not contains "{{user2_model_id}}"
jsonpath "$.data[*].id" not contains "{{admin_only_model_id}}"
jsonpath "$.data[*].id" not contains "{{deleted_model_id}}"

# User2 should see: user2_model + public_model (2 models)
GET http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 2
jsonpath "$.data[*].id" contains "{{user2_model_id}}"
jsonpath "$.data[*].id" contains "{{public_model_id}}"
jsonpath "$.data[*].id" not contains "{{user1_model_id}}"
jsonpath "$.data[*].id" not contains "{{admin_only_model_id}}"

# -----------------------------------------------------------------------------
# StandardUser doesn't get enriched fields even if requested
# -----------------------------------------------------------------------------

# User1 requests all enriched fields - should get none (except endpoints)
GET http://localhost:3001/admin/api/v1/models?include=groups,metrics,pricing,status,endpoints
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data[0].groups" not exists
jsonpath "$.data[0].metrics" not exists
jsonpath "$.data[0].requests_per_second" not exists
jsonpath "$.data[0].burst_size" not exists
# Endpoints are public, user CAN see them
jsonpath "$.data[0].endpoint" exists

# -----------------------------------------------------------------------------
# StandardUser cannot bypass with endpoint filter
# -----------------------------------------------------------------------------

# User1 tries to filter by endpoint - still only sees their accessible models
GET http://localhost:3001/admin/api/v1/models?endpoint={{models_test_endpoint_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 2
jsonpath "$.data[*].id" contains "{{user1_model_id}}"
jsonpath "$.data[*].id" contains "{{public_model_id}}"
jsonpath "$.data[*].id" not contains "{{user2_model_id}}"

# -----------------------------------------------------------------------------
# StandardUser cannot bypass with accessible parameter
# -----------------------------------------------------------------------------

# User1 tries accessible=false - should still be filtered
GET http://localhost:3001/admin/api/v1/models?accessible=false
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 2
jsonpath "$.data[*].id" contains "{{user1_model_id}}"
jsonpath "$.data[*].id" not contains "{{admin_only_model_id}}"

# -----------------------------------------------------------------------------
# StandardUser cannot bypass with search parameter
# -----------------------------------------------------------------------------

# User1 searches for user2's model - should not find it
GET http://localhost:3001/admin/api/v1/models?search=user2-model
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 0

# User1 searches for admin-only model - should not find it
GET http://localhost:3001/admin/api/v1/models?search=admin-only
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 0

# User1 searches for public model - should find it
GET http://localhost:3001/admin/api/v1/models?search=public
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 1
jsonpath "$.data[0].id" == "{{public_model_id}}"

# -----------------------------------------------------------------------------
# StandardUser cannot see deleted models
# -----------------------------------------------------------------------------

# User1 tries to see deleted models - parameter ignored
GET http://localhost:3001/admin/api/v1/models?deleted=true
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data[*].id" not contains "{{deleted_model_id}}"

# -----------------------------------------------------------------------------
# StandardUser gets empty list when no group access
# -----------------------------------------------------------------------------

# Remove user1 from all groups temporarily
DELETE http://localhost:3001/admin/api/v1/groups/{{user1_group_id}}/users/{{user1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

DELETE http://localhost:3001/admin/api/v1/groups/{{everyone_group_id}}/users/{{user1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# User1 should now see empty list (not 403)
GET http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 0
jsonpath "$.total_count" == 0

# Restore user1's access
POST http://localhost:3001/admin/api/v1/groups/{{user1_group_id}}/users/{{user1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

POST http://localhost:3001/admin/api/v1/groups/{{everyone_group_id}}/users/{{user1_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# -----------------------------------------------------------------------------
# StandardUser pagination only counts accessible models
# -----------------------------------------------------------------------------

# User1 with limit=1 should see 1 of their 2 accessible models
GET http://localhost:3001/admin/api/v1/models?limit=1
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 1
jsonpath "$.total_count" == 2

# -----------------------------------------------------------------------------
# StandardUser can see models from ALL their groups
# -----------------------------------------------------------------------------

# User1 is in user1_group and everyone_group, should see models from both
GET http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 2
jsonpath "$.data[?(@.alias == 'user1-model-alias')]" exists
jsonpath "$.data[?(@.alias == 'public-model-alias')]" exists

# -----------------------------------------------------------------------------
# Endpoints are public info - StandardUser CAN see them
# -----------------------------------------------------------------------------

GET http://localhost:3001/admin/api/v1/models?include=endpoints
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data[0].endpoint" exists
jsonpath "$.data[0].endpoint.name" exists
jsonpath "$.data[0].endpoint.url" exists

# =============================================================================
# PlatformManager Permissions Tests
# =============================================================================

# -----------------------------------------------------------------------------
# PlatformManager sees ALL models regardless of groups
# -----------------------------------------------------------------------------

# Admin should see all 4 active models (user1, user2, public, admin-only)
GET http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 4
jsonpath "$.data[*].id" contains "{{user1_model_id}}"
jsonpath "$.data[*].id" contains "{{user2_model_id}}"
jsonpath "$.data[*].id" contains "{{public_model_id}}"
jsonpath "$.data[*].id" contains "{{admin_only_model_id}}"
jsonpath "$.data[*].id" not contains "{{deleted_model_id}}"

# -----------------------------------------------------------------------------
# PlatformManager gets all enriched fields with include
# -----------------------------------------------------------------------------

# Admin requests all enriched fields - should get all of them
GET http://localhost:3001/admin/api/v1/models?include=groups,metrics,pricing,status,endpoints
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data[0].groups" exists
jsonpath "$.data[0].metrics" exists
jsonpath "$.data[0].status" exists
jsonpath "$.data[0].endpoint" exists
# Rate limits visible to admin
jsonpath "$.data[0].requests_per_second" exists
jsonpath "$.data[0].burst_size" exists

# -----------------------------------------------------------------------------
# PlatformManager can see deleted models with deleted=true
# -----------------------------------------------------------------------------

# Admin with deleted=true should see all models including deleted
GET http://localhost:3001/admin/api/v1/models?deleted=true
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 5
jsonpath "$.data[*].id" contains "{{deleted_model_id}}"

# Admin without deleted parameter should NOT see deleted (default behavior)
GET http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data[*].id" not contains "{{deleted_model_id}}"

# -----------------------------------------------------------------------------
# PlatformManager can toggle accessible parameter
# -----------------------------------------------------------------------------

# Admin with accessible=false should see all models (default)
GET http://localhost:3001/admin/api/v1/models?accessible=false
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 4

# Admin with accessible=true should see only models they have group access to
# (admin is not in any groups, so should see only models with no group restriction)
GET http://localhost:3001/admin/api/v1/models?accessible=true
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 0

# Add admin to user1_group
POST http://localhost:3001/admin/api/v1/groups/{{user1_group_id}}/users/{{admin_user_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# Now admin with accessible=true should see user1_model
GET http://localhost:3001/admin/api/v1/models?accessible=true
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count >= 1
jsonpath "$.data[*].id" contains "{{user1_model_id}}"

# Remove admin from group for cleanliness
DELETE http://localhost:3001/admin/api/v1/groups/{{user1_group_id}}/users/{{admin_user_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

# -----------------------------------------------------------------------------
# PlatformManager can search across ALL models
# -----------------------------------------------------------------------------

# Admin searches for user1's model - should find it
GET http://localhost:3001/admin/api/v1/models?search=user1-model
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 1
jsonpath "$.data[0].id" == "{{user1_model_id}}"

# Admin searches for user2's model - should find it
GET http://localhost:3001/admin/api/v1/models?search=user2-model
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 1
jsonpath "$.data[0].id" == "{{user2_model_id}}"

# Admin searches for admin-only model - should find it
GET http://localhost:3001/admin/api/v1/models?search=admin-only
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 1
jsonpath "$.data[0].id" == "{{admin_only_model_id}}"

# Admin searches for deleted model - should find it with deleted=true
GET http://localhost:3001/admin/api/v1/models?search=deleted-model&deleted=true
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.data" count == 1
jsonpath "$.data[0].id" == "{{deleted_model_id}}"

# =============================================================================
# Model Deletion Permissions Tests (DELETE)
# =============================================================================

# -----------------------------------------------------------------------------
# StandardUser CANNOT delete models
# -----------------------------------------------------------------------------

# User1 tries to delete their accessible model - should be forbidden
DELETE http://localhost:3001/admin/api/v1/models/{{user1_model_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User1 tries to delete inaccessible model - should be forbidden
DELETE http://localhost:3001/admin/api/v1/models/{{admin_only_model_id}}
[Cookies]
dwctl_session: {{user_jwt}}
HTTP 403

# User2 tries to delete their model - should be forbidden
DELETE http://localhost:3001/admin/api/v1/models/{{user2_model_id}}
[Cookies]
dwctl_session: {{user2_jwt}}
HTTP 403

# -----------------------------------------------------------------------------
# PlatformManager CAN delete models
# -----------------------------------------------------------------------------

# Create a temporary model to test deletion
POST http://localhost:3001/admin/api/v1/models
[Cookies]
dwctl_session: {{admin_jwt}}
{
  "model_name": "temp-delete-test-model",
  "alias": "temp-delete-alias",
  "hosted_on": "{{models_test_endpoint_id}}",
  "requests_per_second": 100.0,
  "burst_size": 200
}
HTTP 200
[Captures]
temp_delete_model_id: jsonpath "$.id"

# Admin deletes the model - should succeed (soft delete)
DELETE http://localhost:3001/admin/api/v1/models/{{temp_delete_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200

# Verify soft delete: Model not visible without deleted=true
GET http://localhost:3001/admin/api/v1/models/{{temp_delete_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 404

# Verify soft delete: Model IS visible with deleted=true
GET http://localhost:3001/admin/api/v1/models/{{temp_delete_model_id}}?deleted=true
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200
[Asserts]
jsonpath "$.id" == "{{temp_delete_model_id}}"
jsonpath "$.alias" == "temp-delete-alias"

# =============================================================================
# Cleanup
# =============================================================================

DELETE http://localhost:3001/admin/api/v1/models/{{user1_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200

DELETE http://localhost:3001/admin/api/v1/models/{{user2_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200

DELETE http://localhost:3001/admin/api/v1/models/{{public_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200

DELETE http://localhost:3001/admin/api/v1/models/{{admin_only_model_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 200

DELETE http://localhost:3001/admin/api/v1/groups/{{user1_group_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

DELETE http://localhost:3001/admin/api/v1/groups/{{user2_group_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

DELETE http://localhost:3001/admin/api/v1/groups/{{everyone_group_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204

DELETE http://localhost:3001/admin/api/v1/endpoints/{{models_test_endpoint_id}}
[Cookies]
dwctl_session: {{admin_jwt}}
HTTP 204