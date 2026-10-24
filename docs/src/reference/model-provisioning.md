# Model Provisioning

Model provisioning lets an installation describe its model graph, settings,
access and prices in version-controlled YAML instead of recreating rows through
the dashboard after installation.

Each file represents one canonical Hugging Face model. The top-level `backend`
object is reserved for inference-deployment automation and is deliberately
opaque to dwctl. The optional `clay` object is the part applied to the control
layer.

```yaml
model: Qwen/Qwen3-32B

backend:
  engine: sglang

clay:
  alias: qwen3-32b
  display_name: Qwen 3 32B
  description: Production Qwen deployment
  type: chat
  capabilities: [reasoning]

  settings:
    realtime_inflight_limit: 64
    batch_capacity: 16
    throughput: 8
    sanitize_responses: true
    trusted: false
    allowed_batch_completion_windows: [24h]
    metadata:
      provider: Qwen

  deployments:
    - alias: qwen3-32b-piccolo
      model_name: Qwen/Qwen3-32B
      endpoint: piccolo
      type: chat
      capabilities: [reasoning]
      settings:
        sanitize_responses: true
        trusted: false
      provider_pricing:
        mode: per_token
        input_per_million_tokens: "0.40"
        output_per_million_tokens: "1.20"

    - alias: qwen3-32b-chiaotzu
      model_name: Qwen/Qwen3-32B
      endpoint: chiaotzu
      type: chat
      capabilities: [reasoning]
      settings:
        sanitize_responses: true
        trusted: false
      provider_pricing:
        mode: hourly
        rate: "18.50"
        input_token_cost_ratio: "0.25"

  routing:
    strategy: weighted_random
    fallback:
      enabled: true
      on_rate_limit: true
      on_status: [429, 499, 500, 502, 503, 504]
      realtime_on_status: [529]
      with_replacement: false
      max_attempts: 3
      backoff:
        initial_ms: 100
        max_ms: 5000
        factor: 2
        jitter: full
      max_total_backoff_ms: 10000
    pools:
      default:
        - deployment: qwen3-32b-piccolo
          enabled: true
          weight: 1
          sort_order: 0
        - deployment: qwen3-32b-chiaotzu
          enabled: true
          weight: 1
          sort_order: 1
      completions:
        - deployment: qwen3-32b-piccolo
          enabled: true
          weight: 1
          sort_order: 0
          strip_leading_bos: true
          render_kwargs:
            add_generation_prompt: false

  tariffs:
    - name: Realtime
      purpose: realtime
      input_per_million_tokens: "0.80"
      output_per_million_tokens: "2.40"
    - name: 24-hour batch
      purpose: batch
      completion_window: 24h
      input_per_million_tokens: "0.40"
      output_per_million_tokens: "1.20"

  cache_tariff:
    write_multiplier_5m: "1.25"
    write_multiplier_1h: "2.0"
    write_multiplier_24h: "3.0"
    read_multiplier: "0.1"
    min_prefix_tokens: 1024

  access_groups: [Customers]
  traffic_rules:
    - action: deny
      purpose: playground
    - action: redirect
      purpose: continuation
      target: qwen3-32b
```

## Identity and references

Database IDs never appear in the catalog. A model's exact `alias` is its stable
identity. Existing aliases retain their UUIDs; previously unseen aliases receive
normal database-generated UUIDs. Aliases across all catalog files must also be
unique when compared case-insensitively.

Physical deployments reference inference endpoints by exact endpoint `name`.
Components and redirect rules reference models by exact alias. Access grants
reference groups by exact group `name`. Referenced endpoints and groups must
already exist; provisioning does not create them.

A physical deployment shared by multiple virtual models is declared once in
one document. Any virtual model in the directory may reference that alias from
its pools; repeating the deployment declaration would be an alias collision.

Every virtual model requires a non-empty `routing.pools.default`. The only pool
names currently supported are `default` and `completions`.

## Serving classes and class routing

A model can offer more than one serving class. `model` routes to its `standard`
class and `model:<class>` to any other class it declares, each with its own
upstream, realtime prices and default realtime limit. Classes are declared in
`clay.class_routes`; they take effect only once the model sets
`routing_mode: class_routes`. Until then the model keeps its legacy route
(`deployments`, `routing`, `tariffs`), and declared classes are stored but unused.

```yaml
# Merge into the existing clay block, retaining its other fields:
clay:
  routing_mode: class_routes   # default: legacy
  class_routes:
    standard:
      display_name: Standard
      endpoint: gateway
      upstream_model_name: example/model
      tariffs:
        - name: Standard realtime
          purpose: realtime
          input_per_million_tokens: "1.00"
          output_per_million_tokens: "2.00"
    fast:
      display_name: Fast
      endpoint: gateway
      upstream_model_name: example/model-fast
      realtime_inflight_limit: 20
      tariffs:
        - name: Fast realtime
          purpose: realtime
          input_per_million_tokens: "2.00"
          output_per_million_tokens: "4.00"
      cache_tariff:
        write_multiplier_5m: "1.25"
        write_multiplier_1h: "2.0"
        write_multiplier_24h: "3.0"
        read_multiplier: "0.1"
```

Rules:

- `standard` is required whenever `class_routes` is nonempty, and
  `routing_mode: class_routes` requires a declared `standard` class. Other classes
  are optional. Keys use lowercase letters, digits, hyphens and underscores,
  starting with a letter. Every class needs a realtime tariff.
- The endpoint must exist. Upstream names are endpoint-scoped strings, nonempty
  and without whitespace. Classes may share a destination.
- `realtime_inflight_limit` caps one account's concurrent realtime requests to
  that class. It must be positive; omitted, the class uses the model's
  `settings.realtime_inflight_limit`. Each class counts separately.
- Batch tariffs are allowed only under `standard`. Model-level batch tariffs
  remain supported.
- `aliases` must be empty: class names are generated (`model`, `model:<class>`),
  and synonyms are not supported. The retired numeric-target `serving_classes`
  declaration is accepted only as an empty map.
- An activated model cannot carry redirect traffic rules. Deny rules apply to
  every class.

### What activation changes

With `routing_mode: class_routes`, the gateway serves one route per class: `model`
for `standard` and `model:<class>` for the rest. Each route forwards to its class's
endpoint and upstream name, without the composite's members or stream
continuation. The model's fallback settings still apply as retries against that
one upstream. `self_hosted_only` does not apply: the class names its upstream.
Every route shares the model's deny rules, and only `standard` carries the
model's batch capacity.

- **Realtime only.** Flex, background and batch requests use the `standard`
  class. A flex or background request naming `model:<class>` is refused with
  400 `serving_class_realtime_only`; a batch line naming it is refused at upload.
- **Discovery.** `/v1/models` lists `model:<class>` beside the model for each
  class other than `standard`, with the reasoning efforts its upstream supports.
  Class entries carry no prices yet.
- **Analytics and billing.** A request to `model:<class>` is recorded under the
  model's name with `requested_serving_class` and `resolved_serving_class` set to
  the class, and is priced as that class ([how billing works][billing]).
- **Prompt caching.** A class route inherits its model's cache enablement and
  minimum prefix. `standard` keeps the model's cache scope; other classes use
  their own, because they reach different engines.
- **Account settings.** Account files can set priority, scheduling hints,
  toleration presets, realtime limits and prices for any class the model declares.
  A setting without a class applies to the model as a whole; a class's own setting
  wins for that class.

An activation the gateway cannot serve (no `standard` class, or redirect rules
added outside the catalogue) falls back to the legacy route and is logged as
`invalid_class_activation`.

To roll back on a release that supports class routing, set `routing_mode: legacy`
and redeploy. Keep the legacy `deployments`, `routing`, `tariffs` and
`cache_tariff` in place while a model is activated, so the rollback has a route
and prices to return to; class caching also inherits the model's `cache_tariff`.
The catalogue owns the column: it is written on every reconcile, and a model
omitted from the catalogue keeps its last value. Class prices apply only while
class routes serve, so a class request still waiting to be billed when the model
rolls back is recorded in its class but priced at the model's price.

To roll back to an older release, first deploy the catalogue with
`routing_mode: legacy` on every model. Then remove the `routing_mode` and
`class_routes.<class>.realtime_inflight_limit` fields, and any `class_routes`
without a `fast` class, before deploying the older image: older releases reject
those at startup.

### Ownership

For a model present in the catalogue, `class_routes` is complete desired state.
Omitted classes are removed and their public prices retired, preserving tariff
history. An unchanged declaration preserves IDs, timestamps and price versions.
Class keys keep their IDs when a display name, destination, limit or price
changes. A whole model omitted from the catalogue follows the existing
ownership-release rule below rather than deleting its classes.

Public class tariffs use the existing ledger with `user_id = NULL` and a class
key, and are read only for models with `routing_mode: class_routes`. Class cache
tariffs contain multipliers, not token prices; omission inherits the model's
multipliers, and declaring them requires a model-level `cache_tariff`.

Deploy a release that supports these fields, and update the catalogue validator,
before publishing catalogues that use them: older releases reject the
`routing_mode` field and refuse to start.

[billing]: ../conceptual-guides/how-billing-works.md#organisation-and-serving-class-price-precedence

## Authoritative behavior

The complete catalog is loaded and validated before a transaction starts. The
transaction then takes an advisory lock, resolves every reference, clears the
`provisioning_source` marker from all models, and upserts the catalog. A failure
rolls the entire operation back.

An empty directory, or a directory containing only backend-only documents, is
a no-op. No transaction is opened and existing provisioning markers are left
unchanged.

For models present in YAML, model fields, physical deployments, components,
routing, access groups, traffic rules and active tariffs are authoritative.
Manual dashboard changes to those fields remain visible until the next server
restart, when YAML restores them. The dashboard displays a warning on such
models. Two values are exceptions:

- `routing.fallback.realtime_on_status` (statuses that fail over realtime
  requests only): when omitted, the stored value is kept.
- A component's `enabled` is applied when provisioning creates the component.
  An existing component keeps its stored value across restarts; enable or
  disable it through the admin API or dashboard.
- `settings.realtime_inflight_limit` on the virtual model (the default number
  of realtime requests one account may have in flight on it): when omitted,
  the stored value is kept, and a new model starts at 14. It belongs to the
  virtual model; setting it on a deployment is an error. Per-account limits
  are declared in [account limit files](#account-limits), not in the model
  catalog. A request that a traffic rule redirects to another model counts
  against the limit of the model it named.
- `settings.batch_capacity` on the virtual model is the global cap on batch
  requests in flight on it, enforced by onwards when
  [`limits.batch_inflight.enforce`](./configuration.md#batch-in-flight-limits)
  is on. A deployment's own `batch_capacity` remains valid and still sets that
  deployment's fusillade starting concurrency; only the virtual model's value
  becomes the onwards batch cap. A positive virtual-model value
  is used as the cap; when it is omitted (or non-positive, which is invalid),
  the model falls back to
  [`limits.batch_inflight.default_capacity`](./configuration.md#batch-in-flight-limits)
  (default `200`). Over the cap, batch requests are refused with `529` and
  code `batch_capacity_exceeded`, never `429`. The virtual-model value is
  dual-purpose: it
  also seeds fusillade's per-daemon *starting* concurrency, which adaptive
  concurrency may grow past (the configured default does **not** change that
  starting concurrency). Turning enforcement on turns that starting point into
  a global ceiling, so set the value — or raise `default_capacity` — to the
  intended global cap first. File batches, flex and background requests all
  share this one per-model ceiling.

The retired settings `requests_per_second`, `burst_size` and `capacity` are
still accepted so older catalogs load, but they are ignored.

Models omitted from YAML are not deleted or otherwise rewritten. Their
`provisioning_source` becomes `NULL`, which makes them manually managed again.
Physical models that disappear from an endpoint filter are marked inactive;
endpoint synchronization does not delete or rewrite YAML-owned rows.

## Tariff history

Tariffs retain their temporal history. Within an account/class scope, their natural
key is `(model, purpose, completion_window)`, with `completion_window` required only
for `batch`. Model-level `tariffs` remain the public all-class compatibility scope;
each `class_routes.<key>.tariffs` declaration owns its public class scope:

- An identical active tariff is left untouched, retaining its ID and
  `valid_from`.
- A changed tariff closes the active row and inserts its successor at the same
  transaction timestamp.
- A missing desired key is closed without a successor.
- A reintroduced tariff always creates a new version; historical rows are never
  reopened, updated in place, or deleted.

Cache tariffs follow the same rule. Provisioning rejects a model with a
future-scheduled tariff or cache-tariff version because the startup format has
no schedule semantics.

Prices are strings to retain decimal precision. Input and output prices are
expressed per million tokens; dwctl converts them to the database's per-token
representation and rejects values that cannot be represented exactly.

## Validation and deployment

Validate a rendered directory with the Rust types used at runtime:

```bash
cargo run -p dwctl --bin dwctl-model-provisioning -- validate ./model-provisioning.d
```

Print the generated JSON Schema for editor or generic CI integration:

```bash
cargo run -p dwctl --bin dwctl-model-provisioning -- schema
```

The Helm chart exposes a separate, opt-in ConfigMap through
`modelProvisioning.files`. It mounts the files at
`/app/model-provisioning.d`, enables startup provisioning through environment
overrides, and includes the catalog checksum in the pod template so catalog
changes trigger a rollout. An enabled, empty ConfigMap mounts an empty
directory and is a startup no-op.


## Organisation catalog validation and ownership

Run the same offline validation used by deployment CI:

```sh
dwctl-model-provisioning validate-org-overlays ./org-overlays.d --models ./model-provisioning.d
dwctl-model-provisioning org-schema
```

Both directories must exist. Validation checks the complete org directory and
references against public `clay.alias` values in the model catalog. It reuses
startup parsing and pricing validation: duplicate org/model entries, unknown
fields/classes, incompatible purpose/windows, and invalid decimals are rejected.
Cache multipliers must be exactly representable in `DECIMAL(6,4)`: nonnegative,
less than 100, with no more than four significant fractional places. Redundant
trailing zeros are accepted. This avoids silent rounding and repeated versioning.
The API's cache-pricing writer already validates storage precision.

Offline validation cannot inspect database organisations or future scheduled
prices. Check those against the target database before activating a catalog.
Future-price errors identify the file, organisation, alias and class.

Declaring an org/model makes YAML authoritative for its overlay and current
prices, including existing rows inserted directly in the database. Omitted
routing settings inherit defaults; omitted prices retire. Changed prices are
versioned without altering historical amounts. Undeclared, unowned pairs are
untouched. Future schedules must be resolved before adoption because the catalog
has immediate-only semantics. A failed apply rolls back the whole transaction.
See [Customize Organization Models](../how-to/organization-model-overlays.md)
for examples and the complete ownership contract.

Reconciliation captures one wall-clock timestamp **after** obtaining the
transaction-scoped advisory lock. A waiting replica may have begun its
transaction before the winner; its transaction-start timestamp is therefore
not a valid price-version boundary. Tests force a real lock wait and verify
that the second replica observes the winner's rows without creating versions.

Ordinary price edits close current tariffs and insert open-ended replacements.
A pre-existing future-dated general tariff is different: it already reserves a
future interval. Model tariff replacement returns an explicit bad-request error
until an operator resolves that schedule; it neither cancels the schedule nor
leaves partial model edits. Metadata-only edits are still permitted.

## Account limits

An account file holds one account's own settings on some virtual models: its
realtime in-flight limits, its prices, and the scheduling priority and latency
targets sent with its requests. The account is a user or an organisation,
named by its username, and an organisation's settings cover every key the
organisation owns. One file per account, mounted at
`model_provisioning.account_limits_directory`:

```yaml
account: acme
priority: 100                     # optional; default for every model
models:
  example/chat-model:
    priority: 300                 # optional; overrides the account default
    toleration_preset: no-overflow  # optional; overrides the account's preset
    tariffs:                      # batch prices only, at model level
      - {name: batch-24h, purpose: batch, completion_window: 24h,
         input_per_million_tokens: "0.50", output_per_million_tokens: "1.50"}
    classes:
      standard:
        realtime_inflight: 200
        tariffs:                  # realtime and playground prices, per class
          - {name: realtime, purpose: realtime,
             input_per_million_tokens: "0.70", output_per_million_tokens: "1.40"}
        cache_tariff: {read_multiplier: "0.2", write_multiplier_5m: "1",
                       write_multiplier_1h: "1", write_multiplier_24h: "1"}
        ttft_ms: 1000             # optional latency targets
        itl_ms: 30
        priority: 500             # optional; overrides the model's priority
        toleration_preset: gpu-only  # optional; overrides the model's preset
```

The older shape is still accepted and means the same as
`classes.standard.realtime_inflight`:

```yaml
account: acme
realtime_inflight:
  example/chat-model: 200
```

Rules:

- `classes` may name `standard` and any class the model declares in its
  catalogue [`class_routes`](#serving-classes-and-class-routing). A class the
  file omits keeps the model's defaults. Settings for a class other than
  `standard` take effect once the model is activated (`routing_mode:
  class_routes`); its limit counts that class's requests only.
- Model-level `priority` and `toleration_preset` apply to every class; class-level
  ones override them for that class. Prices do not carry across classes: a class
  without its own `tariffs` in the file is billed at the model's price for that
  class.
- Limits, realtime and playground prices, and the cache tariff are declared per
  class; model-level `tariffs` take batch prices only.
- Priority inherits class, then model, then account, then the default. It is
  sent as the scheduling priority on the account's realtime requests to
  `dynamo` serving members; batch and flex keep their deadline priority.
  `ttft_ms` and `itl_ms` are sent as latency targets on the same realtime
  requests. Neither changes the class the request is billed as. Priority
  `-2147483648` is reserved for background work and rejected.
- `toleration_preset` names one of `scheduling.toleration_presets` (see
  [Account Scheduling](./configuration.md#account-scheduling-tolerations-and-provider-fallback)).
  It inherits class, then model, then the account's own preset (set by a
  platform manager), then none. It applies to realtime, flex and background
  requests, and to batch lines by each line's model. A preset id the
  deployment does not configure is logged at startup and fails closed to the
  empty (most restrictive) list.
- If a model appears in both shapes, the two limits must be equal.
- An account/model pair may be configured in its account file or in an
  [organisation overlay](#organisation-catalog-validation-and-ownership), not
  both. The older limits shape may sit alongside an overlay.

Startup applies the files after the model catalog and organisation overlays,
in one transaction under one advisory lock:

- **Limits** replace every stored per-account limit with what the files
  declare. Removing a line or a file returns that account to the model's
  default on the next start.
- **Priorities, latency targets and prices** are owned row by row. Declaring
  an account/model pair makes the file authoritative for its current prices
  and policies, adopting matching rows written by hand. Omitted prices retire, and changed
  prices are versioned without altering historical amounts. Rows that another
  writer owns, such as the organisation catalog's prices, are refused, never
  taken over.

An empty directory clears everything the account files own. A missing
directory changes nothing. An unknown account, or a model that is not a live
virtual model, fails startup before any account-file setting is written; the
model catalog and organisation overlays have already been applied by then.

Run the same offline validation used by deployment CI:

```sh
dwctl-model-provisioning validate-account-limits ./account-limits.d \
  --models ./model-provisioning.d --org-overlays ./org-overlays.d
dwctl-model-provisioning account-schema
```

It rejects unknown fields, classes the model does not declare, limits and latency targets below 1, an
account declared in two files, entries that configure nothing, prices at the
wrong level, models absent from the model catalog and, with `--org-overlays`,
pairs configured in both catalogs. Whether each account exists can only be
checked against the database, at startup.

`GET /admin/api/v1/models/{id}/realtime-inflight-limits` returns a virtual
model's default and its per-account limits, for platform managers.
