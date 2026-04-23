# Doubleword Dashboard

The web frontend for the Doubleword Control Layer (`dwctl`). It is a
[React 19](https://react.dev/) + [TypeScript](https://www.typescriptlang.org/)
[Vite](https://vitejs.dev/) single-page app that talks to the `dwctl` admin API
at `/admin/api/v1/*` and the AI proxy at `/ai/v1/*`.

Root-level documentation lives in
[`../CLAUDE.md`](../CLAUDE.md) and [`../README.md`](../README.md); this file
focuses on things specific to `dashboard/`.

## Prerequisites

- Node.js 20+
- [`pnpm`](https://pnpm.io/) 10+
- A running backend. Either:
  - a local `dwctl` (see the root README for `cargo run` + `just db-*`
    instructions), **or**
  - demo mode, which mocks the entire API via MSW (see
    [Demo mode](#demo-mode)).

## Quick start

```bash
pnpm install
pnpm dev           # Vite dev server on http://localhost:5173
```

By default the dev server proxies `/admin/api/*` and `/ai/*` to a local
`dwctl` on `http://localhost:8080`. See `vite.config.ts` for the full proxy
config; override via `DWCTL_URL` in a `.env.local` if your backend runs
elsewhere.

## Scripts

| Script                | What it does                                                               |
| --------------------- | -------------------------------------------------------------------------- |
| `pnpm dev`            | Start Vite in dev mode with HMR.                                           |
| `pnpm build`          | Type-check (`tsc -b tsconfig.app.json`) then build production assets.      |
| `pnpm preview`        | Serve the production build locally.                                        |
| `pnpm lint`           | Run the full ESLint config.                                                |
| `pnpm test`           | Run Vitest in watch mode.                                                  |
| `pnpm test run`       | Run Vitest once (what CI does).                                            |
| `pnpm test:e2e`       | Run the Playwright E2E suite (see `e2e/`).                                 |
| `pnpm test:e2e:ui`    | Open Playwright in UI mode.                                                |
| `pnpm test:e2e:headed`| Run Playwright with a visible browser.                                     |

The top-level `just` recipes wrap these for CI parity:

```bash
just lint ts        # eslint
just lint ts --fix  # eslint --fix
just test ts        # pnpm test run
just ci ts          # lint + test + build
```

## Project layout

```
src/
├── api/control-layer/     # typed API client, React Query hooks, MSW mocks
├── components/
│   ├── features/          # route-level screens (models, batches, usage, ...)
│   ├── layout/            # shell, sidebar, topbar
│   ├── modals/            # dialog / drawer components
│   ├── common/            # shared cross-feature components
│   └── ui/                # primitive components (wrappers around Radix)
├── contexts/              # auth, organization, and settings context providers
├── hooks/                 # reusable hooks (debounce, pagination, ...)
├── lib/                   # integrations (telemetry, ...)
├── utils/                 # pure helpers (formatters, authorization, money, ...)
├── App.tsx                # route table (lazy-loaded via `lazyWithRetry`)
└── main.tsx               # entry point
```

### API layer

`src/api/control-layer/` is the single source of truth for backend access:

- `types.ts` — generated-style TypeScript interfaces mirroring the `dwctl`
  admin API.
- `client.ts` — a thin fetch wrapper with auth handling and error mapping.
- `hooks.ts` — React Query hooks (`useModels`, `useGroups`, `useBatches`, ...).
  Always add new features through a hook here so caching, invalidation, and
  demo-mode mocking stay consistent.
- `mocks/` — MSW handlers + JSON fixtures used for demo mode and most tests.

### Features

Each top-level route lives under `src/components/features/<feature>/`. Features
are lazy-loaded from `App.tsx` so each becomes its own chunk. A feature
typically looks like:

```
features/<feature>/
├── index.ts               # public barrel
├── <Feature>.tsx          # main screen
├── <Feature>.test.tsx     # integration test (render + MSW)
└── sub-components / hooks specific to this feature
```

## Authentication & roles

Auth state is held in `contexts/auth/`; permissions are checked via
`useAuthorization()` (`src/utils/authorization.ts`). The role model is
additive:

- **StandardUser** — baseline; all signed-in users have it.
- **PlatformManager** — admin surfaces (users, groups, models, settings).
- **RequestViewer** — request logs and analytics.

Always gate admin UI with `hasPermission(...)` rather than inspecting roles
directly.

## Demo mode

Demo mode swaps the live API for [MSW](https://mswjs.io/) handlers so you can
explore the product without a backend. Three ways to enable it:

1. `?flags=demo` query parameter (e.g. `http://localhost:5173/?flags=demo`).
2. Settings page toggle (persisted to `localStorage` under `app-settings`).
3. Directly set `features.demo: true` on the `app-settings` key in
   `localStorage`.

Mock data lives in `src/api/control-layer/mocks/` (`models.json`, `users.json`,
etc.). `demoState.ts` owns stateful operations (adding a user to a group,
etc.) so those interactions survive a page refresh.

Feature flags today:

| Flag          | Purpose                                  |
| ------------- | ---------------------------------------- |
| `demo`        | Enable demo mode with mock data.         |
| `use_billing` | Enable billing / cost management UI.     |

Flags compose: `?flags=demo,use_billing`.

## Styling & design

- [Tailwind CSS 4](https://tailwindcss.com/) with the `doubleword-*` color
  palette defined in `tailwind.config.js`.
- Space Grotesk type family.
- Primitive components (`components/ui/`) are thin wrappers around
  [Radix UI](https://www.radix-ui.com/); prefer composing these over adding
  new one-off components.
- Keep interactive color signals subtle (the codebase leans on layout and
  hierarchy, not vibrant fills).
- Animations should be user-triggered and subtle (`transition-colors`,
  `transition-transform`). No autoplay.
- Always keep mobile responsiveness in mind; tables default to horizontal
  scroll with a sticky header row.

## Testing conventions

We use [Vitest](https://vitest.dev/) + [Testing Library](https://testing-library.com/)
and MSW for HTTP mocking.

- Co-locate tests: `Component.tsx` ↔ `Component.test.tsx`.
- Scope queries to the component under test: destructure `{ container }` from
  `render(...)` and use `within(container).getByRole(...)` rather than
  `screen.*`. The exception is portal-rendered content (modals, dropdown
  menus, popovers) — those live at the document root and must be queried via
  `screen`.
- Select elements by accessible roles / labels, not CSS classes or tag names.
- Prefer `waitFor`/`findBy` over `tokio`-style `sleep`; assert the pre-state,
  trigger the change, then assert the post-state so you test the whole flow.

Fast feedback loop for a single file:

```bash
pnpm test run path/to/Component.test.tsx
```

Playwright tests live in `e2e/` and currently cover a small set of smoke
flows; unit / integration tests are the primary safety net.

## Contributing

Before pushing anything to a PR:

```bash
pnpm lint
pnpm test run
pnpm build
```

The build step also type-checks via `tsc -b`, which catches things ESLint
won't. CI runs `just ci ts` which is equivalent.

For larger patterns (repository conventions, role model, request flow,
pooling, database), see [`../CLAUDE.md`](../CLAUDE.md) — it is the canonical
reference and is kept in sync with reality.
