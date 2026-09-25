import { useState } from "react";
import { Link } from "react-router-dom";
import {
  useModelCachePricing,
  useModelOverlays,
  useOrganizationServing,
} from "@/api/control-layer";
import type {
  Model,
  ModelTariff,
  ServingOverlay,
  TariffApiKeyPurpose,
} from "@/api/control-layer/types";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { formatTariffPrice, getTariffDisplayName } from "@/utils/formatters";
import {
  currentTariffs,
  priceSource,
  resolveCachePrice,
  resolveTokenPrice,
} from "../../serving/pricing";
import { OverlayDetails } from "../../serving/OverlayDetails";

interface Props {
  model: Model;
  manager: boolean;
  initialOrganization?: string;
  onEditPrices: () => void;
  onEditCache: () => void;
}

export function ModelPricing({
  model,
  manager,
  initialOrganization,
  onEditPrices,
  onEditCache,
}: Props) {
  const [selected, setSelected] = useState(initialOrganization ?? "general");
  const organization = manager && selected !== "general" ? selected : undefined;
  const overlays = useModelOverlays(model.id, { enabled: manager });
  const cache = useModelCachePricing(model.id, { enabled: manager });
  const own = useOrganizationServing(organization ?? "", {
    enabled: !!organization,
  });
  const options = new Map<string, string>();
  if (manager) {
    for (const row of overlays.data ?? [])
      options.set(
        row.organization_id,
        row.organization_display_name?.trim() || row.organization_name,
      );
    for (const row of model.tariffs ?? [])
      if (row.organization_id && !options.has(row.organization_id))
        options.set(row.organization_id, row.organization_id);
    if (organization && !options.has(organization))
      options.set(organization, organization);
  }
  const general = (model.tariffs ?? []).filter((row) => !row.organization_id);
  const rows = organization
    ? [
        ...general,
        ...(own.data?.tariffs ?? []).filter(
          (row) => row.deployed_model_id === model.id,
        ),
      ]
    : general;
  const orgCache = (organization ? (own.data?.cache_tariffs ?? []) : []).filter(
    (row) => row.deployed_model_id === model.id,
  );
  const selectedOverlay = own.data?.overlays.find(
    (row) => row.deployed_model_id === model.id,
  );
  const classes = organization
    ? [
        ...new Set([
          "standard",
          ...Object.keys(model.serving_classes ?? {}),
          ...(selectedOverlay?.targets ? ["custom"] : []),
          ...rows
            .filter((row) => row.organization_id === organization)
            .flatMap((row) => (row.serving_class ? [row.serving_class] : [])),
          ...orgCache.flatMap((row) =>
            row.serving_class ? [row.serving_class] : [],
          ),
        ]),
      ]
    : ["standard"];
  const windows = [
    ...new Set([
      ...(model.allowed_batch_completion_windows ?? []),
      ...currentTariffs(rows).flatMap((row) =>
        row.api_key_purpose === "batch" && row.completion_window
          ? [row.completion_window]
          : [],
      ),
    ]),
  ].sort((a, b) => a.localeCompare(b, undefined, { numeric: true }));
  const waiting = !!organization && own.isLoading;
  const failed = !!organization && own.isError;
  const tokenRow = (
    cls: string,
    purpose: TariffApiKeyPurpose,
    window: string | null = null,
  ) => {
    const row = resolveTokenPrice(rows, organization, cls, purpose, window);
    return (
      <PriceRow
        key={`${purpose}:${window}:${cls}`}
        row={row}
        label={
          purpose === "batch"
            ? `${getTariffDisplayName(purpose, window)} · ${window}`
            : organization
              ? cls
              : "All classes"
        }
        source={`${priceSource(row, !!organization)}${purpose !== "realtime" && row?.api_key_purpose === "realtime" ? " · realtime fallback" : ""}`}
        note={
          purpose !== "batch" &&
          cls !== "standard" &&
          cls !== "custom" &&
          own.data
            ? !model.serving_classes?.[cls]
              ? "Not offered by this model"
              : !own.data.granted_serving_classes.includes(
                    cls as "interactive" | "throughput",
                  )
                ? "Not granted to this organisation"
                : undefined
            : undefined
        }
      />
    );
  };
  return (
    <>
      <section className="border-t pt-6 space-y-4" aria-label="Model pricing">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <h3 className="text-sm font-medium">Pricing</h3>
          {manager && (
            <div className="flex flex-wrap items-center gap-2 min-w-0 max-w-full">
              <label
                htmlFor="pricing-organization"
                className="text-sm text-muted-foreground whitespace-nowrap"
              >
                Pricing for
              </label>
              <Select value={selected} onValueChange={setSelected}>
                <SelectTrigger
                  id="pricing-organization"
                  className="w-64 max-w-full"
                >
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="general">General pricing</SelectItem>
                  {[...options]
                    .sort((a, b) => a[1].localeCompare(b[1]))
                    .map(([id, name]) => (
                      <SelectItem key={id} value={id}>
                        {name}
                      </SelectItem>
                    ))}
                </SelectContent>
              </Select>
            </div>
          )}
          {manager && !organization && (
            <Button size="sm" variant="outline" onClick={onEditPrices}>
              Manage Tariffs
            </Button>
          )}
        </div>
        {manager && overlays.isError && (
          <p role="alert" className="text-sm text-destructive">
            Could not load organisation options.{" "}
            <button
              className="underline"
              onClick={() => void overlays.refetch()}
            >
              Retry
            </button>
          </p>
        )}
        {organization && (
          <p className="text-xs text-muted-foreground">
            Effective prices include this organisation’s overrides and inherited
            general prices. Overrides are read-only here.
          </p>
        )}
        {waiting ? (
          <p role="status">Loading organisation pricing…</p>
        ) : failed ? (
          <p role="alert">
            Could not load organisation pricing.{" "}
            <button className="underline" onClick={() => void own.refetch()}>
              Retry
            </button>
          </p>
        ) : manager ? (
          <>
            <section aria-label="Realtime prices">
              <h4 className="text-sm font-medium mb-2">Realtime</h4>
              <div className="space-y-2">
                {classes.map((cls) => tokenRow(cls, "realtime"))}
              </div>
            </section>
            {windows.length > 0 && (
              <section aria-label="Batch and flex prices">
                <h4 className="text-sm font-medium mb-2">
                  Batch and flex · standard class
                </h4>
                <div className="space-y-2">
                  {windows.map((window) =>
                    tokenRow("standard", "batch", window),
                  )}
                </div>
              </section>
            )}
            {classes.some((cls) =>
              resolveTokenPrice(rows, organization, cls, "playground"),
            ) && (
              <section aria-label="Playground prices">
                <h4 className="text-sm font-medium mb-2">Playground</h4>
                <div className="space-y-2">
                  {classes.map((cls) => tokenRow(cls, "playground"))}
                </div>
              </section>
            )}
          </>
        ) : (
          <div className="space-y-2">
            {currentTariffs(model.tariffs ?? [])
              .filter((row) => !row.serving_class)
              .map((row) => (
                <PriceRow
                  key={row.id}
                  row={row}
                  label={getTariffDisplayName(
                    row.api_key_purpose,
                    row.completion_window,
                  )}
                />
              ))}
          </div>
        )}
      </section>
      {manager && (
        <>
          <section
            className="border-t pt-6 space-y-3"
            aria-label="Cache pricing"
          >
            <div className="flex justify-between items-center">
              <h3 className="text-sm font-medium">Cache pricing</h3>
              {!organization && (
                <Button size="sm" variant="outline" onClick={onEditCache}>
                  {cache.data?.enabled ? "Edit" : "Configure"}
                </Button>
              )}
            </div>
            {waiting || cache.isLoading ? (
              <p role="status">Loading cache pricing…</p>
            ) : failed || cache.isError ? (
              <p role="alert">
                Could not load cache pricing.{" "}
                <button
                  className="underline"
                  onClick={() => {
                    if (organization) void own.refetch();
                    void cache.refetch();
                  }}
                >
                  Retry
                </button>
              </p>
            ) : !cache.data?.enabled ? (
              <p className="text-sm text-muted-foreground">
                Cache pricing is disabled on this model. Organisation
                multipliers do not enable it.
              </p>
            ) : (
              <>
                {classes.map((cls) => {
                  const resolved = resolveCachePrice(
                    cache.data,
                    orgCache,
                    cls,
                  )!;
                  return (
                    <div key={cls} className="rounded-lg border p-3 space-y-3">
                      <div className="flex flex-wrap items-center gap-2">
                        <span className="font-medium text-sm">
                          {organization ? cls : "All classes"}
                        </span>
                        <Badge variant="outline">
                          {resolved.own
                            ? `Bespoke · ${resolved.own.serving_class ?? "all classes"}`
                            : organization
                              ? "Inherited · general model"
                              : "General model"}
                        </Badge>
                      </div>
                      <dl className="grid grid-cols-2 sm:grid-cols-4 gap-3 text-sm">
                        {(
                          [
                            ["Read", resolved.values.read_multiplier],
                            ["Write 5m", resolved.values.write_multiplier_5m],
                            ["Write 1h", resolved.values.write_multiplier_1h],
                            ["Write 24h", resolved.values.write_multiplier_24h],
                          ] as const
                        ).map(([label, value]) => (
                          <div key={label}>
                            <dt className="text-xs text-muted-foreground">
                              {label}
                            </dt>
                            <dd>{value ?? "—"}×</dd>
                          </div>
                        ))}
                      </dl>
                    </div>
                  );
                })}
                <p className="text-xs text-muted-foreground">
                  Minimum prefix:{" "}
                  {cache.data.min_prefix_tokens?.toLocaleString() ?? "—"} tokens
                  · general model setting. Batch and flex use standard-class
                  multipliers.
                </p>
              </>
            )}
          </section>
          <section
            className="border-t pt-6 space-y-3"
            aria-label="Serving classes"
          >
            <h3 className="text-sm font-medium">Serving classes</h3>
            {Object.keys(model.serving_classes ?? {}).length ? (
              <div className="grid gap-3 sm:grid-cols-3">
                {Object.entries(model.serving_classes ?? {}).map(
                  ([name, preset]) => (
                    <div key={name} className="rounded-lg border p-3 text-sm">
                      <p className="font-medium">{name}</p>
                      <p className="text-xs text-muted-foreground mt-1">
                        TTFT {preset.ttft_ms} ms · ITL {preset.itl_ms} ms ·
                        priority {preset.priority ?? 0}
                      </p>
                    </div>
                  ),
                )}
              </div>
            ) : (
              <p className="text-sm text-muted-foreground">
                None offered (standard only).
              </p>
            )}
          </section>
          <section
            className="border-t pt-6 space-y-3"
            aria-label="Organisation overlays"
          >
            <h3 className="text-sm font-medium">Organisation overlays</h3>
            <p className="text-xs text-muted-foreground">
              Expand an organisation to inspect its serving settings and
              explicit price overrides.
            </p>
            {overlays.isLoading ? (
              <p role="status">Loading overlays…</p>
            ) : overlays.isError ? (
              <p role="alert">Could not load organisation overlays.</p>
            ) : !options.size ? (
              <p className="text-sm text-muted-foreground">
                No organisation overlays.
              </p>
            ) : (
              [...options].map(([id, name]) => (
                <ModelOverlay
                  key={id}
                  modelId={model.id}
                  organizationId={id}
                  name={name}
                  overlay={overlays.data?.find(
                    (row) => row.organization_id === id,
                  )}
                  onSelect={() => setSelected(id)}
                />
              ))
            )}
          </section>
        </>
      )}
    </>
  );
}

function PriceRow({
  row,
  label,
  source,
  note,
}: {
  row?: ModelTariff;
  label: string;
  source?: string;
  note?: string;
}) {
  return (
    <div className="rounded-lg border p-3 space-y-2">
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-sm font-medium">{label}</span>
        {source && <Badge variant="outline">{source}</Badge>}
        {note && <span className="text-xs text-muted-foreground">{note}</span>}
      </div>
      {row ? (
        <dl className="grid grid-cols-2 gap-4 text-sm">
          <div>
            <dt className="text-xs text-muted-foreground">Input / 1M tokens</dt>
            <dd>{formatTariffPrice(row.input_price_per_token)}</dd>
          </div>
          <div>
            <dt className="text-xs text-muted-foreground">
              Output / 1M tokens
            </dt>
            <dd>{formatTariffPrice(row.output_price_per_token)}</dd>
          </div>
        </dl>
      ) : (
        <p className="text-sm text-muted-foreground">
          Unpriced — no matching tariff is configured.
        </p>
      )}
    </div>
  );
}

function ModelOverlay({
  modelId,
  organizationId,
  name,
  overlay,
  onSelect,
}: {
  modelId: string;
  organizationId: string;
  name: string;
  overlay?: ServingOverlay;
  onSelect: () => void;
}) {
  const [open, setOpen] = useState(false);
  const serving = useOrganizationServing(organizationId, { enabled: open });
  return (
    <details
      className="rounded-lg border"
      onToggle={(event) => setOpen(event.currentTarget.open)}
    >
      <summary className="cursor-pointer p-3 text-sm font-medium">
        {name}{" "}
        <span className="font-normal text-muted-foreground">
          ·{" "}
          {overlay?.default_serving_class ??
            (overlay?.targets ? "custom targets" : "account defaults")}
        </span>
      </summary>
      {open && (
        <div className="border-t p-3 space-y-4">
          {serving.isLoading ? (
            <p role="status">Loading overlay…</p>
          ) : serving.isError || !serving.data ? (
            <p role="alert">
              Could not load overlay details.{" "}
              <button
                className="underline"
                onClick={() => void serving.refetch()}
              >
                Retry
              </button>
            </p>
          ) : (
            <OverlayDetails
              modelId={modelId}
              overlay={overlay}
              serving={serving.data}
            />
          )}
          <div className="flex items-center gap-4">
            <button className="text-sm underline" onClick={onSelect}>
              Show prices above
            </button>
            <Link
              className="text-sm underline"
              to={`/organizations/${organizationId}`}
            >
              Open organisation
            </Link>
          </div>
        </div>
      )}
    </details>
  );
}
