import { useSearchParams } from "react-router-dom";
import {
  Server,
  Settings as SettingsIcon,
  Database,
  AlertCircle,
  Check,
  Activity,
  Cpu,
} from "lucide-react";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "../../../ui/tabs";
import { DataTable } from "../../../ui/data-table";
import { useDaemons } from "../../../../api/control-layer/hooks";
import { Badge } from "../../../ui/badge";
import { Tooltip, TooltipTrigger, TooltipContent } from "../../../ui/tooltip";
import { useSettings } from "../../../../contexts";
import { useAuthorization } from "../../../../utils";
import { useState } from "react";
import { Switch } from "../../../ui/switch";
import { Button } from "../../../ui/button";
import type { Daemon, DaemonStatus } from "../../../../api/control-layer/types";
import type { ColumnDef } from "@tanstack/react-table";

// Helper to format timestamps
function formatTimestamp(timestamp: number): string {
  const date = new Date(timestamp * 1000);
  return date.toLocaleString();
}

// Helper to get relative time (e.g., "2 minutes ago")
function getRelativeTime(timestamp: number): string {
  const now = Date.now();
  const diff = now - timestamp * 1000;
  const seconds = Math.floor(diff / 1000);
  const minutes = Math.floor(seconds / 60);
  const hours = Math.floor(minutes / 60);
  const days = Math.floor(hours / 24);

  if (days > 0) return `${days}d ago`;
  if (hours > 0) return `${hours}h ago`;
  if (minutes > 0) return `${minutes}m ago`;
  return `${seconds}s ago`;
}

// Helper to determine effective status based on heartbeat staleness
function getEffectiveStatus(daemon: Daemon): {
  status: DaemonStatus | "unknown";
  isStale: boolean;
} {
  const heartbeatIntervalMs = daemon.config.heartbeat_interval_ms;

  // If heartbeat interval is NaN or invalid, mark as unknown
  if (isNaN(heartbeatIntervalMs) || heartbeatIntervalMs <= 0) {
    return { status: "unknown", isStale: true };
  }

  // Only check staleness for running daemons
  if (daemon.status === "running" && daemon.last_heartbeat) {
    const now = Date.now();
    const lastHeartbeatMs = daemon.last_heartbeat * 1000;
    const timeSinceLastHeartbeat = now - lastHeartbeatMs;
    const threshold = heartbeatIntervalMs * 2;

    // If last heartbeat is older than 2x the interval, mark as unknown
    if (timeSinceLastHeartbeat > threshold) {
      return { status: "unknown", isStale: true };
    }
  }

  return { status: daemon.status, isStale: false };
}

// Helper to get status badge variant
function getStatusBadgeVariant(
  status: DaemonStatus | "unknown",
): "default" | "secondary" | "destructive" | "outline" {
  switch (status) {
    case "running":
      return "default";
    case "initializing":
      return "secondary";
    case "dead":
      return "destructive";
    case "unknown":
      return "outline";
  }
}

// Column definitions for the daemons table
const createDaemonColumns = (): ColumnDef<Daemon>[] => [
  {
    accessorKey: "id",
    header: "ID",
    cell: ({ row }) => (
      <span className="font-mono text-xs">{row.original.id.slice(0, 8)}</span>
    ),
  },
  {
    accessorKey: "status",
    header: "Status",
    cell: ({ row }) => {
      const { status, isStale } = getEffectiveStatus(row.original);

      const badge = (
        <Badge variant={getStatusBadgeVariant(status)}>
          {status.charAt(0).toUpperCase() + status.slice(1)}
        </Badge>
      );

      if (isStale) {
        const intervalMs = row.original.config.heartbeat_interval_ms;
        const intervalSecs = Math.floor(intervalMs / 1000);
        return (
          <Tooltip>
            <TooltipTrigger asChild>{badge}</TooltipTrigger>
            <TooltipContent>last seen &gt;{intervalSecs}s</TooltipContent>
          </Tooltip>
        );
      }

      return badge;
    },
  },
  {
    accessorKey: "hostname",
    header: "Hostname",
  },
  {
    accessorKey: "pid",
    header: "PID",
  },
  {
    accessorKey: "version",
    header: "Version",
  },
  {
    accessorKey: "config.heartbeat_interval_ms",
    header: "Heartbeat Interval",
    cell: ({ row }) => {
      const intervalMs = row.original.config.heartbeat_interval_ms;
      const intervalSecs = intervalMs / 1000;
      return <span>{intervalSecs}s</span>;
    },
  },
  {
    accessorKey: "started_at",
    header: "Started",
    cell: ({ row }) => (
      <div className="flex flex-col">
        <span className="text-sm">
          {formatTimestamp(row.original.started_at)}
        </span>
        <span className="text-xs text-gray-500">
          {getRelativeTime(row.original.started_at)}
        </span>
      </div>
    ),
  },
  {
    accessorKey: "last_heartbeat",
    header: "Last Heartbeat",
    cell: ({ row }) => {
      const heartbeat = row.original.last_heartbeat;
      if (!heartbeat) return <span className="text-gray-400">-</span>;
      return (
        <div className="flex flex-col">
          <span className="text-sm">{formatTimestamp(heartbeat)}</span>
          <span className="text-xs text-gray-500">
            {getRelativeTime(heartbeat)}
          </span>
        </div>
      );
    },
  },
  {
    accessorKey: "stopped_at",
    header: "Stopped",
    cell: ({ row }) => {
      const stopped = row.original.stopped_at;
      if (!stopped) return <span className="text-gray-400">-</span>;
      return (
        <div className="flex flex-col">
          <span className="text-sm">{formatTimestamp(stopped)}</span>
          <span className="text-xs text-gray-500">
            {getRelativeTime(stopped)}
          </span>
        </div>
      );
    },
  },
  {
    accessorKey: "stats.requests_processed",
    header: "Requests Processed",
    cell: ({ row }) => row.original.stats.requests_processed.toLocaleString(),
  },
  {
    accessorKey: "stats.requests_failed",
    header: "Requests Failed",
    cell: ({ row }) => row.original.stats.requests_failed.toLocaleString(),
  },
  {
    accessorKey: "stats.requests_in_flight",
    header: "In Flight",
    cell: ({ row }) => row.original.stats.requests_in_flight.toLocaleString(),
  },
];

export function System() {
  const [searchParams, setSearchParams] = useSearchParams();

  // Read active tab from URL or default to "settings"
  const activeTab = searchParams.get("tab") || "settings";

  // Fetch daemons data
  const { data: daemonsData, isLoading: daemonsLoading } = useDaemons();

  // Settings state
  const { toggleFeature, isFeatureEnabled, updateDemoConfig, settings } =
    useSettings();
  const { hasPermission } = useAuthorization();
  const canAccessSettings = hasPermission("settings");
  const [demoResponse, setDemoResponse] = useState(
    settings.demoConfig?.customResponse || "",
  );
  const [useCustomResponse, setUseCustomResponse] = useState(
    !!settings.demoConfig?.customResponse,
  );
  const [savedResponse, setSavedResponse] = useState(
    settings.demoConfig?.customResponse || "",
  );

  const hasUnsavedChanges = demoResponse !== savedResponse;

  const handleSave = () => {
    updateDemoConfig({ customResponse: useCustomResponse ? demoResponse : "" });
    setSavedResponse(demoResponse);
  };

  const handleResponseChange = (value: string) => {
    setDemoResponse(value);
  };

  const daemons = daemonsData?.daemons || [];

  // Create columns
  const daemonColumns = createDaemonColumns();

  // Update URL when tab changes
  const updateURL = (tab: string) => {
    const params = new URLSearchParams(searchParams);
    params.set("tab", tab);
    setSearchParams(params, { replace: false });
  };

  return (
    <div className="py-4 px-6">
      <Tabs value={activeTab} onValueChange={(v) => updateURL(v)}>
        {/* Header with Tabs */}
        <div className="mb-6 flex flex-wrap items-center justify-between gap-4">
          {/* Left: Title */}
          <div className="shrink-0">
            <h1 className="text-3xl font-bold text-doubleword-neutral-900">
              System
            </h1>
            <p className="text-doubleword-neutral-600 mt-1">
              System configuration and infrastructure monitoring
            </p>
          </div>

          {/* Right: Tabs */}
          <div className="shrink-0">
            <TabsList>
              <TabsTrigger value="settings" className="flex items-center gap-2">
                <SettingsIcon className="w-4 h-4" />
                Settings
              </TabsTrigger>
              <TabsTrigger value="status" className="flex items-center gap-2">
                <Activity className="w-4 h-4" />
                Status
              </TabsTrigger>
            </TabsList>
          </div>
        </div>

        {/* Content */}
        <div className="space-y-4">
          {/* Status Tab */}
          <TabsContent value="status" className="space-y-4">
            {/* Daemons Section */}
            <div className="bg-white rounded-lg border border-doubleword-neutral-200 p-6">
              <div className="flex items-center gap-2 mb-4">
                <Cpu className="w-5 h-5 text-gray-900" />
                <h2 className="text-lg font-semibold text-doubleword-neutral-900">
                  Batch Daemons ({daemons.length})
                </h2>
              </div>
              {daemonsLoading ? (
                <div className="flex items-center justify-center h-32">
                  <div className="text-center">
                    <div
                      className="animate-spin rounded-full h-8 w-8 border-b-2 border-doubleword-accent-blue mx-auto mb-2"
                      aria-label="Loading"
                    ></div>
                    <p className="text-sm text-doubleword-neutral-600">
                      Loading daemons...
                    </p>
                  </div>
                </div>
              ) : daemons.length === 0 ? (
                <div className="text-center py-8">
                  <div className="p-3 bg-doubleword-neutral-100 rounded-full w-12 h-12 mx-auto mb-3 flex items-center justify-center">
                    <Server className="w-6 h-6 text-doubleword-neutral-600" />
                  </div>
                  <h3 className="text-sm font-medium text-doubleword-neutral-900 mb-1">
                    No daemons found
                  </h3>
                  <p className="text-sm text-doubleword-neutral-600">
                    No daemons are currently registered in the system
                  </p>
                </div>
              ) : (
                <DataTable
                  columns={daemonColumns}
                  data={daemons}
                  searchPlaceholder="Search daemons..."
                  showColumnToggle={true}
                  initialColumnVisibility={{ id: false }}
                />
              )}
            </div>
          </TabsContent>

          {/* Settings Tab */}
          <TabsContent value="settings" className="space-y-4">
            {canAccessSettings ? (
              <div className="bg-white rounded-lg border border-doubleword-neutral-200">
                {/* Demo Mode Section */}
                <div className="p-6">
                  <div className="flex items-center gap-2 mb-4">
                    {isFeatureEnabled("demo") ? (
                      <Database className="w-5 h-5 text-blue-600" />
                    ) : (
                      <Server className="w-5 h-5 text-gray-600" />
                    )}
                    <h2 className="text-lg font-semibold text-doubleword-neutral-900">
                      Demo Mode
                    </h2>
                  </div>

                  <div className="space-y-6">
                    <div className="flex items-center justify-between">
                      <div className="flex-1">
                        <h3 className="text-sm font-medium text-doubleword-neutral-900">
                          Enable Demo Mode
                        </h3>
                        <p className="text-sm text-doubleword-neutral-600 mt-1">
                          Use mock data for demonstration purposes. Toggle off
                          to connect to the live API.
                        </p>
                      </div>
                      <Switch
                        checked={isFeatureEnabled("demo")}
                        onCheckedChange={(checked) =>
                          toggleFeature("demo", checked)
                        }
                        aria-label="Toggle demo mode"
                      />
                    </div>

                    <div className="flex items-start gap-3 p-4 bg-amber-50 border border-amber-200 rounded-lg">
                      <AlertCircle className="w-5 h-5 text-amber-600 mt-0.5 shrink-0" />
                      <div className="flex-1">
                        <p className="text-sm text-amber-800">
                          <strong>Note:</strong> Changing this setting will
                          reload the page to apply the new configuration.
                        </p>
                      </div>
                    </div>

                    {/* Custom Response Configuration - only show when demo mode is enabled */}
                    {isFeatureEnabled("demo") && (
                      <div className="pt-6 mt-6 border-t border-doubleword-neutral-200">
                        <div className="flex items-center justify-between mb-4">
                          <div className="flex-1">
                            <h3 className="text-sm font-medium text-doubleword-neutral-900">
                              Custom Response Template
                            </h3>
                            <p className="text-sm text-doubleword-neutral-600 mt-1">
                              Override the default playground response with a
                              custom template.
                            </p>
                          </div>
                          <Switch
                            checked={useCustomResponse}
                            onCheckedChange={setUseCustomResponse}
                            aria-label="Toggle custom response template"
                          />
                        </div>

                        {useCustomResponse && (
                          <div className="space-y-3 mt-4">
                            <textarea
                              id="demo-response"
                              value={demoResponse}
                              onChange={(e) =>
                                handleResponseChange(e.target.value)
                              }
                              placeholder="Enter your custom demo response... Use {userMessage} to include the user input."
                              className="w-full px-3 py-2 border border-doubleword-neutral-300 rounded-md text-sm focus:outline-none focus:ring-2 focus:ring-blue-500 focus:border-blue-500 font-mono"
                              rows={4}
                            />
                            <p className="text-xs text-doubleword-neutral-600">
                              Use{" "}
                              <code className="px-1 py-0.5 bg-gray-100 rounded text-xs">
                                {"{userMessage}"}
                              </code>{" "}
                              as a placeholder to include the user's message in
                              the response.
                            </p>

                            <div className="flex justify-end">
                              <Button
                                onClick={handleSave}
                                variant="default"
                                size="sm"
                                disabled={!hasUnsavedChanges}
                              >
                                {hasUnsavedChanges ? (
                                  "Save Template"
                                ) : (
                                  <>
                                    <Check className="w-4 h-4" />
                                    Saved
                                  </>
                                )}
                              </Button>
                            </div>
                          </div>
                        )}
                      </div>
                    )}
                  </div>
                </div>
              </div>
            ) : (
              <div className="bg-gray-50 border border-gray-200 rounded-lg p-6">
                <h2 className="text-lg font-semibold text-gray-900 mb-2">
                  Settings Access Restricted
                </h2>
                <p className="text-sm text-gray-600">
                  Settings are only available to users with the appropriate
                  permissions. Contact your admin if you need access to
                  configuration options.
                </p>
              </div>
            )}
          </TabsContent>
        </div>
      </Tabs>
    </div>
  );
}
