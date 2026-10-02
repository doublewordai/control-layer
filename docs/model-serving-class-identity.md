# Model serving classes and optional aliases

A tier is an ingestion/execution offering: realtime, flex or batch. A serving
class distinguishes service within a tier. The initial new classes are standard
and fast. The foundation added dormant storage; the catalog now authors classes,
synonyms and public class prices. Request routing and discovery still do not consume
them. See [staged catalog configuration](src/reference/model-provisioning.md#staging-class-destinations-and-public-prices).

## Representation

| Input, illustrative | Canonical model/class | Optional alias row needed |
| --- | --- | --- |
| `example/model` | Same model / standard | No |
| `example/model:fast` | Same model / fast | No |
| `example/model-fast` | Same model / fast | Yes, if explicitly configured |

`model_serving_classes` contains a stable UUID, a class key scoped to the canonical
model, a display name, an endpoint reference and a configurable upstream name.
Two classes can share an upstream without sharing service identity. Display or
destination changes preserve the class ID. Existing model UUIDs remain unchanged.

`model_aliases` contains only optional exact synonyms: unique accepted name,
canonical model UUID, required class UUID and timestamps. The composite foreign
key ensures the class belongs to that model. Synonyms share class identity,
pricing scope, cache namespace and future allowances.

Both new tables start empty. Creating/renaming existing models does not populate
aliases; creating standard/fast class rows does not create aliases either. There
is no reservation backfill, discovery flag, model-write hook or implicit alias
retention on rename. Explicit synonyms remain bound to stable model/class IDs.
Canonical bare/fast recognition is future request-handling work, not SQL triggers.

## Integrity and authoring boundary

Storage rejects duplicate synonym strings, missing classes, classes belonging to
another model, and empty/whitespace-containing names. A class referenced by a
synonym cannot be removed. Models/endpoints referenced by classes cannot be
hard-deleted. Existing ledger foreign keys and history remain unchanged.

Cross-table collisions with primary model/class names are **not** enforced by
the foundation migration. Catalog reconciliation and ordinary model create/rename
writers now serialize on the same transaction advisory lock and reject collisions
with primary class names and synonyms. Direct SQL must perform the same preflight;
a unique synonym key alone is insufficient. Deliberate private-name remapping
requires a separately validated migration preserving access and historical identity.
There is no user-facing API for these tables in this foundation.

## Migration and compatibility

Both migrations create empty tables and indexes, with no model/name backfill and
no changes to existing model writes, prices, routes or discovery. Foreign-key
creation briefly locks referenced tables; both files use the repository's
five-second lock timeout. Indexes are created on new empty tables, so no existing
ledger index is rebuilt. No new configuration-notification trigger is installed.

Older serving binaries keep using their existing tables. Do not edit migrations
after release. Roll back application/configuration, not by dropping the new tables.
See [migration guidance](migrations.md).

## Subsequent integration

- Catalog-authoritative classes and optional synonyms are implemented as dormant
  configuration. Primary names need no synonym rows. Deploy the new readers to
  every pod before staging public class prices; old binaries do not distinguish
  them from general prices. Runtime activation requires the following integration.
- A small startup-loaded synonym lookup before prompt caching, retaining the
  submitted name for responses/diagnostics. Canonical routing, class availability,
  authentication and admission remain reactive. No synonym polling is required
  initially; a targeted notification hook can be added if needed.
- Public discovery derived from canonical model/classes and access policy, never
  from the optional synonym table. Additional names are not advertised there.
- Captured model/class identity for pricing, cache and durable analytics, preserving
  old queued records and purpose/window semantics.
- Account overrides in a later step, independent of initial empty synonym storage.

No global suffix-rewrite engine is introduced. An eventual generic spelling rule
can feed the same resolved model/class identity without changing ledgers or classes.
Unknown names must not silently select another model.

## Validation

Run `cargo test -p dwctl --test model_serving_class_schema` against local PostgreSQL.
Tests cover empty initial storage, same-model class references, unique synonyms,
model/class identity preservation and deletion constraints. The upgrade test keeps
a prepared legacy read alive while applying both migrations and verifies unchanged
model rows, composite members and empty new tables. It also runs the previous
migration set's compatibility check against the expanded schema. A separate test
resumes the application migration runner after only the class migration has landed,
preserving its rows and applying the alias migration exactly once. These are schema
tests, not routing acceptance or a full previous-binary rollout test.

## Legacy replacement boundaries

The old implementation remains operational while classes are dormant. Remove its
unused behavior in the corresponding replacement, rather than keeping parallel
resolvers indefinitely:

| Existing storage / consumers | Replacement boundary |
| --- | --- |
| `deployed_models.serving_classes` JSON presets; `model_provisioning.rs` | Class catalog configuration replaces numeric target presets; retain shared model metadata. |
| `users.granted_serving_classes`, `default_serving_class`; sync account snapshots | Explicit class selection replaces grants and implicit defaults. Preserve routing preferences such as `self_hosted_only`. |
| `model_overlays.default_serving_class`, `targets`; `org_overlays.rs`, serving API | Replace target/default selection with class overrides. Retain account/model ownership, catalog versioning and prices. |
| `inference/middleware.rs`, `onwards/src/serving.rs`, `onwards/src/handlers.rs` | Replace suffix/default resolution and numeric target injection; preserve ingress scrubbing and trusted priority handling. |
| `sync/onwards_config` and composite member rows | Keep legacy routing until all live consumers have migrated; new classes remain dormant until explicit activation. |
| Tariff ledgers, analytics, queued billing records | Retain historical class keys and compatible readers; introducing class UUIDs does not rewrite history. |

Removing runtime behavior and dropping its columns are separate changes. Keep old
columns/API shapes while supported readers need them; do not edit released migrations.
