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

Treat retention as a policy on **artifact groups**, not individual tables.

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

This is the key simplification: we avoid situations where a batch still exists but its request content or internal request state has disappeared on a separate schedule.

### 3. Analytics Data

This group covers usage and operational analytics.

Examples:

- `http_analytics`

Policy:

- Excluded from this retention policy by default.
- No automatic cleanup tied to batch/file deletion.

Reasoning:

- Analytics serves a different purpose from user-facing artifacts.
- It is useful for reporting, billing, operational analysis, and historical usage views.
- If we ever want analytics retention, that should be a separate policy with separate discussion.

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

This means retention behaves like:

- **Batch/file artifacts**: configurable and optionally expiring
- **Requests/templates**: inherited cleanup within the same batch lifecycle
- **Analytics/user data**: retained

## Default Behavior

The default should be conservative and explicit.

Suggested default:

- There is a system-wide default TTL for batch artifacts, but it is config-driven.
- In `main`, that default should be `null`.
- A `null` default means the artifact does not expire unless a TTL is set explicitly.
- Batch content and processing data is still cleaned up when its parent artifact is deleted.
- Analytics remains untouched.

This avoids surprising data loss while still giving us a clean way to apply retention where it makes sense. It also makes the platform default visible instead of leaving retention behavior implicit.

## Product/Operator Mental Model

The policy should read like a small set of decisions:

- Do we allow expiry for this kind of artifact?
- If yes, what is the default TTL?
- What is the maximum allowed TTL?
- What happens to child processing data?

That is much easier to reason about than exposing raw table behavior.

## Suggested Policy Shape

At a high level, the configuration should express:

- Which artifact groups support TTL
- Default TTL for those groups
- Maximum TTL for those groups
- Which groups inherit parent deletion
- Which groups are always retained

Illustrative example:

```yaml
retention:
  batch_artifacts:
    default_ttl: null
    max_ttl: 90d
    action: delete

  batch_content_and_processing:
    mode: inherit_parent

  analytics:
    mode: retain

  durable_user_data:
    mode: retain
```

The important point is not the exact schema. The important point is that the policy is grouped by meaning, not by storage detail.

## Why This Shape

This approach gives us:

- A clean story for customers and operators.
- A predictable lifecycle for batches and related files.
- Simpler cleanup rules for requests and templates.
- Clear protection for analytics and durable account data.

It also aligns with the principle that user-visible artifacts should define the lifecycle, while related request content and internal processing data should follow behind them.

## Open Questions

These are the decisions worth discussing with others before implementation:

1. Should batch/file TTL start from creation time or from terminal state?

Recommended direction:
Use terminal state where possible, so a long-running artifact does not expire while still active.

2. What should the global default TTL for batch artifacts be?

Recommended direction:
There should be a global default TTL that is config-driven, with `null` in `main`.

3. Should input files and output/error files share one policy, or should outputs have a different default?

Recommended direction:
Start with one batch artifact policy unless there is a clear operational need to split them.

4. Should analytics retention be discussed separately?

Recommended direction:
Yes. Keep it out of this change.

## Recommended First Scope

The first version should stay narrow:

- Add retention policy for batch-related user-facing artifacts.
- Make requests and request templates inherit deletion from those artifacts as part of the same batch lifecycle.
- Leave analytics and durable user/account data alone.

That gives us a meaningful cleanup policy without turning this into a broad data-governance project.
