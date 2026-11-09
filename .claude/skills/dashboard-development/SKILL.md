---
name: dashboard-development
description: Use when adding or changing dashboard pages, API hooks, cached queries, organization switching, demo data, or React component tests in control-layer.
---

# Dashboard development

Build on the existing API and UI layers. The dashboard README is a Vite starter
template, not the repository's application architecture.

## Implement a feature through the layers

1. Match the backend contract in `dashboard/src/api/control-layer/types.ts`.
   Add request construction and error handling to `client.ts`; use its existing
   management/AI URL and credential helpers rather than hardcoding a deployment.
2. Add cache keys to `keys.ts` and queries/mutations to `hooks.ts`. Include filters,
   pagination, and relevant ownership context in cache identity. Invalidate both
   the changed resource and dependent lists/counts after mutations.
3. Use the existing `components/ui/` primitives and nearby feature components.
   Keep loading, empty, error, and permission-denied states explicit. Use the
   established typography/colors, accessible labels, and responsive layouts.
4. Update `api/control-layer/mocks/handlers.ts`, fixture data, and `demoState.ts`
   when the feature participates in demo mode. Keep mock response shapes and
   mutation behavior aligned with the real API.

## Organization context

The current `OrganizationProvider` derives active context from the server's
current-user response and changes it through `organizations.setActive()`. Extend
its query invalidation when adding another context-sensitive resource. Do not
copy the historical organization plan's localStorage/header implementation.

For example, switching from an organization to a personal account must not leave
the previous account's list or balance visible as the new account's data. Test
switching alongside pagination/filter changes and mutations. UI visibility does
not replace backend authorization.

## Tests and common mistakes

- Use Vitest/React Testing Library, existing query-client providers, and API
  mocks. Test user-visible behavior and cache invalidation, not component internals.
- Query the render's `container` with `within(container)` and roles/labels.
  Use `screen` for portals such as dialogs, menus, and popovers outside it.
- Cover loading, empty/error responses, mutation success/failure, and context
  changes where relevant. Do not assume a working demo proves real API access.
- Run `just lint ts` and `just test ts` for TypeScript changes. Check the build
  when changing contracts, imports, or bundling.

## Sources and examples

- [Repository conventions](../../../CLAUDE.md), especially demo mode and React tests.
- [API layer](../../../dashboard/src/api/control-layer/) and its `__tests__/`.
- [Organization context](../../../dashboard/src/contexts/organization/OrganizationContext.tsx).
- [Shared hooks](../../../dashboard/src/hooks/) for pagination and persisted filters.
