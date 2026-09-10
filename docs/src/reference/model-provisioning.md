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
    requests_per_second: 100
    burst_size: 200
    capacity: 64
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

## Authoritative behavior

The complete catalog is loaded and validated before a transaction starts. The
transaction then takes an advisory lock, resolves every reference, clears the
`provisioning_source` marker from all models, and upserts the catalog. A failure
rolls the entire operation back.

For models present in YAML, model fields, physical deployments, components,
routing, access groups, traffic rules and active tariffs are authoritative.
Manual dashboard changes to those fields remain visible until the next server
restart, when YAML restores them. The dashboard displays a warning on such
models.

Models omitted from YAML are not deleted or otherwise rewritten. Their
`provisioning_source` becomes `NULL`, which makes them manually managed again.
Physical models that disappear from an endpoint filter are marked inactive;
endpoint synchronization does not delete or rewrite YAML-owned rows.

## Tariff history

Tariffs retain their temporal history. Their natural key is `(model, purpose,
completion_window)`, with `completion_window` required only for `batch`:

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
changes trigger a rollout. Enabling the chart option with no files is an error.
