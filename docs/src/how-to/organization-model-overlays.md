# Organization model overlays

Organization overlays configure per-model serving behavior and pricing for an
organization in a Control Layer installation. Account defaults, class grants and
model access are configured separately; declaring a price does not grant model
access or enable a serving class.

## Enable the built-in catalog

Control Layer includes an optional YAML catalog reader and startup reconciler.
The catalog files can live on a local filesystem or be mounted into a container;
no particular deployment repository, CI service or orchestration system is
required. Startup provisioning is disabled by default.

Configure the paths in your Control Layer configuration:

```yaml
model_provisioning:
  enabled: true
  directory: ./model-provisioning.d
  org_overlays_directory: ./org-overlays.d
```

Paths are resolved from the server's working directory. Use absolute paths when
mounting configuration files into containers. Enabling provisioning applies both
the [model catalog](../reference/model-provisioning.md) and the organization
catalog at startup; maintain both as the desired configuration for your
installation.

Create organization accounts first. Each organization file references the
account's `username` and model catalog aliases. Overlay and organization-price
views in the admin console are read-only; this release provides no management
API for authoring those overlays. Account-level settings remain editable through
the authorized management API and console.

## Inspect effective configuration

Platform managers can select **General pricing** or an organization above the
pricing section on a model's management page. The selection applies to both token
prices and cache multipliers. Source labels distinguish organization overrides
from inherited general prices. Realtime prices are shown by serving class; batch
and flex use standard-class pricing for the specified completion window. A missing
batch window uses the realtime safety net only after every exact batch scope has
been exhausted; the source label identifies that fallback. With no eligible
realtime price either, the view shows unpriced.

Below cache pricing, **Serving classes** lists the model's presets and
**Organisation overlays** expands each organization's explicit settings and price
overrides. The organization's own page groups these same details by model under
**Model overlays**, with a link to its effective model prices.

These inspection controls are restricted to platform managers. Customer price
listings continue to show a single all-class organization price set, falling back
to general model prices, without exposing class-specific overrides.

## Ownership and inheritance

A declared org/model entry is the complete desired overlay and price set. It
adopts existing current rows for that pair, including rows inserted directly in
the database. For catalog-managed entries, update the YAML source; a
database-only change may be replaced on the next reconciliation.

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

The organization, model alias and prices below are fictional. Token prices are
expressed per million tokens:

```yaml
org: example-organization
models:
  - alias: example-model
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

Billing selects the key owner's class-specific price, then its all-class price,
then the general model price. Each scope must match the purpose and, for batch,
the exact completion window. Playground may fall back to realtime within the
same scope. Batch first exhausts exact-window prices across standard-class,
all-class organization and general model scopes. Only if none matches does it
try realtime prices in that same scope order, using standard class. It never
borrows another batch window or an elevated class's realtime price. This final
fallback supports a single realtime definition as a flat price across tiers.
Prefer explicit prices for every supported window so the intended rates are
clear. A matching zero batch price stops fallback. With neither a batch nor a
realtime match, the request remains unpriced. Continuation and platform keys
have no customer tariffs.

Cache multipliers follow class, all-class, then model scope, with the general
model cache tariff controlling whether cache billing is enabled. Customer model
listings and price sorting show the all-class price or general model price;
class-specific prices are visible only in platform-manager views. Actual billing
still uses the resolved class. Usage's realtime-equivalent comparison is an estimate, separate from the
recorded actual charge.

Zero is an explicit price and stops price fallback. Generally free or unpriced
models retain their existing access without balance or key-cap headroom. A zero
organization price on a generally paid model does not grant that exemption:
positive account balance or `ALLOW_NEGATIVE_BALANCE` is still required, and key caps apply.
A positive organization price on an otherwise free model requires credit for
that account only; it does not restrict other accounts. Admission checks tariff
existence without resolving effective purpose/window/class prices per key/model.

## Validation and rollout

Run the validator from the same Control Layer release that will apply the files:

```sh
dwctl-model-provisioning validate-org-overlays /path/to/org-overlays.d \
  --models /path/to/model-deployments
```

The validator checks syntax, pricing precision, class/purpose compatibility and
model references. You can run it locally or add it to your own CI pipeline. It
cannot verify organization existence or future ledger rows in the target
database. Check those prerequisites before restarting the server with an updated
catalog.

After applying a catalog, verify reconciliation, gateway configuration
synchronization, effective prices, a small billed request and key-cap
attribution. If consolidating existing model aliases, retain each alias until
its callers have moved to the replacement.
