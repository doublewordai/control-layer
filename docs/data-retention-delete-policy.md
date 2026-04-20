# Data Retention On Delete Policy

## Goal

Introduce a clear retention policy for user-facing artefacts so that soft deletes become real data cleanup after an optional time-to-live, without treating all data the same.

The main intent is:

- Clean up processing artefacts that should not live forever.
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

Treat retention as a policy on **artefact groups**, not individual tables.

This keeps the model simple:

- User-facing artefacts can have an optional TTL.
- Processing data inherits the lifecycle of the artefact it belongs to.
- Analytics and durable user/account records are excluded unless we explicitly opt them in later.

## Proposed Retention Groups

### 1. Batch Artefacts

This group covers user-facing batch objects and the files attached to them.

Examples:

- Batch records
- Input files
- Output files
- Error files

Policy:

- May have an optional TTL.
- Can also be deleted explicitly by a user.
- Once eligible for deletion, these artefacts are soft-deleted first and then cleaned up by the existing deletion path.

### 2. Batch Processing Data

This group covers internal processing state that exists only to execute or render batch work.

Examples:

- Requests
- Request templates

Policy:

- Should not have an independent user-facing TTL.
- Should inherit the lifecycle of the parent batch artefact.
- When the parent artefact is deleted or expires, this data is cleaned up as part of the same lifecycle.

This is the key simplification: we avoid situations where a batch still exists but its internal request state has disappeared on a separate schedule.

### 3. Analytics Data

This group covers usage and operational analytics.

Examples:

- `http_analytics`

Policy:

- Excluded from this retention policy by default.
- No automatic cleanup tied to batch/file deletion.

Reasoning:

- Analytics serves a different purpose from user-facing artefacts.
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

1. A user-facing artefact may optionally be created with a TTL.
2. When that TTL expires, the artefact becomes eligible for deletion.
3. Deletion first removes it from active user-facing views.
4. Internal processing rows tied to that artefact are then cleaned up through the existing background deletion path.

This means retention behaves like:

- **Batch/file artefacts**: configurable and optionally expiring
- **Requests/templates**: inherited cleanup
- **Analytics/user data**: retained

## Default Behavior

The default should be conservative.

Suggested default:

- No TTL unless explicitly set for a user-facing artefact.
- Batch processing data is still cleaned up when its parent artefact is deleted.
- Analytics remains untouched.

This avoids surprising data loss while still giving us a clean way to apply retention where it makes sense.

## Product/Operator Mental Model

The policy should read like a small set of decisions:

- Do we allow expiry for this kind of artefact?
- If yes, what is the default TTL?
- What is the maximum allowed TTL?
- What happens to child processing data?

That is much easier to reason about than exposing raw table behavior.

## Suggested Policy Shape

At a high level, the configuration should express:

- Which artefact groups support TTL
- Default TTL for those groups
- Maximum TTL for those groups
- Which groups inherit parent deletion
- Which groups are always retained

Illustrative example:

```yaml
retention:
  batch_artefacts:
    default_ttl: null
    max_ttl: 90d
    action: delete

  batch_processing:
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

It also aligns with the principle that user-visible artefacts should define the lifecycle, while internal processing data should follow behind them.

## Open Questions

These are the decisions worth discussing with others before implementation:

1. Should batch/file TTL start from creation time or from terminal state?

Recommended direction:
Use terminal state where possible, so a long-running artefact does not expire while still active.

2. Should there be a global default TTL for batch artefacts, or should TTL be opt-in only?

Recommended direction:
Start opt-in only.

3. Should input files and output/error files share one policy, or should outputs have a different default?

Recommended direction:
Start with one batch artefact policy unless there is a clear operational need to split them.

4. Should analytics retention be discussed separately?

Recommended direction:
Yes. Keep it out of this change.

## Recommended First Scope

The first version should stay narrow:

- Add retention policy for batch-related user-facing artefacts.
- Make requests and request templates inherit deletion from those artefacts.
- Leave analytics and durable user/account data alone.

That gives us a meaningful cleanup policy without turning this into a broad data-governance project.
