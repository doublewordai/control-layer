---
name: admin-api-authorization
description: Use when adding management API endpoints, changing resource ownership, organization permissions, API-key capabilities, or authorization tests in control-layer.
---

# Management API and authorization

Authentication establishes the actor. Each operation must separately authorize
access to the target resource. Organization context is a selection, not a grant.

## Implement the complete API contract

Follow nearby handlers under `dwctl/src/api/handlers/`: typed API models,
`PoolProvider`-generic state, `#[tracing::instrument(skip_all)]`, repository access,
and the existing `Error` variants. Register routes and utoipa/OpenAPI schemas;
update dashboard contracts/mocks when their API changes. Use
[sqlx-queries](../sqlx-queries/SKILL.md) for database operations and consistency.

## Keep actor and owner separate

- In API-key authentication, `CurrentUser.id` identifies the acting creator;
  `api_keys.user_id` identifies the resource/billing owner, which may be an
  organization. Preserve both in attribution and authorization.
- Use the operation-specific own/all permission helpers and appropriate active
  organization membership/management checks in `auth/permissions.rs`. Do not
  turn membership into universal write permission or trust an arbitrary target ID.
  Use primary-pool authorization reads where revocation must take effect immediately.
- Roles are additive. PlatformManager does not imply permission to see private
  request data; consult the actual resource/action rules rather than `is_admin`.
- Key management also uses `resolve_key_capabilities`. The additive `manage_keys`
  role affects creation/edit/deletion rights; an issued-key holder may be allowed
  to rotate a key without being allowed to delete or change its limits. Capability
  resolution does not replace target-access checks or the caller's all-resource
  permission checks.
- Preserve organization last-owner and owner-assignment guards. Pending or revoked
  membership must not behave like active membership.

For example, a member rotating an issued organization key must remain attributed
to the human actor, must pass that operation's access checks, and must not acquire
permission to create or delete keys merely because rotation succeeded.

## Verification

Use `#[sqlx::test]` and the existing API test helpers. Cover own resources, another
user's resources, active/pending/non-member organization access, relevant roles,
and revoked privileges. Include key capability differences and last-owner races
when those paths change. Verify the real route/status/response and OpenAPI contract.
Run `just lint rust` and `just test rust` for Rust changes.

## Sources

- [Actor extraction](../../../dwctl/src/auth/current_user.rs) and
  [permission/capability helpers](../../../dwctl/src/auth/permissions.rs).
- [API-key handlers](../../../dwctl/src/api/handlers/api_keys.rs) and
  [organization handlers](../../../dwctl/src/api/handlers/organizations.rs).
- [Users/groups guide](../../../docs/src/how-to/users-and-groups.md) for user flows.
  [Organization design](../../../docs/organizations.md) is historical context;
  it predates current capability and membership behavior. Follow the code above.
