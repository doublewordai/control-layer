import { Link } from "react-router-dom";
import type {
  OrganizationServing,
  ServingOverlay,
} from "@/api/control-layer/types";
import { formatTariffPrice, getTariffDisplayName } from "@/utils/formatters";

export function OverlayDetails({
  modelId,
  overlay,
  serving,
}: {
  modelId: string;
  overlay?: ServingOverlay;
  serving: OrganizationServing;
}) {
  const tariffs = serving.tariffs.filter(
    (row) => row.deployed_model_id === modelId,
  );
  const cache = serving.cache_tariffs.filter(
    (row) => row.deployed_model_id === modelId,
  );
  return (
    <div className="space-y-4 text-sm">
      <dl className="grid gap-3 sm:grid-cols-2">
        <div>
          <dt className="text-muted-foreground">Default class</dt>
          <dd>
            {overlay?.targets
              ? "Custom targets"
              : overlay?.default_serving_class
                ? `${overlay.default_serving_class} · model override`
                : `Inherit account · ${serving.default_serving_class ?? "standard"}`}
          </dd>
        </div>
        <div>
          <dt className="text-muted-foreground">Self-hosted only</dt>
          <dd>
            {overlay?.self_hosted_only != null
              ? `${overlay.self_hosted_only ? "Yes" : "No"} · model override`
              : `Inherit account · ${serving.self_hosted_only ? "Yes" : "No"}`}
          </dd>
        </div>
        <div>
          <dt className="text-muted-foreground">Account class grants</dt>
          <dd>
            {serving.granted_serving_classes.join(", ") || "Standard only"}
          </dd>
        </div>
        <div>
          <dt className="text-muted-foreground">Configuration source</dt>
          <dd className="break-all">
            {overlay?.provisioning_source ?? "Not catalog managed"}
          </dd>
        </div>
        {overlay?.targets && (
          <div>
            <dt className="text-muted-foreground">Explicit targets</dt>
            <dd>
              TTFT {overlay.targets.ttft_ms} ms · ITL {overlay.targets.itl_ms}{" "}
              ms · priority {overlay.targets.priority ?? 0}
            </dd>
          </div>
        )}
      </dl>
      <section>
        <h4 className="font-medium mb-2">Token price overrides</h4>
        {tariffs.length ? (
          <div className="overflow-x-auto">
            <table className="w-full text-left text-sm">
              <thead className="text-xs text-muted-foreground">
                <tr>
                  <th>Class</th>
                  <th>Tier</th>
                  <th>Input / 1M</th>
                  <th>Output / 1M</th>
                </tr>
              </thead>
              <tbody>
                {tariffs.map((row) => (
                  <tr key={row.id} className="border-t">
                    <td className="py-2 pr-3">
                      {row.serving_class ?? "All classes"}
                    </td>
                    <td className="pr-3">
                      {getTariffDisplayName(
                        row.api_key_purpose,
                        row.completion_window,
                      )}
                      {row.completion_window && ` · ${row.completion_window}`}
                    </td>
                    <td>{formatTariffPrice(row.input_price_per_token)}</td>
                    <td>{formatTariffPrice(row.output_price_per_token)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : (
          <p className="text-muted-foreground">
            No token price overrides; general model prices apply.
          </p>
        )}
      </section>
      <section>
        <h4 className="font-medium mb-2">Cache multiplier overrides</h4>
        {cache.length ? (
          <div className="overflow-x-auto">
            <table className="w-full text-left text-sm">
              <thead className="text-xs text-muted-foreground">
                <tr>
                  <th>Class</th>
                  <th>Read</th>
                  <th>Write 5m</th>
                  <th>Write 1h</th>
                  <th>Write 24h</th>
                </tr>
              </thead>
              <tbody>
                {cache.map((row) => (
                  <tr key={row.serving_class ?? "all"} className="border-t">
                    <td className="py-2 pr-3">
                      {row.serving_class ?? "All classes"}
                    </td>
                    <td>{row.read_multiplier}×</td>
                    <td>{row.write_multiplier_5m}×</td>
                    <td>{row.write_multiplier_1h}×</td>
                    <td>{row.write_multiplier_24h}×</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : (
          <p className="text-muted-foreground">
            No cache overrides; general model multipliers apply.
          </p>
        )}
      </section>
      <p className="text-xs text-muted-foreground">
        Only explicit overrides are listed here. Missing tiers inherit the next
        matching price scope; cache pricing must be enabled on the model.
      </p>
      <Link
        className="text-sm underline"
        to={`/models/manage/${modelId}?pricing_org=${serving.organization_id}`}
      >
        View effective model pricing
      </Link>
    </div>
  );
}
