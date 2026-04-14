# Workloads & Async UI Feature Design

**Date:** 2026-04-14
**Status:** Approved
**Branch:** async-ui

## Overview

Restructure the dashboard's "Batches" section into a "Workloads" section with two sub-pages: **Batch** (decluttered batch table) and **Async** (flat list of individual requests backed by 1h-completion-window batches). The async page abstracts away the batch concept — users submit individual requests or JSONL files through a simplified modal, and view results as a flat request list. Behind the scenes, everything is still batches.

## 1. Sidebar Navigation

**Current:** Single "Batches" nav item at `/batches`.

**Proposed:**
- "Workloads" section header in the sidebar (small, uppercase, muted text)
- Collapsible via chevron, but **expanded by default**
- Two sub-items indented beneath:
  - **Async** (`/workloads/async`) — listed first (primary workflow)
  - **Batch** (`/workloads/batch`) — listed second
- Old `/batches` route redirects to `/workloads/batch`
- Visibility gated by same `config.batches?.enabled` check as current Batches item

## 2. Batch Page (Decluttered)

**Route:** `/workloads/batch`

Replaces the current batch list view with a cleaner table. Shows all batches regardless of completion window.

### Columns

| Column | Source | Notes |
|--------|--------|-------|
| Created | `created_at` | Timestamp, sortable |
| User | `metadata.created_by_email` | Creator email, shown for PlatformManagers |
| Type | Derived from `completion_window` | `1h` → "async" badge, everything else → "batch" badge |
| Completion Window | `completion_window` | Raw API value (1h, 4h, 24h, etc.) — 1:1 with API parameter |
| Status | `status` | Badge with icon (same statuses as current) |
| Progress | `request_counts` | Stacked progress bar + count (e.g. "325/500") |
| Duration | `in_progress_at` → `completed_at` | Elapsed time |
| Actions | — | Menu: view, download output/error, cancel, retry, delete |

### Removed columns (vs current)

Input File ID, Source, Priority, Batch ID, Context.

### Header

- Title: "Batches"
- Button: "+ New Batch" (opens existing CreateBatchModal)

### Behavior

- Clicking a row navigates to the existing BatchInfo detail page
- No toggle/filter — all batches shown by default
- Same auto-refresh and active-first sorting as current

## 3. Async Page

**Route:** `/workloads/async`

A flat chronological list of individual requests from batches with `completion_window=1h`. The batch abstraction is hidden from the user.

### Columns

| Column | Source | Notes |
|--------|--------|-------|
| Created | `created_at` | Timestamp |
| Model | `model` | Model name/alias |
| Status | `status` | Badge: running, queued, completed, failed |
| Tokens | `prompt_tokens` / `completion_tokens` | Format: "1,247 / 832", dashes if incomplete |
| Cost | `cost` | Dollar amount, dash if incomplete |
| Duration | Duration from start to completion | Elapsed time, dash if incomplete |
| (link) | — | "View →" navigates to request detail |

### Header

- Title: "Async"
- No auto-refreshing indicator (refreshes silently under the hood)
- Buttons:
  - `</> API` — opens API usage documentation/guide
  - `+ New` — opens the async submission modal

### Behavior

- **Data source:** `GET /admin/api/v1/batches/requests` (new endpoint, see Section 6)
- **Active-first sorting:** Running/queued requests float to top with subtle blue background highlight
- **Auto-refresh:** Polls every 2s when active requests exist (same pattern as batch page)
- **Pagination:** Offset-based, matching admin API pattern

## 4. Async Submission Modal

**Title:** "Create Async Requests"
**Subtitle:** "Submit requests for async processing (1h completion window)"

### Two tabs

**Tab 1: Compose (default)**

Fields:
- **Model** — dropdown selector (from available models)
- **System** (optional) — textarea for system prompt
- **Prompts** — single textarea, each newline is a separate request. Helper text on the right: "Each line is a separate request"
- **Temperature** — numeric input (default 0.7)
- **Max Tokens** — numeric input (default 4096)

Footer shows live request count (e.g., "3 requests") and Cancel / Create buttons.

**Tab 2: Upload JSONL**

- Drag-and-drop zone or click to browse
- No helper text about format — JSONL can contain various request types
- Create button disabled until file selected

### Behind the scenes

Both paths:
1. **Compose:** Build a JSONL file from the form (one line per prompt, all sharing model/system/params), upload as a file with purpose "batch", then `POST /ai/v1/batches` with `completion_window=1h`
2. **Upload:** Upload the JSONL file, then `POST /ai/v1/batches` with `completion_window=1h`

On submit: modal closes, user returns to Async list where new request(s) appear.

## 5. Request Detail View

**Route:** `/workloads/async/:requestId`

Two-column layout:

### Left column (main content)

**Input section:**
- Messages displayed with role labels (System, User) in distinct colors
- System message label in purple, User message label in blue

**Output section:**
- Assistant response with green role label
- Failed requests: show error message instead
- Running requests: loading state, auto-refreshes

### Right sidebar (metadata)

Stacked key-value pairs grouped with dividers:

**Details group:**
- Status (badge)
- Model
- Created (timestamp)
- Duration

**Tokens group:**
- Prompt Tokens
- Completion Tokens
- Cost

**Batch group:**
- Batch ID (clickable, links to `/workloads/batch/:batchId`)
- Completion Window
- Request Index (e.g., "1 of 3")

Metrics in the sidebar can be expanded later with more detail.

### Header

- "← Back" link returns to Async list
- "Request Detail" title
- Status badge

## 6. Backend: New Admin Endpoint

### `GET /admin/api/v1/batches/requests`

Lists individual batch requests from fusillade, joining against batches to get metadata.

**Query parameters:**

| Param | Type | Default | Notes |
|-------|------|---------|-------|
| `skip` | integer | 0 | Offset-based pagination (reuse `Pagination` struct) |
| `limit` | integer | 10 | Max 100 (reuse `Pagination` struct) |
| `completion_window` | string | `1h` | Filter by batch completion window |
| `status` | string | — | Filter by request status |
| `model` | string | — | Filter by model (optional, future use) |
| `active_first` | bool | true | Sort running/queued before completed |

**Response:** `PaginatedResponse<AsyncRequestResponse>`

```json
{
  "data": [
    {
      "id": "req_abc123",
      "batch_id": "batch_6f3a9b",
      "model": "claude-sonnet-4",
      "status": "completed",
      "created_at": 1713100000,
      "completed_at": 1713100012,
      "prompt_tokens": 1247,
      "completion_tokens": 832,
      "cost": 0.012,
      "duration_ms": 12000
    }
  ],
  "total_count": 142,
  "skip": 0,
  "limit": 10
}
```

### `GET /admin/api/v1/batches/requests/:id`

Returns full request detail including input messages and output response.

**Response:** `AsyncRequestDetailResponse`

```json
{
  "id": "req_abc123",
  "batch_id": "batch_6f3a9b",
  "model": "claude-sonnet-4",
  "status": "completed",
  "created_at": 1713100000,
  "completed_at": 1713100012,
  "prompt_tokens": 1247,
  "completion_tokens": 832,
  "cost": 0.012,
  "duration_ms": 12000,
  "input": {
    "messages": [
      { "role": "system", "content": "You are a financial analyst..." },
      { "role": "user", "content": "Summarize the key points..." }
    ]
  },
  "output": {
    "message": { "role": "assistant", "content": "Here are the key highlights..." }
  }
}
```

### Auth & Permissions

- Same auth as other admin endpoints (session cookie, proxy header, or API key)
- StandardUser sees own requests only (filtered by `created_by` in batch metadata)
- PlatformManager sees all requests

### Implementation Notes

- Reuse `Pagination` struct from `api/models/pagination.rs`
- Reuse `PaginatedResponse<T>` for response wrapping
- No new database tables — queries fusillade's existing request storage
- Join against batch records to get `completion_window` for filtering
- Handler follows same pattern: `async fn handler<P: PoolProvider>(State(state): State<AppState<P>>, ...) -> Result<...>`

## 7. Routes Summary

| Route | Component | Notes |
|-------|-----------|-------|
| `/workloads/async` | AsyncList | Flat request list |
| `/workloads/async/:requestId` | AsyncRequestDetail | Request input/output view |
| `/workloads/batch` | Batches (existing, modified) | Decluttered batch table |
| `/workloads/batch/:batchId` | BatchInfo (existing) | Existing batch detail page |
| `/batches` | Redirect | → `/workloads/batch` |
| `/batches/:batchId` | Redirect | → `/workloads/batch/:batchId` |

## 8. Non-Goals (Out of Scope)

- Search/filtering on the async request list (future enhancement)
- API guide page content (placeholder button for now)
- Changes to the existing batch creation modal
- Changes to the existing batch detail page
- Backend changes to batch creation (`POST /ai/v1/batches` stays unchanged)
- Mobile-specific layout optimizations for request detail view
