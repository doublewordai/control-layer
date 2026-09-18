import { useOrganizationServing } from "@/api/control-layer/hooks";
import type {
  ModelTariff,
  OrganizationCacheTariff,
  ServingOverlay,
} from "@/api/control-layer/types";
import { Badge } from "@/components/ui/badge";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { getTariffDisplayName } from "@/utils/formatters";

interface OrganizationServingCardProps {
  organizationId: string;
}

/**
 * Platform-manager view of an organisation's serving deal: the account
 * settings (set through the organisation edit modal), the per-model overlays
 * and the organisation's own prices (both declared in the organisation
 * catalog and applied at startup; read-only here).
 */
export function OrganizationServingCard({
  organizationId,
}: OrganizationServingCardProps) {
  const { data, isLoading, isError } = useOrganizationServing(organizationId);

  if (isLoading) {
    return (
      <Card>
        <CardHeader>
          <CardTitle>Serving</CardTitle>
        </CardHeader>
        <CardContent>
          <div className="h-16 animate-pulse rounded bg-gray-100" />
        </CardContent>
      </Card>
    );
  }
  if (isError || !data) {
    return null;
  }

  const classes = data.granted_serving_classes;

  return (
    <Card>
      <CardHeader>
        <CardTitle>Serving</CardTitle>
        <CardDescription>
          Serving classes, routing preference and prices for this organisation.
          Overlays and prices come from the organisation catalog; settings are
          edited here.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-6">
        <dl className="grid grid-cols-1 gap-4 text-sm sm:grid-cols-3">
          <div>
            <dt className="text-xs text-gray-500">Granted classes</dt>
            <dd className="mt-1 flex flex-wrap gap-1">
              {classes.length === 0 ? (
                <span className="text-gray-500">none (standard only)</span>
              ) : (
                classes.map((c) => (
                  <Badge key={c} variant="outline">
                    {c}
                  </Badge>
                ))
              )}
            </dd>
          </div>
          <div>
            <dt className="text-xs text-gray-500">Default class</dt>
            <dd className="mt-1 font-medium">
              {data.default_serving_class ?? "standard"}
            </dd>
          </div>
          <div>
            <dt className="text-xs text-gray-500">Self-hosted only</dt>
            <dd className="mt-1 font-medium">
              {data.self_hosted_only ? "Yes" : "No"}
            </dd>
          </div>
        </dl>

        <OverlaysTable overlays={data.overlays} />
        <TariffsTable tariffs={data.tariffs} />
        <CacheTariffsTable rows={data.cache_tariffs} />
      </CardContent>
    </Card>
  );
}

function OverlaysTable({ overlays }: { overlays: ServingOverlay[] }) {
  return (
    <section>
      <h3 className="mb-2 text-sm font-medium text-gray-700">
        Per-model overlays
      </h3>
      {overlays.length === 0 ? (
        <p className="text-sm text-gray-500">No overlays.</p>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full text-sm">
            <thead className="text-left text-xs uppercase tracking-wide text-gray-500">
              <tr>
                <th className="py-1 pr-4">Model</th>
                <th className="py-1 pr-4">Default class</th>
                <th className="py-1 pr-4">Targets</th>
                <th className="py-1 pr-4">Self-hosted only</th>
                <th className="py-1">Source</th>
              </tr>
            </thead>
            <tbody className="divide-y">
              {overlays.map((o) => (
                <tr key={`${o.organization_id}-${o.deployed_model_id}`}>
                  <td className="py-1.5 pr-4 font-mono text-xs">{o.alias}</td>
                  <td className="py-1.5 pr-4">
                    {o.default_serving_class ?? "—"}
                  </td>
                  <td className="py-1.5 pr-4 tabular-nums">
                    {o.targets ? formatTargets(o.targets) : "—"}
                  </td>
                  <td className="py-1.5 pr-4">
                    {o.self_hosted_only === undefined
                      ? "—"
                      : o.self_hosted_only
                        ? "Yes"
                        : "No"}
                  </td>
                  <td className="py-1.5 text-xs text-gray-500">
                    {o.provisioning_source ?? "hand-written"}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

function TariffsTable({ tariffs }: { tariffs: ModelTariff[] }) {
  return (
    <section>
      <h3 className="mb-2 text-sm font-medium text-gray-700">
        Organisation prices
      </h3>
      {tariffs.length === 0 ? (
        <p className="text-sm text-gray-500">
          No organisation prices; general model prices apply.
        </p>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full text-sm">
            <thead className="text-left text-xs uppercase tracking-wide text-gray-500">
              <tr>
                <th className="py-1 pr-4">Model</th>
                <th className="py-1 pr-4">Tier</th>
                <th className="py-1 pr-4">Input / 1M</th>
                <th className="py-1 pr-4">Output / 1M</th>
                <th className="py-1">Since</th>
              </tr>
            </thead>
            <tbody className="divide-y">
              {tariffs.map((t) => (
                <tr key={t.id}>
                  <td className="py-1.5 pr-4 font-mono text-xs">
                    {t.deployed_model_id}
                  </td>
                  <td className="py-1.5 pr-4">
                    {getTariffDisplayName(
                      t.api_key_purpose,
                      t.completion_window,
                    )}
                  </td>
                  <td className="py-1.5 pr-4 tabular-nums">
                    ${(parseFloat(t.input_price_per_token) * 1_000_000).toFixed(2)}
                  </td>
                  <td className="py-1.5 pr-4 tabular-nums">
                    ${(parseFloat(t.output_price_per_token) * 1_000_000).toFixed(2)}
                  </td>
                  <td className="py-1.5 text-xs text-gray-500">
                    {new Date(t.valid_from).toLocaleDateString()}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

function CacheTariffsTable({ rows }: { rows: OrganizationCacheTariff[] }) {
  if (rows.length === 0) {
    return null;
  }
  return (
    <section>
      <h3 className="mb-2 text-sm font-medium text-gray-700">
        Organisation cache multipliers
      </h3>
      <div className="overflow-x-auto">
        <table className="w-full text-sm">
          <thead className="text-left text-xs uppercase tracking-wide text-gray-500">
            <tr>
              <th className="py-1 pr-4">Model</th>
              <th className="py-1 pr-4">Read</th>
              <th className="py-1 pr-4">Write 5m</th>
              <th className="py-1 pr-4">Write 1h</th>
              <th className="py-1 pr-4">Write 24h</th>
              <th className="py-1">Min prefix</th>
            </tr>
          </thead>
          <tbody className="divide-y">
            {rows.map((r) => (
              <tr key={r.deployed_model_id}>
                <td className="py-1.5 pr-4 font-mono text-xs">{r.alias}</td>
                <td className="py-1.5 pr-4 tabular-nums">{r.read_multiplier}×</td>
                <td className="py-1.5 pr-4 tabular-nums">{r.write_multiplier_5m}×</td>
                <td className="py-1.5 pr-4 tabular-nums">{r.write_multiplier_1h}×</td>
                <td className="py-1.5 pr-4 tabular-nums">{r.write_multiplier_24h}×</td>
                <td className="py-1.5 tabular-nums">{r.min_prefix_tokens}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

function formatTargets(t: {
  ttft_ms: number;
  itl_ms: number;
  priority: number;
}): string {
  const priority = t.priority ? `, priority ${t.priority}` : "";
  return `TTFT ${t.ttft_ms} ms, ITL ${t.itl_ms} ms${priority}`;
}
