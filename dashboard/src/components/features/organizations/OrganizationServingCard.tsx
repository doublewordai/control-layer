import { useState } from "react";
import { useOrganizationServing } from "@/api/control-layer/hooks";
import type { OrganizationServing } from "@/api/control-layer/types";
import { OverlayDetails } from "../serving/OverlayDetails";
import { Badge } from "@/components/ui/badge";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";

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
    return (
      <p role="alert">Could not load organisation serving configuration.</p>
    );
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

        <section className="space-y-3">
          <h3 className="font-medium text-sm">Model overlays</h3>
          <p className="text-sm text-muted-foreground">
            Serving settings and price overrides are grouped by model. Expand a
            model to inspect its configuration.
          </p>
          {[
            ...new Set([
              ...data.overlays.map((row) => row.deployed_model_id),
              ...data.tariffs.map((row) => row.deployed_model_id),
              ...data.cache_tariffs.map((row) => row.deployed_model_id),
            ]),
          ].map((modelId) => (
            <OrganizationModelOverlay
              key={modelId}
              modelId={modelId}
              serving={data}
            />
          ))}
          {!data.overlays.length &&
            !data.tariffs.length &&
            !data.cache_tariffs.length && (
              <p className="text-sm text-muted-foreground">
                No model overrides; account settings and general model prices
                apply.
              </p>
            )}
        </section>
      </CardContent>
    </Card>
  );
}

function OrganizationModelOverlay({
  modelId,
  serving,
}: {
  modelId: string;
  serving: OrganizationServing;
}) {
  const [open, setOpen] = useState(false);
  const overlay = serving.overlays.find(
    (row) => row.deployed_model_id === modelId,
  );
  const alias =
    overlay?.alias ??
    serving.cache_tariffs.find((row) => row.deployed_model_id === modelId)
      ?.alias ??
    serving.model_aliases?.[modelId];
  const tokenCount = serving.tariffs.filter(
    (row) => row.deployed_model_id === modelId,
  ).length;
  const cacheCount = serving.cache_tariffs.filter(
    (row) => row.deployed_model_id === modelId,
  ).length;
  return (
    <details
      className="rounded-lg border"
      onToggle={(event) => setOpen(event.currentTarget.open)}
    >
      <summary className="cursor-pointer p-3 text-sm">
        <span className="font-medium">
          {alias ?? modelId}
        </span>
        <span className="text-muted-foreground ml-2">
          {tokenCount} token price overrides · {cacheCount} cache overrides
        </span>
      </summary>
      {open && (
        <div className="border-t p-3">
          <OverlayDetails
            modelId={modelId}
            overlay={overlay}
            serving={serving}
          />
        </div>
      )}
    </details>
  );
}
