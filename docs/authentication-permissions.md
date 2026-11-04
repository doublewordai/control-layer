# Authentication and permission boundaries

Platform configuration, customer ownership and the reserved system credential
have separate authorization rules. An account's effective permissions depend on
how the request authenticates and, for API keys, the verified key purpose.

## Request authorization

`CurrentUser::from_request_parts` records the successful authentication method.
API credentials are authoritative when supplied: an invalid or insufficient key
is rejected, never retried as a cookie or proxy identity. Provenance and the
resolved API key ID are internal fields; JSON cannot supply them. Both effective
roles and the shared permission checker enforce these boundaries.

| Authentication | Effective platform authority |
| --- | --- |
| Trusted proxy SSO | Assigned platform roles and the legacy admin flag |
| Platform-purpose API key | Assigned PlatformManager role, plus ordinary capabilities; no inherited legacy admin flag, BillingManager or RequestViewer role |
| Inference-purpose API key | StandardUser and explicitly assigned ordinary capabilities |
| Native session | StandardUser and explicitly assigned ordinary capabilities |
| Reserved system API key | Unrestricted service authority, including customer resources and account administration |

Ordinary capabilities include BatchAPIUser, ConnectionsUser and
BackgroundInferenceUser. Background inference still requires that explicit role.
The key-purpose gate prevents ordinary platform keys from invoking direct
`/ai/*` inference routes and inference keys from invoking management routes.
Platform keys can use the `/admin/api/v1/ai/*` inference proxy. The reserved
system key is also accepted on direct `/ai/*` routes.
`/users/current` and self-profile update responses expose the effective roles and
admin flag, so clients see the same authority the permission checker enforces.

The proxy-header configuration is a trust boundary. Deployments must strip
caller-supplied identity headers, set them only after verified SSO, and prevent
untrusted direct backend access. Requests using API-key bypass paths must not
receive trusted identity headers. The application does not cryptographically
validate upstream identity headers.

## PlatformManager automation

PlatformManager platform keys can manage models, composite routing, endpoints,
pricing, groups and entitlements, probes, system monitoring and credit
transactions. They can inspect account and billing metadata, create new
organizations with an assigned owner, and provision ordinary users.

Only personal platform keys carry the holder's PlatformManager role.
Organization keys keep the ordinary ceiling, because organization managers can
issue them in another member's name and receive the secret.

User creation rejects PlatformManager, BillingManager and RequestViewer role
assignments unless the caller uses the reserved system credential. Platform
managers cannot modify existing users' roles or general account settings, delete
arbitrary accounts, or use provisioning to acquire another account's credentials.

Platform managers authenticated through SSO or a platform key may enable another
account's zero data retention using an enable-only PATCH payload. They cannot
disable it or bundle unrelated settings into that update. They may manage their
own platform-scope webhooks; that does not grant access to another account's
webhooks. PlatformManager does not grant request-log access.

## Customer ownership

Except for the reserved system credential, customer files, batches, stored
responses, connections, API keys and webhooks require ownership or legitimate
organization access, including for SSO admins and PlatformManager keys. Lists
are scoped before pagination. Platform roles do not grant organization membership,
allow staff to issue customer keys, or permit them to add themselves to an
existing organization. Owner/member key-management permissions remain available.

Personal settings and organization-owner operations retain their existing rules.
Platform-wide organization update/delete and user update/delete permissions are
reserved for the system credential. Traffic metadata remains subject to its
endpoint-specific authorization and is not justification for exposing content.

## System credential

The exception requires a verified platform-purpose API key whose key ID, creator
and account are all the reserved nil UUID. A legacy admin flag, PlatformManager
role, matching name/email, native login, SSO identity, or a different key issued
for the system account does not qualify. The system key may use both management
and AI API routes. Authentication still rejects revoked keys and deleted holders.

This credential can provision privileged accounts, issue customer credentials,
change account settings, inspect customer resources and retire accounts. It must
be treated as a fully trusted service credential. The exception does not bypass
input validation or database integrity constraints. Internal process credentials
used by probes and queued inference remain restricted to the AI proxy; they do
not gain access to management endpoints.

## Rollout

Existing PlatformManager platform keys regain the configuration capabilities
listed above. The reserved system key regains service authority. Other API keys
and native sessions retain the ordinary permission ceiling. This change does not
rotate keys, grant stored roles or modify existing organization membership.

Verify platform automation, system provisioning, customer ownership isolation,
credential revocation and trusted SSO header handling before deploying a release.
