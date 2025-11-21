import React from "react";
import { useProbeResults } from "@/api/control-layer/hooks";
import { ProbeTimeline } from "../ModelInfo/ProbeTimeline";
import type { Endpoint, Model, Probe } from "@/api/control-layer/types";

// StatusRow component for status page layout
interface StatusRowProps {
  model: Model;
  probesData?: Probe[];
  endpointsRecord: Record<string, Endpoint>;
  onNavigate: (modelId: string) => void;
}

export const StatusRow: React.FC<StatusRowProps> = ({
  model,
  probesData,
  endpointsRecord,
  onNavigate,
}) => {
  const probe = probesData?.find((p) => p.deployment_id === model.id);
  const { data: probeResults } = useProbeResults(
    probe?.id || "",
    { limit: 100 },
    { enabled: !!probe },
  );

  const uptimePercentage = model.status?.uptime_percentage;
  const lastSuccess = model.status?.last_success;

  return (
    <div
      className="border-b border-gray-200 py-4 px-6 hover:bg-gray-50 transition-colors cursor-pointer"
      onClick={() => onNavigate(model.id)}
    >
      <div className="flex items-center gap-3">
        {/* Status indicator and model info */}
        <div className="w-48 shrink-0">
          <div className="flex items-center gap-2">
            <div
              className={`h-3 w-3 rounded-full ${
                lastSuccess === true
                  ? "bg-green-500 animate-pulse"
                  : lastSuccess === false
                    ? "bg-red-500 animate-pulse"
                    : "bg-gray-400"
              }`}
            />
            <div className="min-w-0">
              <div className="font-medium text-sm truncate break-all">
                {model.alias}
              </div>
              <div className="text-xs text-gray-500 truncate">
                {endpointsRecord[model.hosted_on]?.name || "Unknown"}
              </div>
            </div>
          </div>
          <div className="text-xs text-gray-600 mt-1 ml-5 space-y-0.5">
            {uptimePercentage !== undefined && uptimePercentage !== null && (
              <div>{uptimePercentage.toFixed(2)}% uptime (24h)</div>
            )}
            {probe && (
              <div className="text-gray-500">
                Checking every {probe.interval_seconds}s
              </div>
            )}
          </div>
        </div>

        {/* Timeline */}
        <div className="flex-1">
          {probeResults && probeResults.length > 0 ? (
            <ProbeTimeline
              results={probeResults}
              compact={true}
              showSummary={false}
              showTimeLabels={true}
              showLegend={false}
            />
          ) : (
            <div className="text-sm text-gray-400">No probe data available</div>
          )}
        </div>
      </div>
    </div>
  );
};
