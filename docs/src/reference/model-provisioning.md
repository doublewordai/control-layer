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

## Staging class destinations and public prices

`clay.class_routes` stages explicit destinations on the existing model UUID. This
is preparatory storage: this release continues using the legacy route, general
price, cache configuration and model discovery. Public class prices are deliberately
excluded from runtime tariff/cache histories and existing dashboard price panels;
customer-specific class deals retain their established billing and credit checks.
This release does not accept new inference names or enable class routing. Keep the existing `deployments`, `routing`,
`tariffs` and `cache_tariff` definitions while staging classes.

```yaml
# Merge into the existing clay block, retaining its other fields:
clay:
  class_routes:
    standard:
      display_name: Standard
      endpoint: gateway
      upstream_model_name: dynamo-example/model:throughput
      tariffs:
        - name: Standard realtime
          purpose: realtime
          input_per_million_tokens: "1.00"
          output_per_million_tokens: "2.00"
    fast:
      display_name: Fast
      endpoint: gateway
      upstream_model_name: dynamo-example/model:fast
      aliases: [example/model-fast]
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

The endpoint must exist. Upstream names are configurable, endpoint-scoped strings
that must be nonempty and contain no whitespace. Classes may share a destination.
Class keys retain their IDs when a display name,
destination or price changes. Nonempty class configuration requires `standard`
and `fast`, each with a realtime tariff. Additional keys may use lowercase letters,
digits, hyphens and underscores, starting with a letter. The old numeric-target
`serving_classes` declaration cannot be combined with `class_routes` on one model.
Existing legacy-only catalogs remain valid.

Batch tariffs are allowed only under `standard`: batch always resolves to that
class regardless of the submitted name. Other classes cannot declare batch prices
that would never be selected. Model-level batch tariffs remain supported.

Optional aliases are exact synonyms, not discovery entries or extra model rows.
Do not list the primary bare name or generated `:fast` name as synonyms. Validation
reserves primary class names and rejects collisions with existing models and other
synonyms. Ordinary model creates/renames share the catalog lock and collision check.
Synonyms cannot be rebound to another model/class by a catalog edit; deliberate
private-name migration requires a later, separately validated procedure.

For a model present in the catalog, class routes and synonyms are complete desired
state. Omitted synonyms are removed; omitted classes are removed and their public
prices retired, preserving tariff history. An unchanged declaration preserves IDs,
timestamps and price versions. A whole model omitted from the catalog follows the
existing ownership-release rule below, rather than deleting its classes.

Public class tariffs use the existing ledger with `user_id = NULL` and a class
key. The future resolver order is account/class, account/all-classes, public/class,
then general compatibility price. Batch still exhausts exact completion-window
prices before its standard realtime fallback; zero is a price. Class cache tariffs
contain multipliers, not token prices. They inherit model-level cache enablement
and minimum prefix length; omission inherits the model's multipliers. Declaring
class multipliers requires a model-level `cache_tariff`.

The database's `routing_mode` defaults to `legacy` and is outside catalog ownership;
putting it in YAML is rejected. This release is **not activation-ready**: request
resolution, billing snapshots, reactive forwarding and writer support must land
before switching any model. The catalog refuses edits to models already set to
`class_routes`, instead of overwriting an active route with a composite definition.
The class-aware SQL resolver is not an activation mechanism: switching this flag
manually would bypass the supported rollout boundary. Class-aware admission,
durable billing/cache context and dashboard readers must be integrated together
before enabling it. Do not enable individual public-price readers in this release.
Deploy this support release and update the catalog validator before publishing
catalogs containing the new field. Do not run an older binary against staged public
class prices: its readers do not distinguish them from general prices.

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

An account limit file gives one account its own realtime in-flight limit on
some virtual models, in place of each model's
`settings.realtime_inflight_limit`. The account is a user or an organisation,
named by its username, and an organisation's limit covers every key the
organisation owns. One file per account, mounted at
`model_provisioning.account_limits_directory`:

```yaml
account: acme
realtime_inflight:
  example/chat-model: 200
  example/fast-model: 40
```

Startup applies the files after the model catalog, in one transaction, and
replaces every stored per-account limit with what the files declare. Removing
a line or a file returns that account to the model's default on the next
start. An empty directory clears every per-account limit; a missing directory
changes nothing. An unknown account, or a model that is not a live virtual
model, fails startup before anything is written.

Run the same offline validation used by deployment CI:

```sh
dwctl-model-provisioning validate-account-limits ./account-limits.d --models ./model-provisioning.d
```

It rejects unknown fields, limits below 1, an account declared in two files and
models absent from the model catalog. Whether each account exists can only be
checked against the database, at startup.

`GET /admin/api/v1/models/{id}/realtime-inflight-limits` returns a virtual
model's default and its per-account limits, for platform managers.
