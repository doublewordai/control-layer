import React, { useState, useMemo } from "react";
import { useNavigate } from "react-router-dom";
import {
  Users,
  X,
  ArrowRight,
  Code,
  Plus,
  Search,
  Clock,
  Activity,
  BarChart3,
  ArrowUpDown,
  Info,
  DollarSign,
  ArrowDownToLine,
  ArrowUpToLine,
  GitMerge,
} from "lucide-react";
import {
  useModels,
  useModelsMetrics,
  type Model,
  type ModelsInclude,
  useProbes,
} from "../../../../api/control-layer";
import { AccessManagementModal } from "../../../modals";
import { ApiExamples } from "../../../modals";
import { UpdateModelPricingModal } from "../../../modals";
import { TablePagination } from "../../../ui/table-pagination";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "../../../ui/card";
import { Badge } from "../../../ui/badge";
import { Button } from "../../../ui/button";
import {
  HoverCard,
  HoverCardContent,
  HoverCardTrigger,
} from "../../../ui/hover-card";
import { Sparkline } from "../../../ui/sparkline";
import {
  formatNumber,
  formatLatency,
  formatRelativeTime,
} from "../../../../utils/formatters";
import { Skeleton } from "../../../ui/skeleton";
import { StatusRow } from "./StatusRow";
import { Markdown } from "../../../ui/markdown";

export interface ModelsContentProps {
  pagination: ReturnType<
    typeof import("../../../../hooks/useServerPagination").useServerPagination
  >;
  searchQuery: string;
  filterProvider: string;
  filterModelType: "all" | "virtual" | "hosted";
  endpointId?: string;
  groupId?: string;
  showAccessibleOnly: boolean;
  isStatusMode: boolean;
  canManageGroups: boolean;
  canViewAnalytics: boolean;
  canViewEndpoints: boolean;
  showPricing: boolean;
  canManageModels: boolean;
  onClearFilters: () => void;
}

export const ModelsContent: React.FC<ModelsContentProps> = ({
  pagination,
  searchQuery,
  filterProvider,
  filterModelType,
  endpointId,
  groupId,
  showAccessibleOnly,
  isStatusMode,
  canManageGroups,
  canViewAnalytics,
  canViewEndpoints,
  showPricing,
  canManageModels,
  onClearFilters,
}) => {
  const navigate = useNavigate();
  const [showAccessModal, setShowAccessModal] = useState(false);
  const [accessModel, setAccessModel] = useState<Model | null>(null);
  const [showApiExamples, setShowApiExamples] = useState(false);
  const [apiExamplesModel, setApiExamplesModel] = useState<Model | null>(null);
  const [showPricingModal, setShowPricingModal] = useState(false);
  const [pricingModel, setPricingModel] = useState<Model | null>(null);

  const includeParam = useMemo(() => {
    const parts: string[] = ["status", "components"];
    if (canViewEndpoints) parts.push("endpoints");
    if (canManageGroups) parts.push("groups");
    // metrics loaded separately via useModelsMetrics for lazy background loading
    if (showPricing) parts.push("pricing");
    return parts.join(",");
  }, [canViewEndpoints, canManageGroups, showPricing]);

  // Convert filterModelType to is_composite API parameter
  const isCompositeFilter = filterModelType === "all"
    ? undefined
    : filterModelType === "virtual";

  const {
    data: rawModelsData,
    isLoading: modelsLoading,
    error: modelsError,
  } = useModels({
    skip: pagination.queryParams.skip,
    limit: pagination.queryParams.limit,
    include: includeParam as ModelsInclude,
    accessible: isStatusMode ? true : !canManageGroups || showAccessibleOnly,
    search: searchQuery || undefined,
    endpoint: endpointId,
    group: groupId,
    is_composite: isCompositeFilter,
  });

  // Load metrics lazily in the background so model cards render immediately
  const { metricsMap, isLoading: metricsLoading } = useModelsMetrics({
    skip: pagination.queryParams.skip,
    limit: pagination.queryParams.limit,
    accessible: isStatusMode ? true : !canManageGroups || showAccessibleOnly,
    search: searchQuery || undefined,
    endpoint: endpointId,
    group: groupId,
    is_composite: isCompositeFilter,
    enabled: canViewAnalytics,
  });

  const { data: probesData } = useProbes();

  // Merge lazily-loaded metrics into models
  const models = useMemo(() => {
    const raw = rawModelsData?.data || [];
    if (!canViewAnalytics || metricsMap.size === 0) return raw;
    return raw.map((model) => {
      const metrics = metricsMap.get(model.id);
      return metrics ? { ...model, metrics } : model;
    });
  }, [rawModelsData?.data, metricsMap, canViewAnalytics]);

  const loading = modelsLoading;
  const error = modelsError ? (modelsError as Error).message : null;

  // Filter models for status mode (only show models with probes)
  // Note: Model type filtering is now handled server-side via is_composite parameter
  const filteredModels = isStatusMode
    ? models.filter((model) => model.status?.probe_id)
    : models;

  const hasNoModels =
    (rawModelsData?.total_count || 0) === 0 && pagination.page === 1;
  const hasNoFilteredResults = !hasNoModels && filteredModels.length === 0;

  if (loading) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center">
          <div
            className="animate-spin rounded-full h-12 w-12 border-b-2 border-doubleword-accent-blue mx-auto mb-4"
            aria-label="Loading"
          ></div>
          <p className="text-doubleword-neutral-600">
            Loading model usage data...
          </p>
        </div>
      </div>
    );
  }

  if (error) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center">
          <div className="text-red-500 mb-4">
            <X className="h-12 w-12 mx-auto" />
          </div>
          <p className="text-red-600 font-semibold">Error: {error}</p>
        </div>
      </div>
    );
  }

  if (hasNoModels) {
    return (
      <div className="text-center py-16">
        <div className="p-4 bg-doubleword-neutral-100 rounded-full w-20 h-20 mx-auto mb-6 flex items-center justify-center">
          <BarChart3 className="w-10 h-10 text-doubleword-neutral-600" />
        </div>
        <h3 className="text-xl font-semibold text-doubleword-neutral-900 mb-3">
          {isStatusMode
            ? "No models being monitored"
            : "No models available yet"}
        </h3>
        <p className="text-doubleword-neutral-600 mb-8 max-w-l mx-auto">
          {isStatusMode
            ? "No models have monitoring configured. Toggle to Grid view to set up probes."
            : "Models are automatically synced when you add an inference endpoint. Add an endpoint to start interacting with AI models through the control layer."}
        </p>
        {!isStatusMode && (
          <Button
            onClick={() =>
              navigate("/endpoints", { state: { openCreateModal: true } })
            }
            className="bg-doubleword-background-dark hover:bg-doubleword-neutral-900"
          >
            <Plus className="w-4 h-4 mr-2" />
            Add Endpoint
          </Button>
        )}
      </div>
    );
  }

  return (
    <>
      {isStatusMode ? (
        hasNoFilteredResults ? (
          <div className="text-center py-16">
            <div className="p-4 bg-doubleword-neutral-100 rounded-full w-16 h-16 mx-auto mb-4 flex items-center justify-center">
              <Search className="w-8 h-8 text-doubleword-neutral-600" />
            </div>
            <h3 className="text-lg font-medium text-doubleword-neutral-900 mb-2">
              No monitored models found
            </h3>
            <p className="text-doubleword-neutral-600 mb-6">
              {searchQuery
                ? `No models match "${searchQuery}"`
                : filterProvider !== "all"
                  ? `No models found for ${filterProvider}`
                  : "Try adjusting your filters"}
            </p>
            <Button variant="outline" onClick={onClearFilters}>
              Clear filters
            </Button>
          </div>
        ) : (
          <div>
            {filteredModels.map((model) => (
              <StatusRow
                key={model.id}
                model={model}
                probesData={probesData}
                onNavigate={(modelId: string) =>
                  navigate(
                    `/models/${modelId}?from=${encodeURIComponent("/models?view=status")}`,
                  )
                }
              />
            ))}
          </div>
        )
      ) : hasNoFilteredResults ? (
        <div className="text-center py-16">
          <div className="p-4 bg-doubleword-neutral-100 rounded-full w-16 h-16 mx-auto mb-4 flex items-center justify-center">
            <Search className="w-8 h-8 text-doubleword-neutral-600" />
          </div>
          <h3 className="text-lg font-medium text-doubleword-neutral-900 mb-2">
            No models found
          </h3>
          <p className="text-doubleword-neutral-600 mb-6">
            {searchQuery
              ? `No models match "${searchQuery}"`
              : filterProvider !== "all"
                ? `No models found for ${filterProvider}`
                : "Try adjusting your filters"}
          </p>
          <Button variant="outline" onClick={onClearFilters}>
            Clear filters
          </Button>
        </div>
      ) : (
        <>
          <div
            role="list"
            className="grid grid-cols-1 lg:grid-cols-2 2xl:grid-cols-3 gap-6"
          >
            {filteredModels.map((model) => {
              return (
                <Card
                  key={model.id}
                  role="listitem"
                  className="hover:shadow-md transition-shadow rounded-lg p-0 gap-0 overflow-hidden flex flex-col"
                >
                  <div
                    className="cursor-pointer hover:bg-gray-50 transition-colors group grow flex flex-col"
                    onClick={() => {
                      navigate(
                        `/models/${model.id}?from=${encodeURIComponent("/models")}`,
                      );
                    }}
                  >
                    <CardHeader className="px-6 pt-5 pb-0">
                      <div
                        className={canViewEndpoints ? "space-y-0" : "space-y-2"}
                      >
                        {/* ROW 1: Alias on left, groups/chevron on right */}
                        <div className="flex items-center justify-between gap-1">
                          <div className="flex items-center gap-2">
                            {model.alias.length > 30 ? (
                              <HoverCard openDelay={200} closeDelay={100}>
                                <HoverCardTrigger asChild>
                                  <CardTitle
                                    className={`text-lg truncate ${canManageGroups ? "max-w-[460px] md:max-w-[420px] lg:max-w-[270px] xl:max-w-[360px] 2xl:max-w-[230px] 3xl:max-w-[300px] 4xl:max-w-[360px] 5xl:max-w-[420px]" : "max-w-[460px] md:max-w-[500px] lg:max-w-[320px] xl:max-w-[480px] 2xl:max-w-[360px] 3xl:max-w-[450px] 4xl:max-w-[520px] 5xl:max-w-[600px]"} break-all hover:opacity-70 transition-opacity cursor-default`}
                                  >
                                    {model.alias}
                                  </CardTitle>
                                </HoverCardTrigger>
                                <HoverCardContent
                                  className="w-auto max-w-sm"
                                  sideOffset={5}
                                >
                                  <p className="text-sm break-all">
                                    {model.alias}
                                  </p>
                                </HoverCardContent>
                              </HoverCard>
                            ) : (
                              <CardTitle
                                className={`text-lg truncate ${canManageGroups ? "max-w-[460px] md:max-w-[420px] lg:max-w-[270px] xl:max-w-[360px] 2xl:max-w-[230px] 3xl:max-w-[300px] 4xl:max-w-[360px] 5xl:max-w-[420px]" : "max-w-[460px] md:max-w-[500px] lg:max-w-[320px] xl:max-w-[480px] 2xl:max-w-[360px] 3xl:max-w-[450px] 4xl:max-w-[520px] 5xl:max-w-[600px]"} break-all`}
                              >
                                {model.alias}
                              </CardTitle>
                            )}

                            {canManageGroups && model.is_composite && (
                              <HoverCard openDelay={200} closeDelay={100}>
                                <HoverCardTrigger asChild>
                                  <Badge
                                    variant="outline"
                                    className="text-xs gap-1 px-1.5 py-0.5 text-gray-600 border-gray-300 hover:bg-gray-50 cursor-default"
                                    onClick={(e) => e.stopPropagation()}
                                  >
                                    <GitMerge className="h-3 w-3" />
                                    <span>Virtual</span>
                                  </Badge>
                                </HoverCardTrigger>
                                <HoverCardContent
                                  className="w-56"
                                  sideOffset={5}
                                >
                                  <div className="space-y-2">
                                    <p className="text-sm font-medium">
                                      Virtual Model
                                    </p>
                                    <p className="text-xs text-muted-foreground">
                                      Routes requests across multiple hosted
                                      models with{" "}
                                      {model.lb_strategy === "priority"
                                        ? "priority-based failover"
                                        : "weighted load balancing"}
                                      .
                                    </p>
                                    {model.components &&
                                      model.components.length > 0 && (
                                        <p className="text-xs text-muted-foreground">
                                          {model.components.length} hosted model
                                          {model.components.length !== 1
                                            ? "s"
                                            : ""}{" "}
                                          configured
                                        </p>
                                      )}
                                  </div>
                                </HoverCardContent>
                              </HoverCard>
                            )}

                            {model.status?.probe_id && (
                              <HoverCard openDelay={200} closeDelay={100}>
                                <HoverCardTrigger asChild>
                                  <div
                                    className={`h-2 w-2 rounded-full ${
                                      model.status.last_success === true
                                        ? "bg-green-500 animate-pulse"
                                        : model.status.last_success === false
                                          ? "bg-red-500 animate-pulse"
                                          : "bg-gray-400"
                                    }`}
                                    onClick={(e) => e.stopPropagation()}
                                  />
                                </HoverCardTrigger>
                                <HoverCardContent
                                  className="w-56"
                                  sideOffset={5}
                                >
                                  <div className="space-y-2">
                                    <div className="flex items-center gap-2">
                                      <div
                                        className={`h-2 w-2 rounded-full ${
                                          model.status.last_success === true
                                            ? "bg-green-500"
                                            : model.status.last_success ===
                                                false
                                              ? "bg-red-500"
                                              : "bg-gray-400"
                                        }`}
                                      />
                                      <span className="font-medium text-sm">
                                        {model.status.last_success === true
                                          ? "Operational"
                                          : model.status.last_success === false
                                            ? "Down"
                                            : "Unknown"}
                                      </span>
                                    </div>
                                    {model.status.uptime_percentage !==
                                      undefined &&
                                      model.status.uptime_percentage !==
                                        null && (
                                        <p className="text-xs text-muted-foreground">
                                          {model.status.uptime_percentage.toFixed(
                                            2,
                                          )}
                                          % uptime (24h)
                                        </p>
                                      )}
                                  </div>
                                </HoverCardContent>
                              </HoverCard>
                            )}

                            {model.metrics && (
                              <HoverCard openDelay={200} closeDelay={100}>
                                <HoverCardTrigger asChild>
                                  <button
                                    className="text-gray-500 hover:text-gray-700 transition-colors p-1"
                                    onClick={(e) => e.stopPropagation()}
                                  >
                                    <Info className="h-4 w-4" />
                                    <span className="sr-only">
                                      View model description
                                    </span>
                                  </button>
                                </HoverCardTrigger>
                                <HoverCardContent
                                  className="w-96"
                                  sideOffset={5}
                                >
                                  {model.description ? (
                                    <Markdown
                                      className="text-sm text-muted-foreground"
                                      compact
                                    >
                                      {model.description}
                                    </Markdown>
                                  ) : (
                                    <p className="text-sm text-muted-foreground">
                                      No description provided
                                    </p>
                                  )}
                                </HoverCardContent>
                              </HoverCard>
                            )}
                          </div>
                          {/* Access Groups and Expand Icon */}
                          {canManageGroups && (
                            <div className="flex items-center gap-3">
                              <div
                                className="flex items-center gap-1 max-w-[180px]"
                                onClick={(e) => e.stopPropagation()}
                              >
                                {!model.groups || model.groups.length === 0 ? (
                                  <Button
                                    variant="outline"
                                    size="sm"
                                    onClick={() => {
                                      setAccessModel(model);
                                      setShowAccessModal(true);
                                    }}
                                    className="h-6 px-2 text-xs"
                                  >
                                    <Plus className="h-2.5 w-2.5" />
                                    Add groups
                                  </Button>
                                ) : (
                                  <>
                                    {model.groups.slice(0, 1).map((group) => (
                                      <Badge
                                        key={group.id}
                                        variant="secondary"
                                        className="text-xs"
                                        title={`Group: ${group.name}`}
                                      >
                                        <Users className="h-3 w-3" />
                                        <span className="max-w-[60px] truncate break-all">
                                          {group.name}
                                        </span>
                                      </Badge>
                                    ))}
                                    {model.groups.length > 1 ? (
                                      <HoverCard
                                        openDelay={200}
                                        closeDelay={100}
                                      >
                                        <HoverCardTrigger asChild>
                                          <Badge
                                            variant="outline"
                                            className="text-xs hover:bg-gray-50 select-none"
                                            onClick={() => {
                                              setAccessModel(model);
                                              setShowAccessModal(true);
                                            }}
                                          >
                                            +{model.groups.length - 1} more
                                          </Badge>
                                        </HoverCardTrigger>
                                        <HoverCardContent
                                          className="w-60"
                                          align="start"
                                          sideOffset={5}
                                        >
                                          <div className="flex flex-wrap gap-1">
                                            {model.groups.map((group) => (
                                              <Badge
                                                key={group.id}
                                                variant="secondary"
                                                className="text-xs max-w-[200px]"
                                              >
                                                <Users className="h-3 w-3 shrink-0" />
                                                <span className="truncate break-all">
                                                  {group.name}
                                                </span>
                                              </Badge>
                                            ))}
                                          </div>
                                        </HoverCardContent>
                                      </HoverCard>
                                    ) : (
                                      <Button
                                        variant="outline"
                                        size="icon"
                                        onClick={() => {
                                          setAccessModel(model);
                                          setShowAccessModal(true);
                                        }}
                                        className="h-6 w-6"
                                        title="Manage access groups"
                                      >
                                        <Plus className="h-2.5 w-2.5" />
                                      </Button>
                                    )}
                                  </>
                                )}
                              </div>
                            </div>
                          )}
                        </div>

                        {/* ROW 2: Endpoint or Hosted Model Count (only for platform managers) */}
                        {canManageGroups && model.is_composite ? (
                          <CardDescription className="flex items-center gap-1.5 min-w-0 mb-2">
                            <span className="text-gray-600 text-sm">
                              <span className="font-medium">
                                {model.components?.length || 0} hosted model
                                {(model.components?.length || 0) !== 1
                                  ? "s"
                                  : ""}
                              </span>
                            </span>
                          </CardDescription>
                        ) : (
                          !model.is_composite &&
                          canViewEndpoints &&
                          model.endpoint && (
                            <CardDescription className="flex items-center gap-1.5 min-w-0 mb-2">
                              <span className="text-gray-600 text-sm">
                                <span className="font-medium">
                                  {model.endpoint.name}
                                </span>
                              </span>
                            </CardDescription>
                          )
                        )}

                        {/* ROW 3: Tariffs */}
                        <CardDescription className="flex items-center gap-1.5 min-w-0">
                          {/* Show pricing for users with pricing permissions */}
                          {showPricing && (
                            <>
                              {(() => {
                                const batchTariffs = model.tariffs?.filter(
                                  (t) => t.api_key_purpose === "batch",
                                ) || [];

                                return (
                                  <>
                                    {batchTariffs.map((batchTariff, index) => (
                                      <React.Fragment key={batchTariff.id}>
                                        {index > 0 && <span className="mx-1">•</span>}
                                        <HoverCard openDelay={200} closeDelay={100}>
                                          <HoverCardTrigger asChild>
                                            <button
                                              className="flex items-center gap-0.5 shrink-0 hover:opacity-70 transition-opacity"
                                              onClick={(e) => e.stopPropagation()}
                                            >
                                              {batchTariff.completion_window || "Batch"}:
                                              {!batchTariff.input_price_per_token &&
                                              !batchTariff.output_price_per_token ? (
                                                <span className="flex items-center gap-0.5 text-green-700">
                                                  <div className="relative h-2.5 w-2.5">
                                                    <DollarSign className="h-2.5 w-2.5" />
                                                    <div className="absolute inset-0 flex items-center justify-center">
                                                      <div className="w-5 h-px bg-green-700 rotate-[-50deg]" />
                                                    </div>
                                                  </div>
                                                  <span>Free</span>
                                                </span>
                                              ) : (
                                                <span className="flex items-center gap-1">
                                                  <span className="flex items-center gap-0.5">
                                                    <ArrowUpToLine className="h-2.5 w-2.5 text-gray-500 shrink-0" />
                                                    <span className="whitespace-nowrap tabular-nums">
                                                      {batchTariff.input_price_per_token
                                                        ? (() => {
                                                            const price =
                                                              Number(
                                                                batchTariff.input_price_per_token,
                                                              ) * 1000000;
                                                            return `$${price % 1 === 0 ? price.toFixed(0) : price.toFixed(2)}`;
                                                          })()
                                                        : "$0"}
                                                    </span>
                                                    <span className="text-[8px] text-gray-400">
                                                      /M
                                                    </span>
                                                  </span>
                                                  <span className="flex items-center gap-0.5">
                                                    <ArrowDownToLine className="h-2.5 w-2.5 text-gray-500 shrink-0" />
                                                    <span className="whitespace-nowrap tabular-nums">
                                                      {batchTariff.output_price_per_token
                                                        ? (() => {
                                                            const price =
                                                              Number(
                                                                batchTariff.output_price_per_token,
                                                              ) * 1000000;
                                                            return `$${price % 1 === 0 ? price.toFixed(0) : price.toFixed(2)}`;
                                                          })()
                                                        : "$0"}
                                                    </span>
                                                    <span className="text-[8px] text-gray-400">
                                                      /M
                                                    </span>
                                                  </span>
                                                </span>
                                              )}
                                              <span className="sr-only">
                                                View {batchTariff.name} pricing details
                                              </span>
                                            </button>
                                          </HoverCardTrigger>
                                          <HoverCardContent
                                            className="w-48"
                                            sideOffset={5}
                                          >
                                            <p className="font-medium text-sm mb-1">
                                              {batchTariff.name}
                                            </p>
                                                {!batchTariff.input_price_per_token &&
                                                !batchTariff.output_price_per_token ? (
                                                  <div className="text-sm">
                                                    <p className="font-medium text-green-700">
                                                      Free
                                                    </p>
                                                    <p className="text-xs text-muted-foreground mt-1">
                                                      No charge for calls to this
                                                      model
                                                    </p>
                                                  </div>
                                                ) : (
                                                  <div className="space-y-1 text-xs">
                                                    <p className="text-muted-foreground">
                                                      Pricing per million tokens:
                                                    </p>
                                                    <p>
                                                      <span className="font-medium">
                                                        Input:
                                                      </span>{" "}
                                                      {batchTariff.input_price_per_token
                                                        ? (() => {
                                                            const price =
                                                              Number(
                                                                batchTariff.input_price_per_token,
                                                              ) * 1000000;
                                                            return `$${price % 1 === 0 ? price.toFixed(0) : price.toFixed(2)}`;
                                                          })()
                                                        : "$0"}
                                                    </p>
                                                    <p>
                                                      <span className="font-medium">
                                                        Output:
                                                      </span>{" "}
                                                      {batchTariff.output_price_per_token
                                                        ? (() => {
                                                            const price =
                                                              Number(
                                                                batchTariff.output_price_per_token,
                                                              ) * 1000000;
                                                            return `$${price % 1 === 0 ? price.toFixed(0) : price.toFixed(2)}`;
                                                          })()
                                                        : "$0"}
                                                    </p>
                                                  </div>
                                                )}
                                          </HoverCardContent>
                                        </HoverCard>
                                      </React.Fragment>
                                    ))}

                                    {batchTariffs.length === 0 && canManageModels && (
                                        <button
                                          className="flex items-center gap-1 text-xs text-gray-600 hover:text-gray-900 transition-colors"
                                          onClick={(e) => {
                                            e.stopPropagation();
                                            setPricingModel(model);
                                            setShowPricingModal(true);
                                          }}
                                          title="Set pricing tariffs"
                                        >
                                          <DollarSign className="h-3 w-3" />
                                          <span>Set pricing</span>
                                        </button>
                                      )}
                                  </>
                                );
                              })()}
                            </>
                          )}
                        </CardDescription>
                      </div>
                    </CardHeader>

                    <CardContent className="grow px-0 pt-0 pb-0 flex flex-col">
                      {model.metrics ? (
                        <div
                          className="flex gap-6 items-center px-6 pb-4"
                          style={{ minHeight: "90px" }}
                        >
                          <div className="flex-1">
                            <div className="grid grid-cols-2 gap-2 text-xs">
                              <div className="flex items-center gap-1.5">
                                <HoverCard openDelay={200} closeDelay={100}>
                                  <HoverCardTrigger asChild>
                                    <BarChart3 className="h-3.5 w-3.5 text-gray-500 " />
                                  </HoverCardTrigger>
                                  <HoverCardContent
                                    className="w-40"
                                    sideOffset={5}
                                  >
                                    <p className="text-xs text-muted-foreground">
                                      Total requests made to this model
                                    </p>
                                  </HoverCardContent>
                                </HoverCard>
                                <span className="text-gray-600">
                                  {formatNumber(model.metrics.total_requests)}{" "}
                                  requests
                                </span>
                              </div>

                              <div className="flex items-center gap-1.5">
                                <HoverCard openDelay={200} closeDelay={100}>
                                  <HoverCardTrigger asChild>
                                    <Activity className="h-3.5 w-3.5 text-gray-500 " />
                                  </HoverCardTrigger>
                                  <HoverCardContent
                                    className="w-40"
                                    sideOffset={5}
                                  >
                                    <p className="text-xs text-muted-foreground">
                                      Average response time across all requests
                                    </p>
                                  </HoverCardContent>
                                </HoverCard>
                                <span className="text-gray-600">
                                  {formatLatency(model.metrics.avg_latency_ms)}{" "}
                                  avg
                                </span>
                              </div>

                              <div className="flex items-center gap-1.5">
                                <HoverCard openDelay={200} closeDelay={100}>
                                  <HoverCardTrigger asChild>
                                    <ArrowUpDown className="h-3.5 w-3.5 text-gray-500 " />
                                  </HoverCardTrigger>
                                  <HoverCardContent
                                    className="w-48"
                                    sideOffset={5}
                                  >
                                    <div className="text-xs text-muted-foreground">
                                      <p>
                                        Input:{" "}
                                        {formatNumber(
                                          model.metrics.total_input_tokens,
                                        )}
                                      </p>
                                      <p>
                                        Output:{" "}
                                        {formatNumber(
                                          model.metrics.total_output_tokens,
                                        )}
                                      </p>
                                      <p className="mt-1 font-medium">
                                        Total tokens processed
                                      </p>
                                    </div>
                                  </HoverCardContent>
                                </HoverCard>
                                <span className="text-gray-600">
                                  {formatNumber(
                                    model.metrics.total_input_tokens +
                                      model.metrics.total_output_tokens,
                                  )}{" "}
                                  tokens
                                </span>
                              </div>

                              <div className="flex items-center gap-1.5">
                                <HoverCard openDelay={200} closeDelay={100}>
                                  <HoverCardTrigger asChild>
                                    <Clock className="h-3.5 w-3.5 text-gray-500 " />
                                  </HoverCardTrigger>
                                  <HoverCardContent
                                    className="w-36"
                                    sideOffset={5}
                                  >
                                    <p className="text-xs text-muted-foreground">
                                      Last request received
                                    </p>
                                  </HoverCardContent>
                                </HoverCard>
                                <span className="text-gray-600">
                                  {formatRelativeTime(
                                    model.metrics.last_active_at,
                                  )}
                                </span>
                              </div>
                            </div>
                          </div>

                          <div className="flex-1 flex items-center justify-center px-2">
                            <div className="w-full max-w-[200px] min-w-[120px]">
                              <Sparkline
                                data={model.metrics.time_series || []}
                                width={180}
                                height={35}
                                className="w-full h-auto"
                              />
                            </div>
                          </div>
                        </div>
                      ) : canViewAnalytics && metricsLoading ? (
                        <div
                          className="flex gap-6 items-center px-6 pb-4"
                          style={{ minHeight: "90px" }}
                        >
                          <div className="flex-1">
                            <div className="grid grid-cols-2 gap-2">
                              <Skeleton className="h-4 w-24" />
                              <Skeleton className="h-4 w-20" />
                              <Skeleton className="h-4 w-28" />
                              <Skeleton className="h-4 w-16" />
                            </div>
                          </div>
                          <div className="flex-1 flex items-center justify-center px-2">
                            <Skeleton className="h-[35px] w-[180px]" />
                          </div>
                        </div>
                      ) : (
                        <div
                          className="px-6 pb-4"
                          style={{ minHeight: "90px" }}
                        >
                          {model.description ? (
                            <div className="relative">
                              <div
                                className="text-sm text-gray-700"
                                style={{
                                  display: "-webkit-box",
                                  WebkitLineClamp: 2,
                                  WebkitBoxOrient: "vertical",
                                  overflow: "hidden",
                                  wordBreak: "break-word",
                                }}
                              >
                                <Markdown className="inline" compact>
                                  {(() => {
                                    // Get first line only (split by newlines)
                                    const firstLine =
                                      model.description.split("\n")[0];
                                    // Roughly estimate how many characters fit in 2 lines
                                    const maxChars = 150;
                                    if (firstLine.length <= maxChars) {
                                      return firstLine;
                                    }
                                    // Find the last complete word before the limit
                                    let truncated = firstLine.substring(
                                      0,
                                      maxChars,
                                    );
                                    const lastSpace =
                                      truncated.lastIndexOf(" ");
                                    if (lastSpace > 0) {
                                      truncated = truncated.substring(
                                        0,
                                        lastSpace,
                                      );
                                    }
                                    return truncated + "...";
                                  })()}
                                </Markdown>
                              </div>
                              {(model.description.split("\n")[0].length > 150 ||
                                model.description.split("\n").length > 1 ||
                                model.description.length > 150) && (
                                <span className="text-xs text-blue-600 hover:text-blue-700 cursor-pointer">
                                  read more
                                </span>
                              )}
                            </div>
                          ) : (
                            <p className="text-sm text-gray-700">
                              No description provided
                            </p>
                          )}
                        </div>
                      )}
                    </CardContent>
                  </div>

                  <div className="border-t">
                    <div className="grid grid-cols-2 divide-x">
                      <button
                        className="flex items-center justify-center gap-1.5 py-3.5 text-sm font-medium text-gray-600 hover:bg-gray-50 hover:text-gray-700 transition-colors rounded-bl-lg"
                        onClick={() => {
                          setApiExamplesModel(model);
                          setShowApiExamples(true);
                        }}
                      >
                        <Code className="h-4 w-4 text-blue-500" />
                        <span>API</span>
                      </button>
                      <button
                        className="flex items-center justify-center gap-1.5 py-3.5 text-sm font-medium text-gray-600 hover:bg-gray-50 hover:text-gray-700 transition-colors rounded-br-lg group"
                        onClick={() => {
                          navigate(
                            `/playground?model=${encodeURIComponent(model.alias)}&from=${encodeURIComponent("/models")}`,
                          );
                        }}
                      >
                        <ArrowRight className="h-4 w-4 text-purple-500 group-hover:translate-x-0.5 transition-transform" />
                        <span>Playground</span>
                      </button>
                    </div>
                  </div>
                </Card>
              );
            })}
          </div>

          <TablePagination
            itemName="models"
            itemsPerPage={pagination.pageSize}
            currentPage={pagination.page}
            onPageChange={pagination.handlePageChange}
            totalItems={rawModelsData?.total_count || 0}
          />
        </>
      )}

      {canManageGroups && accessModel && (
        <AccessManagementModal
          isOpen={showAccessModal}
          onClose={() => {
            setShowAccessModal(false);
            setAccessModel(null);
          }}
          model={accessModel}
        />
      )}

      <ApiExamples
        isOpen={showApiExamples}
        onClose={() => {
          setShowApiExamples(false);
          setApiExamplesModel(null);
        }}
        model={apiExamplesModel}
      />

      {showPricing && pricingModel && (
        <UpdateModelPricingModal
          isOpen={showPricingModal}
          modelId={pricingModel.id}
          modelName={pricingModel.alias}
          onClose={() => {
            setShowPricingModal(false);
            setPricingModel(null);
          }}
        />
      )}
    </>
  );
};
