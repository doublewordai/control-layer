# Workloads & Async UI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restructure Batches into a Workloads section with Batch (decluttered) and Async (flat request list) sub-pages.

**Architecture:** Frontend-heavy feature with two new backend endpoints. The Async page queries fusillade's request tables directly via SQLx (cross-batch request listing). Frontend uses existing patterns: DataTable, React Query hooks, container/presenter, Tabs from Radix UI. Sidebar gets a collapsible Workloads section header.

**Tech Stack:** Rust/axum (backend), React/TypeScript/TanStack Query/Tailwind (frontend), fusillade schema via SQLx (database)

**Spec:** `docs/superpowers/specs/2026-04-14-workloads-async-ui-design.md`

---

### Task 1: Frontend — Routing & Sidebar Navigation

**Files:**
- Modify: `dashboard/src/App.tsx` (routes)
- Modify: `dashboard/src/components/layout/Sidebar/AppSidebar.tsx` (sidebar nav)

- [ ] **Step 1: Add new route imports in App.tsx**

Add lazy imports for the new Async components alongside existing Batches imports:

```tsx
const AsyncRequests = lazy(() =>
  import("./components/features/async-requests").then((m) => ({
    default: m.AsyncRequests,
  })),
);

const AsyncRequestDetail = lazy(() =>
  import("./components/features/async-requests").then((m) => ({
    default: m.AsyncRequestDetail,
  })),
);
```

- [ ] **Step 2: Add workloads routes and redirects in App.tsx**

Add routes for `/workloads/async`, `/workloads/async/:requestId`, `/workloads/batch`, and redirects from `/batches`:

```tsx
{/* Workloads - Async */}
<Route
  path="/workloads/async"
  element={
    <AppLayout>
      <ProtectedRoute path="/batches">
        <Suspense fallback={<RouteLoader />}>
          <AsyncRequests />
        </Suspense>
      </ProtectedRoute>
    </AppLayout>
  }
/>
<Route
  path="/workloads/async/:requestId"
  element={
    <AppLayout>
      <ProtectedRoute path="/batches">
        <Suspense fallback={<RouteLoader />}>
          <AsyncRequestDetail />
        </Suspense>
      </ProtectedRoute>
    </AppLayout>
  }
/>

{/* Workloads - Batch (move existing /batches routes) */}
<Route
  path="/workloads/batch"
  element={
    <AppLayout>
      <ProtectedRoute path="/batches">
        <Suspense fallback={<RouteLoader />}>
          <Batches />
        </Suspense>
      </ProtectedRoute>
    </AppLayout>
  }
/>
<Route
  path="/workloads/batch/:batchId"
  element={
    <AppLayout>
      <ProtectedRoute path="/batches">
        <Suspense fallback={<RouteLoader />}>
          <BatchInfo />
        </Suspense>
      </ProtectedRoute>
    </AppLayout>
  }
/>

{/* Redirects from old /batches paths */}
<Route path="/batches" element={<Navigate to="/workloads/batch" replace />} />
<Route path="/batches/:batchId" element={<Navigate to={`/workloads/batch/${/* use useParams */}`} replace />} />
```

Note: For the parameterized redirect `/batches/:batchId`, create a small redirect component:

```tsx
function BatchRedirect() {
  const { batchId } = useParams();
  return <Navigate to={`/workloads/batch/${batchId}`} replace />;
}
```

- [ ] **Step 3: Update sidebar in AppSidebar.tsx**

Replace the single "Batches" nav item with a collapsible "Workloads" section. Use the existing `Collapsible` pattern from the Models section (lines 160-247 of AppSidebar.tsx):

```tsx
<Collapsible key="workloads" defaultOpen className="group/collapsible">
  <SidebarGroup className="p-0">
    <SidebarGroupLabel asChild>
      <CollapsibleTrigger className="flex w-full items-center justify-between">
        Workloads
        <ChevronRight className="ml-auto h-4 w-4 transition-transform group-data-[state=open]/collapsible:rotate-90" />
      </CollapsibleTrigger>
    </SidebarGroupLabel>
    <CollapsibleContent>
      <SidebarMenu>
        <SidebarMenuItem>
          <SidebarMenuButton asChild isActive={pathname.startsWith("/workloads/async")}>
            <NavLink to="/workloads/async">
              <Zap className="h-4 w-4" />
              <span>Async</span>
            </NavLink>
          </SidebarMenuButton>
        </SidebarMenuItem>
        <SidebarMenuItem>
          <SidebarMenuButton asChild isActive={pathname.startsWith("/workloads/batch")}>
            <NavLink to="/workloads/batch">
              <Box className="h-4 w-4" />
              <span>Batch</span>
            </NavLink>
          </SidebarMenuButton>
        </SidebarMenuItem>
      </SidebarMenu>
    </CollapsibleContent>
  </SidebarGroup>
</Collapsible>
```

Remove the old Batches nav item from the `navItems` array. Keep the same `config.batches?.enabled` visibility gate on the whole Workloads section.

- [ ] **Step 4: Create placeholder AsyncRequests component**

Create `dashboard/src/components/features/async-requests/index.ts`:

```tsx
export { AsyncRequests } from "./AsyncRequests";
export { AsyncRequestDetail } from "./AsyncRequestDetail";
```

Create `dashboard/src/components/features/async-requests/AsyncRequests.tsx`:

```tsx
export function AsyncRequests() {
  return (
    <div className="p-6">
      <h1 className="text-3xl font-bold">Async</h1>
      <p className="text-neutral-600 mt-1">Coming soon</p>
    </div>
  );
}
```

Create `dashboard/src/components/features/async-requests/AsyncRequestDetail.tsx`:

```tsx
export function AsyncRequestDetail() {
  return (
    <div className="p-6">
      <h1 className="text-3xl font-bold">Request Detail</h1>
      <p className="text-neutral-600 mt-1">Coming soon</p>
    </div>
  );
}
```

- [ ] **Step 5: Update internal links**

Update any links pointing to `/batches` within the dashboard to use `/workloads/batch` instead. Key files to check:
- `dashboard/src/components/features/batches/BatchInfo/BatchInfo.tsx` (back button)
- Any `useNavigate` calls that navigate to `/batches`

Run: `grep -r '"/batches' dashboard/src/ --include='*.tsx' --include='*.ts' -l` to find all references.

- [ ] **Step 6: Verify routing works**

Run: `cd dashboard && pnpm run dev`

Verify:
- `/workloads/async` shows placeholder
- `/workloads/batch` shows existing batch list
- `/batches` redirects to `/workloads/batch`
- Sidebar shows Workloads section with Async and Batch items
- Collapsible works

- [ ] **Step 7: Run lints and tests**

Run: `just lint ts` and `just test ts`
Fix any issues.

- [ ] **Step 8: Commit**

```bash
git add dashboard/src/
git commit -m "feat: add workloads section with async/batch routing and sidebar navigation"
```

---

### Task 2: Frontend — Batch Page Declutter

**Files:**
- Modify: `dashboard/src/components/features/batches/BatchesTable/columns.tsx`
- Modify: `dashboard/src/components/features/batches/Batches/Batches.tsx`

- [ ] **Step 1: Add Type and Completion Window columns to columns.tsx**

Add a Type column that derives from `completion_window`:

```tsx
{
  id: "type",
  header: "Type",
  cell: ({ row }) => {
    const completionWindow = row.original.completion_window;
    const isAsync = completionWindow === "1h";
    return (
      <span
        className={cn(
          "inline-flex items-center rounded px-2 py-0.5 text-xs font-medium",
          isAsync
            ? "bg-yellow-500/10 text-yellow-500"
            : "bg-indigo-500/10 text-indigo-400",
        )}
      >
        {isAsync ? "async" : "batch"}
      </span>
    );
  },
},
```

Add a Completion Window column:

```tsx
{
  accessorKey: "completion_window",
  header: "Completion Window",
  cell: ({ row }) => {
    const value = row.getValue("completion_window") as string;
    return <span className="text-gray-500">{value}</span>;
  },
},
```

- [ ] **Step 2: Remove old columns**

Remove these columns from the `createBatchColumns` function:
- Input File ID column
- Source column (`showSourceColumn` conditional)
- Priority column (was showing `completion_window` as "Priority")
- Batch ID column
- Context column (`showContextColumn` conditional)

Keep: Created, User (conditional), the new Type, Completion Window, Status, Progress, Duration, Action columns.

- [ ] **Step 3: Update Batches.tsx**

Remove props/state related to removed columns (`showSourceColumn`, `showContextColumn`). Update the column creation call to match the new signature. Update the page title from "Batch Requests" to "Batches". Update the navigate path from `/batches/${id}` to `/workloads/batch/${id}`.

- [ ] **Step 4: Run lints and tests**

Run: `just lint ts` and `just test ts`
Fix any failing tests due to column changes.

- [ ] **Step 5: Commit**

```bash
git add dashboard/src/components/features/batches/
git commit -m "feat: declutter batch table with Type and Completion Window columns"
```

---

### Task 3: Backend — Batch Requests Endpoints

**Files:**
- Create: `dwctl/src/api/handlers/batch_requests.rs`
- Create: `dwctl/src/api/models/batch_requests.rs`
- Modify: `dwctl/src/api/handlers/mod.rs` (add module)
- Modify: `dwctl/src/api/models/mod.rs` (add module)
- Modify: `dwctl/src/lib.rs` (register routes)

- [ ] **Step 1: Create API models**

Create `dwctl/src/api/models/batch_requests.rs`:

```rust
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::pagination::Pagination;

/// Query parameters for listing batch requests
#[derive(Debug, Default, Deserialize, IntoParams, ToSchema)]
pub struct ListBatchRequestsQuery {
    #[serde(flatten)]
    #[param(inline)]
    pub pagination: Pagination,

    /// Filter by batch completion window (e.g., "1h", "24h")
    pub completion_window: Option<String>,

    /// Filter by request status (pending, processing, completed, failed, canceled)
    pub status: Option<String>,

    /// Filter by model
    pub model: Option<String>,

    /// Sort active requests first (default: true)
    #[serde(default = "default_true")]
    pub active_first: bool,
}

fn default_true() -> bool {
    true
}

/// Individual batch request summary
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BatchRequestResponse {
    pub id: Uuid,
    pub batch_id: Uuid,
    pub model: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_status: Option<i16>,
}

/// Full batch request detail including input/output
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BatchRequestDetailResponse {
    #[serde(flatten)]
    pub summary: BatchRequestResponse,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub completion_window: String,
    pub batch_created_by: String,
}
```

- [ ] **Step 2: Register the models module**

Add `pub mod batch_requests;` to `dwctl/src/api/models/mod.rs`.

- [ ] **Step 3: Create the handler**

Create `dwctl/src/api/handlers/batch_requests.rs`:

```rust
//! Batch request handlers
//!
//! Endpoints for listing individual requests across fusillade batches.
//! These query the fusillade schema directly for cross-batch request listing.

use axum::{
    extract::{Path, Query, State},
    response::Json,
};
use sqlx_pool_router::PoolProvider;
use uuid::Uuid;

use crate::{
    AppState,
    api::models::{
        batch_requests::{BatchRequestDetailResponse, BatchRequestResponse, ListBatchRequestsQuery},
        pagination::PaginatedResponse,
        users::CurrentUser,
    },
    auth::permissions::{RequiresPermission, operation, resource},
    errors::Error,
};

/// List individual batch requests across all batches
#[utoipa::path(
    get,
    path = "/admin/api/v1/batches/requests",
    params(ListBatchRequestsQuery),
    responses(
        (status = 200, description = "Paginated list of batch requests", body = PaginatedResponse<BatchRequestResponse>),
        (status = 403, description = "Insufficient permissions"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "batch_requests",
)]
#[tracing::instrument(skip_all)]
pub async fn list_batch_requests<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Query(query): Query<ListBatchRequestsQuery>,
    current_user: CurrentUser,
    _: RequiresPermission<resource::Batches, operation::ReadOwn>,
) -> Result<Json<PaginatedResponse<BatchRequestResponse>>> {
    let skip = query.pagination.skip();
    let limit = query.pagination.limit();
    let can_read_all = crate::auth::permissions::can_read_all_resources(
        &current_user,
        crate::auth::permissions::Resource::Batches,
    );

    // Build the SQL query against fusillade schema
    // StandardUser sees only own requests, PlatformManager sees all
    let mut sql = String::from(
        r#"
        SELECT
            r.id,
            r.batch_id,
            r.model,
            r.state as status,
            r.created_at,
            r.completed_at,
            r.failed_at,
            r.response_status,
            CASE
                WHEN r.completed_at IS NOT NULL AND r.started_at IS NOT NULL
                THEN EXTRACT(EPOCH FROM (r.completed_at - r.started_at)) * 1000
                ELSE NULL
            END as duration_ms
        FROM fusillade.requests r
        JOIN fusillade.batches b ON r.batch_id = b.id
        WHERE b.deleted_at IS NULL
        "#,
    );

    let mut count_sql = String::from(
        r#"
        SELECT COUNT(*) as count
        FROM fusillade.requests r
        JOIN fusillade.batches b ON r.batch_id = b.id
        WHERE b.deleted_at IS NULL
        "#,
    );

    // Apply filters dynamically based on query params
    // The actual implementation will use sqlx query builder or parameterized queries
    // This is the general pattern — exact SQL will depend on fusillade schema details

    let pool = state.db.read();
    let requests: Vec<BatchRequestResponse> = sqlx::query_as(/* built SQL */)
        .fetch_all(pool)
        .await
        .map_err(|e| Error::Database(e.into()))?;

    let total_count: i64 = sqlx::query_scalar(/* count SQL */)
        .fetch_one(pool)
        .await
        .map_err(|e| Error::Database(e.into()))?;

    Ok(Json(PaginatedResponse::new(requests, total_count, skip, limit)))
}

/// Get individual batch request detail
#[utoipa::path(
    get,
    path = "/admin/api/v1/batches/requests/{request_id}",
    params(
        ("request_id" = Uuid, Path, description = "The request ID"),
    ),
    responses(
        (status = 200, description = "Batch request detail", body = BatchRequestDetailResponse),
        (status = 404, description = "Request not found"),
        (status = 403, description = "Insufficient permissions"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "batch_requests",
)]
#[tracing::instrument(skip_all)]
pub async fn get_batch_request<P: PoolProvider>(
    State(state): State<AppState<P>>,
    Path(request_id): Path<Uuid>,
    current_user: CurrentUser,
    _: RequiresPermission<resource::Batches, operation::ReadOwn>,
) -> Result<Json<BatchRequestDetailResponse>> {
    let pool = state.db.read();

    let request = sqlx::query_as!(
        BatchRequestDetailResponse,
        r#"
        SELECT
            r.id,
            r.batch_id,
            r.model,
            r.state as status,
            r.created_at,
            r.completed_at,
            r.failed_at,
            r.response_status,
            CASE
                WHEN r.completed_at IS NOT NULL AND r.started_at IS NOT NULL
                THEN EXTRACT(EPOCH FROM (r.completed_at - r.started_at)) * 1000
                ELSE NULL
            END as duration_ms,
            r.body,
            r.response_body,
            r.error,
            b.completion_window,
            b.created_by as batch_created_by
        FROM fusillade.requests r
        JOIN fusillade.batches b ON r.batch_id = b.id
        WHERE r.id = $1 AND b.deleted_at IS NULL
        "#,
        request_id
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| Error::Database(e.into()))?
    .ok_or_else(|| Error::NotFound {
        resource: "BatchRequest".to_string(),
        id: request_id.to_string(),
    })?;

    // Check ownership
    let can_read_all = crate::auth::permissions::can_read_all_resources(
        &current_user,
        crate::auth::permissions::Resource::Batches,
    );
    if !can_read_all {
        // Verify the batch belongs to this user via created_by metadata
        let is_owner = crate::api::handlers::batches::is_batch_owner(
            &current_user,
            &request.batch_created_by,
        );
        if !is_owner {
            return Err(Error::NotFound {
                resource: "BatchRequest".to_string(),
                id: request_id.to_string(),
            });
        }
    }

    Ok(Json(request))
}
```

Note: The exact SQL will need to be refined once we verify the fusillade schema column names at implementation time. The handler above shows the pattern — the implementer should check `fusillade.requests` and `fusillade.batches` table schemas and adjust column names accordingly.

- [ ] **Step 4: Register the handler module**

Add `pub mod batch_requests;` to `dwctl/src/api/handlers/mod.rs`.

- [ ] **Step 5: Register routes in lib.rs**

Add the two new routes to the admin API router in `lib.rs`, near where other admin routes are defined:

```rust
.route("/batches/requests", get(api::handlers::batch_requests::list_batch_requests))
.route("/batches/requests/{request_id}", get(api::handlers::batch_requests::get_batch_request))
```

- [ ] **Step 6: Run `cargo check`**

Run: `cargo check`
Fix compilation errors. Key things to watch:
- SQLx compile-time verification requires database running
- Column name mismatches with fusillade schema
- Import paths

- [ ] **Step 7: Write tests**

Add tests in `dwctl/src/api/handlers/batch_requests.rs`:

```rust
#[cfg(test)]
mod tests {
    use crate::{api::models::users::Role, test::utils::*};
    use sqlx::PgPool;

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_batch_requests_unauthorized(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user(&pool, Role::StandardUser).await;

        let response = app
            .get("/admin/api/v1/batches/requests")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        // StandardUser with BatchAPIUser should be able to see own batch requests
        // but StandardUser without it should not
        response.assert_status_forbidden();
    }

    #[sqlx::test]
    #[test_log::test]
    async fn test_list_batch_requests_empty(pool: PgPool) {
        let (app, _bg_services) = create_test_app(pool.clone(), false).await;
        let user = create_test_user_with_roles(
            &pool,
            vec![Role::StandardUser, Role::BatchAPIUser],
        ).await;

        let response = app
            .get("/admin/api/v1/batches/requests")
            .add_header(&add_auth_headers(&user)[0].0, &add_auth_headers(&user)[0].1)
            .add_header(&add_auth_headers(&user)[1].0, &add_auth_headers(&user)[1].1)
            .await;

        response.assert_status_ok();
    }
}
```

- [ ] **Step 8: Run tests**

Run: `just test rust`
Fix any failures.

- [ ] **Step 9: Run lints**

Run: `just lint rust`

- [ ] **Step 10: Commit**

```bash
git add dwctl/src/api/handlers/batch_requests.rs dwctl/src/api/models/batch_requests.rs dwctl/src/api/handlers/mod.rs dwctl/src/api/models/mod.rs dwctl/src/lib.rs
git commit -m "feat: add GET /admin/api/v1/batches/requests endpoints for cross-batch request listing"
```

---

### Task 4: Frontend — API Client & Hooks for Batch Requests

**Files:**
- Modify: `dashboard/src/api/control-layer/types.ts`
- Modify: `dashboard/src/api/control-layer/client.ts`
- Modify: `dashboard/src/api/control-layer/hooks.ts`

- [ ] **Step 1: Add types**

Add to `types.ts`:

```tsx
export interface BatchRequest {
  id: string;
  batch_id: string;
  model: string;
  status: "pending" | "claimed" | "processing" | "completed" | "failed" | "canceled";
  created_at: string;
  completed_at: string | null;
  failed_at: string | null;
  duration_ms: number | null;
  response_status: number | null;
}

export interface BatchRequestDetail extends BatchRequest {
  body: string;
  response_body: string | null;
  error: string | null;
  completion_window: string;
  batch_created_by: string;
}

export interface BatchRequestsListQuery {
  skip?: number;
  limit?: number;
  completion_window?: string;
  status?: string;
  model?: string;
  active_first?: boolean;
}

export interface PaginatedResponse<T> {
  data: T[];
  total_count: number;
  skip: number;
  limit: number;
}
```

- [ ] **Step 2: Add API client methods**

Add to `client.ts` in the `dwctlApi` object:

```tsx
batchRequests: {
  async list(options?: BatchRequestsListQuery): Promise<PaginatedResponse<BatchRequest>> {
    const params = new URLSearchParams();
    if (options?.skip) params.set("skip", options.skip.toString());
    if (options?.limit) params.set("limit", options.limit.toString());
    if (options?.completion_window) params.set("completion_window", options.completion_window);
    if (options?.status) params.set("status", options.status);
    if (options?.model) params.set("model", options.model);
    if (options?.active_first !== undefined) params.set("active_first", options.active_first.toString());

    const response = await fetch(
      `/admin/api/v1/batches/requests${params.toString() ? "?" + params.toString() : ""}`,
    );
    if (!response.ok) {
      throw new Error(`Failed to fetch batch requests: ${response.status}`);
    }
    return response.json();
  },

  async get(id: string): Promise<BatchRequestDetail> {
    const response = await fetch(`/admin/api/v1/batches/requests/${id}`);
    if (!response.ok) {
      throw new Error(`Failed to fetch batch request: ${response.status}`);
    }
    return response.json();
  },
},
```

- [ ] **Step 3: Add React Query hooks**

Add to `hooks.ts`:

```tsx
export function useBatchRequests(options?: BatchRequestsListQuery & { enabled?: boolean }) {
  const { enabled, ...queryOptions } = options || {};

  return useQuery({
    queryKey: ["batchRequests", queryOptions],
    queryFn: () => dwctlApi.batchRequests.list(queryOptions),
    enabled,
    refetchOnMount: "always",
    refetchInterval: (query) => {
      const requests = query.state.data?.data;
      if (requests?.some((r) =>
        ["pending", "claimed", "processing"].includes(r.status)
      )) {
        return 2000;
      }
      return false;
    },
  });
}

export function useBatchRequest(id: string | undefined) {
  return useQuery({
    queryKey: ["batchRequests", id],
    queryFn: () => dwctlApi.batchRequests.get(id!),
    enabled: !!id,
    refetchOnMount: "always",
    refetchInterval: (query) => {
      const request = query.state.data;
      if (request && ["pending", "claimed", "processing"].includes(request.status)) {
        return 2000;
      }
      return false;
    },
  });
}
```

- [ ] **Step 4: Run lints and tests**

Run: `just lint ts` and `just test ts`

- [ ] **Step 5: Commit**

```bash
git add dashboard/src/api/control-layer/
git commit -m "feat: add API client and hooks for batch requests endpoints"
```

---

### Task 5: Frontend — Async Page (Request List)

**Files:**
- Modify: `dashboard/src/components/features/async-requests/AsyncRequests.tsx`

- [ ] **Step 1: Build the AsyncRequests component**

Replace the placeholder with the full implementation:

```tsx
import { useNavigate } from "react-router-dom";
import { Zap, Code } from "lucide-react";
import { Button } from "../../../ui/button";
import { DataTable } from "../../../ui/data-table";
import { useBatchRequests } from "../../../../api/control-layer/hooks";
import type { BatchRequest } from "../../../../api/control-layer/types";
import { useState } from "react";
import { CreateAsyncModal } from "../../../modals/CreateAsyncModal/CreateAsyncModal";
import { type ColumnDef } from "@tanstack/react-table";
import { cn } from "../../../../lib/utils";

const columns: ColumnDef<BatchRequest>[] = [
  {
    accessorKey: "created_at",
    header: "Created",
    cell: ({ row }) => {
      const timestamp = row.getValue("created_at") as string;
      return (
        <span className="text-gray-500">
          {new Date(timestamp).toLocaleString(undefined, {
            month: "short",
            day: "numeric",
            hour: "numeric",
            minute: "2-digit",
          })}
        </span>
      );
    },
  },
  {
    accessorKey: "model",
    header: "Model",
    cell: ({ row }) => <span>{row.getValue("model")}</span>,
  },
  {
    accessorKey: "status",
    header: "Status",
    cell: ({ row }) => {
      const status = row.getValue("status") as string;
      const styles: Record<string, string> = {
        completed: "bg-green-500/10 text-green-400",
        failed: "bg-red-500/10 text-red-400",
        processing: "bg-blue-500/10 text-blue-400",
        claimed: "bg-blue-500/10 text-blue-400",
        pending: "bg-yellow-500/10 text-yellow-400",
        canceled: "bg-gray-500/10 text-gray-400",
      };
      const labels: Record<string, string> = {
        processing: "running",
        claimed: "running",
        pending: "queued",
        canceled: "cancelled",
      };
      return (
        <span className={cn("inline-flex items-center rounded px-2 py-0.5 text-xs font-medium", styles[status] || "bg-gray-500/10 text-gray-400")}>
          {labels[status] || status}
        </span>
      );
    },
  },
  {
    id: "tokens",
    header: "Tokens",
    cell: ({ row }) => {
      // Tokens will come from response_body parsing or separate fields
      // For now show dash for non-completed
      if (row.original.status !== "completed") return <span className="text-gray-600">—</span>;
      return <span className="text-gray-500">—</span>;
    },
  },
  {
    id: "cost",
    header: "Cost",
    cell: ({ row }) => {
      if (row.original.status !== "completed") return <span className="text-gray-600">—</span>;
      return <span className="text-gray-500">—</span>;
    },
  },
  {
    id: "duration",
    header: "Duration",
    cell: ({ row }) => {
      const ms = row.original.duration_ms;
      if (!ms) return <span className="text-gray-600">—</span>;
      const seconds = Math.round(ms / 1000);
      if (seconds < 60) return <span className="text-gray-500">{seconds}s</span>;
      const minutes = Math.floor(seconds / 60);
      const remainingSeconds = seconds % 60;
      return <span className="text-gray-500">{minutes}m {remainingSeconds}s</span>;
    },
  },
];

export function AsyncRequests() {
  const navigate = useNavigate();
  const [createModalOpen, setCreateModalOpen] = useState(false);
  const { data, isLoading } = useBatchRequests({
    completion_window: "1h",
    active_first: true,
    limit: 50,
  });

  const requests = data?.data ?? [];

  return (
    <div className="p-6">
      <div className="mb-6 flex items-center justify-between">
        <h1 className="text-3xl font-bold">Async</h1>
        <div className="flex gap-2">
          <Button variant="outline" size="sm">
            <Code className="mr-2 h-4 w-4" />
            API
          </Button>
          <Button size="sm" onClick={() => setCreateModalOpen(true)}>
            + New
          </Button>
        </div>
      </div>

      <DataTable
        columns={columns}
        data={requests}
        onRowClick={(row) => navigate(`/workloads/async/${row.id}`)}
      />

      <CreateAsyncModal
        isOpen={createModalOpen}
        onClose={() => setCreateModalOpen(false)}
        onSuccess={() => setCreateModalOpen(false)}
      />
    </div>
  );
}
```

Note: Tokens and Cost columns will be placeholder dashes initially since we need to parse response_body JSON or add dedicated fields. These can be enhanced later.

- [ ] **Step 2: Create placeholder CreateAsyncModal**

Create `dashboard/src/components/modals/CreateAsyncModal/CreateAsyncModal.tsx`:

```tsx
interface CreateAsyncModalProps {
  isOpen: boolean;
  onClose: () => void;
  onSuccess: () => void;
}

export function CreateAsyncModal({ isOpen, onClose }: CreateAsyncModalProps) {
  if (!isOpen) return null;
  // Placeholder — will be implemented in Task 6
  return null;
}
```

- [ ] **Step 3: Verify the page works**

Run: `cd dashboard && pnpm run dev`
Navigate to `/workloads/async` — should show the table (empty if no 1h batches exist).

- [ ] **Step 4: Run lints and tests**

Run: `just lint ts` and `just test ts`

- [ ] **Step 5: Commit**

```bash
git add dashboard/src/components/features/async-requests/ dashboard/src/components/modals/CreateAsyncModal/
git commit -m "feat: implement async request list page with data table"
```

---

### Task 6: Frontend — Create Async Modal

**Files:**
- Modify: `dashboard/src/components/modals/CreateAsyncModal/CreateAsyncModal.tsx`

- [ ] **Step 1: Implement the full modal**

Build the two-tab modal (Compose / Upload JSONL). The Compose tab builds a JSONL from the form, uploads it as a file, then creates a batch with `completion_window=1h`. The Upload tab accepts a JSONL drag-and-drop.

Key implementation details:
- Use existing `Dialog` component from `components/ui/dialog`
- Use existing `Tabs`, `TabsList`, `TabsTrigger`, `TabsContent` from `components/ui/tabs`
- Use existing `useCreateBatch` and file upload from `hooks.ts`
- Model selector should use `useModels` hook to list available models
- Compose mode: build JSONL string from prompts (each line = `{"custom_id": "req-N", "method": "POST", "url": "/v1/chat/completions", "body": {"model": "...", "messages": [...], "temperature": N, "max_tokens": N}}`)
- Upload the JSONL string as a Blob with purpose "batch", then create batch with `completion_window: "1h"`

The modal should follow the same patterns as `CreateBatchModal` for file upload and batch creation.

- [ ] **Step 2: Wire up to AsyncRequests page**

The modal is already imported and rendered in AsyncRequests.tsx from Task 5. Just verify the `onSuccess` callback invalidates the batch requests query.

- [ ] **Step 3: Test the compose flow manually**

Run: `cd dashboard && pnpm run dev`
1. Navigate to `/workloads/async`
2. Click "+ New"
3. Fill in model, prompts (multiple lines), submit
4. Verify batch is created and requests appear in the list

- [ ] **Step 4: Run lints and tests**

Run: `just lint ts` and `just test ts`

- [ ] **Step 5: Commit**

```bash
git add dashboard/src/components/modals/CreateAsyncModal/
git commit -m "feat: implement create async requests modal with compose and upload tabs"
```

---

### Task 7: Frontend — Request Detail View

**Files:**
- Modify: `dashboard/src/components/features/async-requests/AsyncRequestDetail.tsx`

- [ ] **Step 1: Implement the two-column detail view**

Replace the placeholder with the full implementation using `useBatchRequest` hook. Left column shows input/output, right sidebar shows metadata.

Key implementation details:
- Parse `body` field (JSON string) to extract messages array
- Parse `response_body` field (JSON string) to extract assistant response
- Use `useParams` to get `requestId` from URL
- "Back" link navigates to `/workloads/async`
- Batch ID in sidebar links to `/workloads/batch/${batchId}` (will navigate to batch detail via existing BatchInfo redirect)
- Loading state while request is pending/processing
- Error state for failed requests (show `error` field)

- [ ] **Step 2: Verify the detail view**

Navigate to a request from the async list. Verify:
- Input messages display correctly
- Output response displays correctly
- Metadata sidebar shows all fields
- Back button works
- Auto-refresh works for in-progress requests

- [ ] **Step 3: Run lints and tests**

Run: `just lint ts` and `just test ts`

- [ ] **Step 4: Commit**

```bash
git add dashboard/src/components/features/async-requests/
git commit -m "feat: implement async request detail view with two-column layout"
```

---

### Task 8: Final Integration & Cleanup

- [ ] **Step 1: Full lint pass**

Run: `just lint rust` and `just lint ts`
Fix all issues.

- [ ] **Step 2: Full test pass**

Run: `just test rust` and `just test ts`
Fix all failures.

- [ ] **Step 3: Manual smoke test**

1. Navigate to `/workloads/batch` — verify decluttered table with Type and Completion Window columns
2. Navigate to `/workloads/async` — verify request list, "+ New" modal, request detail
3. Verify `/batches` redirects to `/workloads/batch`
4. Verify sidebar navigation works correctly
5. Create an async request via compose mode and verify it appears in the list

- [ ] **Step 4: Commit any final fixes**

```bash
git add -A
git commit -m "fix: integration cleanup for workloads feature"
```
