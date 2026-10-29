# Authentication and permission boundaries

Management API keys do not inherit platform privileges from their holder. The
same account can have different effective permissions when authenticated with a
key, a native login session, or trusted upstream SSO.

## Request authorization

`CurrentUser::from_request_parts` records the successful authentication method.
API credentials are authoritative when supplied: an invalid key is rejected,
not retried as a cookie or proxy identity. Both the resolved roles and the shared
permission checker enforce the authentication ceiling. Persisted account roles
are unchanged. `/users/current` returns effective roles and the effective admin
flag so dashboard controls follow the request's authority.

Only trusted proxy SSO retains platform roles and the legacy admin flag. API keys
and native sessions retain StandardUser and explicitly assigned ordinary
capabilities such as BatchAPIUser, ConnectionsUser and BackgroundInferenceUser.
Organization permissions still follow actual membership and key-management grants; a platform role supplies no membership override.
Key purpose still determines which API surface accepts the key. In particular,
a platform-purpose key can manage permitted personal resources but cannot grant
credits, inspect arbitrary accounts, or change another account's retention.

The proxy-header authentication configuration is a trust boundary. Deployments
must strip caller-supplied identity headers, set them only after verified SSO,
and prevent untrusted direct access to the backend. A request passing through a
proxy is not itself proof of SSO. API-key bypass paths must leave identity headers
empty. Deployments using forwarded trusted headers must enforce these rules at
the authenticating upstream hop. This code does not cryptographically validate
an upstream identity header and does not replace those deployment controls.

## Customer ownership

Files, batches, stored responses, external connections, API credentials and webhooks have
no platform-wide permission override, including for legacy admins. Lists are
scoped before pagination and direct access is checked against the personal owner
or a membership-validated active organization. Ordinary organization key-manager
rights remain available within the organization. Staff cannot use their platform
role to issue customer credentials or add themselves to an organization.

Organization webhook access follows membership; platform-scope webhooks additionally
require trusted SSO platform authority, even when accessing one's own webhook.

Broad user create/update/delete permissions and organization update/delete overrides
are disabled. Trusted SSO platform managers can still create new organizations and
assign their initial owner; this does not grant access to existing organizations.
Platform role assignment and privileged account provisioning must use a separately
controlled administrative process; they are not support actions.

## Existing dashboard operations

The existing credit transaction endpoint retains its permission gate; trusted
SSO users with the appropriate platform/billing privileges may grant credits.
API keys and native login sessions cannot inherit that privilege.

The existing user and organization PATCH endpoints accept an SSO platform
manager's cross-account update only when it contains `zero_data_retention: true`
and no other changes. Repeating it is harmless. Staff cannot disable another
account's retention protection, change roles, or bundle other edits into it.
Personal-account and organization-owner settings retain their existing rules.
The dashboard sends only the enable-ZDR field in its support view.

Traffic metadata remains a separate policy concern. This change grants no new
cross-account API-key analytics access. Status metadata is not a justification
for returning request or response content.

## Rollout

This is a permission-tightening change for existing credentials, not just newly
issued keys. Before enabling it, inventory automation using platform/admin keys.
There is no legacy or system-key exemption in this policy. Privileged automation
using these credentials will be denied and must move to a separately designed,
explicit service authorization mechanism before rollout; this change adds none. Also review existing
organization memberships and credentials separately: this change does not revoke
independently issued customer keys or undo prior membership grants.

Verify SSO header handling, native-session behavior, support workflows, ordinary
organization access and automation in staging before production rollout. Do not
restore broad customer-content access as a workaround for a failing support or
automation workflow. Existing credentials are not automatically rotated.
