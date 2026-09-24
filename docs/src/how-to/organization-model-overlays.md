# Organization model overlays

Define customer deals in the mounted `org-overlays.d` YAML catalog. Reference the
organization's account username and the public or private virtual model alias.
Account defaults and class grants are managed separately; declaring a price does
not grant model access or enable a serving class.

## Ownership and inheritance

A declared org/model entry is the complete desired overlay and price set. It
adopts existing current rows for that pair, including rows inserted directly in
the database. Manage production deals in YAML; direct database edits are suitable
only for isolated testing or an emergency repair that is also reflected in YAML.

Omitted serving fields inherit account/model defaults. For example, omitting
`self_hosted_only` restores the account's preference; `false` explicitly permits
external providers for that model even when the account default forbids them.
Omitted prices retire the corresponding versions and use the normal pricing
fallbacks. Changed prices close the previous version and create a new one;
historical amounts and references are preserved. Unchanged prices retain their
IDs. Undeclared, previously unowned org/model pairs are untouched.

Removing an owned entry retires its overlay and prices. A mounted empty directory
retires all catalog-owned entries; a missing/unconfigured catalog is not a cleanup
request. Future-dated prices must be resolved before catalog adoption or editing:
startup reconciliation applies immediately and does not author schedules.

## Example: batch/flex and class-specific realtime prices

These are illustrative prices per million tokens, not customer terms:

```yaml
org: example-customer
models:
  - alias: z-ai/glm-5.2
    default_class: throughput
    tariffs:
      - name: realtime-default
        purpose: realtime
        input_per_million_tokens: "2.00"
        output_per_million_tokens: "6.00"
      - name: batch-24h
        purpose: batch
        completion_window: 24h
        input_per_million_tokens: "0.50"
        output_per_million_tokens: "1.50"
      - name: batch-1h-and-flex
        purpose: batch
        completion_window: 1h
        input_per_million_tokens: "1.00"
        output_per_million_tokens: "3.00"
    cache_tariff:
      write_multiplier_5m: "1"
      write_multiplier_1h: "1.25"
      write_multiplier_24h: "2"
      read_multiplier: "0.10"
    class_pricing:
      throughput:
        tariffs:
          - name: throughput-realtime
            purpose: realtime
            input_per_million_tokens: "1.50"
            output_per_million_tokens: "4.50"
        cache_tariff:
          write_multiplier_5m: "1"
          write_multiplier_1h: "1.25"
          write_multiplier_24h: "2"
          read_multiplier: "0.08"
      interactive:
        tariffs:
          - name: interactive-realtime
            purpose: realtime
            input_per_million_tokens: "3.00"
            output_per_million_tokens: "9.00"
        cache_tariff:
          write_multiplier_5m: "1"
          write_multiplier_1h: "1.50"
          write_multiplier_24h: "2"
          read_multiplier: "0.15"
```

For this example, enable the model's throughput/interactive presets and grant the
organization those classes separately. Realtime requests use the resolved class:
explicit suffix first, then applicable overlay/account/model defaults. Batch and
flex use standard; flex uses the batch `1h` tariff. Prefer putting batch prices in
the all-class `tariffs` list. `class_pricing.standard` also supports batch;
interactive/throughput batch prices are rejected by validation.

## Billing, display and admission

Billing selects the key owner's class-specific deal, then its all-class deal,
then the general model price. Each scope must match the purpose and, for batch,
the exact completion window. Playground may fall back to realtime within the
same scope; batch never falls back to realtime. If no scope supplies the batch
window, it is unpriced: define every supported window in the general model
catalog. Continuation and platform keys have no customer tariffs.

Cache multipliers follow class, all-class, then model scope, with the general
model cache tariff controlling whether cache billing is enabled. Customer model
listings and price sorting show the all-class deal or general model price;
class-specific prices remain internal. Actual billing still uses the resolved
class. Usage's realtime-equivalent comparison is an estimate, separate from the
recorded actual charge.

Zero is an explicit price and stops price fallback. Generally free or unpriced
models retain their existing access without balance or key-cap headroom. A zero
customer deal on a generally paid model does not grant that exemption: positive
account balance or `ALLOW_NEGATIVE_BALANCE` is still required, and key caps apply.
A positive customer deal on an otherwise free model requires credit for that
account only; it does not restrict other accounts. Admission checks tariff
existence without resolving effective purpose/window/class prices per key/model.

## Validation and rollout

Run the matching release's validator before merging catalog edits:

```sh
dwctl-model-provisioning validate-org-overlays /path/to/org-overlays.d \
  --models /path/to/model-deployments
```

Internal CI runs this against the staging release image when organization YAML
files exist. It checks syntax, pricing precision, class/purpose compatibility and
model references. It cannot verify organization existence or future ledger rows
in the target database. Check those prerequisites before rollout, then verify
catalog reconciliation, Onwards synchronization, effective prices, a small billed
request and key-cap attribution. Do not remove an existing private alias until
its callers have moved to the shared model.
