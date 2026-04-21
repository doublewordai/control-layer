# Data Retention On Delete Policy

## Goal

Introduce a clear retention policy for user-facing artifacts so that soft deletes become real data cleanup after an optional time-to-live, without treating all data the same.

The main intent is:

- Clean up processing artifacts that should not live forever.
- Keep analytics and durable account data out of this policy by default.
- Make retention understandable as a product and operations concept, not just a table-by-table implementation detail.

## What Problem We Are Solving

Today, deletion is partly split across two concepts:

- User-facing delete actions perform soft deletes.
- A background purge path eventually removes some processing rows.

That is useful for safety, but it is not yet a complete retention policy. We want a clearer answer to:

- Which kinds of data are meant to expire?
- When should they expire?
- Which data should explicitly not be touched?

## Core Idea

Treat retention as a policy on artifact groups, not individual tables.

This keeps the model simple:

- User-facing artifacts can have an optional TTL.
- Processing data inherits the lifecycle of the artifact it belongs to.
- Analytics and durable user/account records are excluded unless we explicitly opt them in later.

## Proposed Retention Groups

### 1. Batch Artifacts

This group covers user-facing batch objects and the files attached to them.

Examples:

- Batch records
- Input files
- Output files
- Error files

Policy:

- May have an optional TTL.
- Can also be deleted explicitly by a user.
- Once eligible for deletion, these artifacts are soft-deleted first and then cleaned up by the existing deletion path.

### 2. Batch Content And Processing Data

This group covers the request-level data that belongs to a batch.

That includes both execution state and customer-facing content.

Examples:

- Requests
- Request templates

Policy:

- Should not have an independent user-facing TTL.
- Should inherit the lifecycle of the parent batch artifact.
- When the parent batch artifact is deleted or expires, this data is cleaned up as part of the same lifecycle.

Reasoning:

- Request templates contain customer prompts and other submitted batch content.
- Requests contain the responses and execution record for that same batch content.
- Even if some of this data is internal from an implementation point of view, it still belongs to the same batch lifecycle from a retention point of view.

### 3. Analytics Data

This group covers usage and operational analytics.

Examples:

- `http_analytics`

Policy:

- Excluded from this retention policy by default.
- No automatic cleanup tied to batch/file deletion.

### 4. Durable User Data

This group covers identity and account-level records.

Examples:

- Users
- Organization membership
- Other durable account records

Policy:

- Out of scope for this proposal.
- Continue to use existing deletion and scrubbing rules.

## Deletion Model

The policy should be easy to explain:

1. A user-facing artifact may optionally be created with a TTL.
2. When that TTL expires, the artifact becomes eligible for deletion.
3. Deletion first removes it from active user-facing views.
4. Internal processing rows tied to that artifact are then cleaned up through the existing background deletion path.

## Default Behavior

The default should be conservative and explicit.

Suggested default:

- There is a system-wide default TTL for batch artifacts, but it is config-driven.
- In `main`, that default should be `null`.
- A `null` default means the artifact does not expire unless a TTL is set explicitly.
- Batch content and processing data is still cleaned up when its parent artifact is deleted.
- Analytics remains untouched.
