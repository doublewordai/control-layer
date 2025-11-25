import React, { useState, useMemo, useEffect } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
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
  ChevronRight,
  DollarSign,
  ArrowDown,
  ArrowUp,
} from "lucide-react";
import {
  useModels,
  type Model,
  type ModelsInclude,
  useEndpoints,
  type Endpoint,
  useProbes,
} from "../../../../api/control-layer";
import { AccessManagementModal } from "../../../modals";
import { ApiExamples } from "../../../modals";
import {
  Pagination,
  PaginationContent,
  PaginationEllipsis,
  PaginationItem,
  PaginationLink,
  PaginationNext,
  PaginationPrevious,
} from "../../../ui/pagination";
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
import { StatusRow } from "./StatusRow";
import { useDebounce } from "@/hooks/useDebounce";

export interface ModelsContentProps {
  currentPage: number;
  setCurrentPage: (page: number) => void;
  searchQuery: string;
  filterProvider: string;
  showAccessibleOnly: boolean;
  isStatusMode: boolean;
  canManageGroups: boolean;
  canViewAnalytics: boolean;
  showPricing: boolean;
  onClearFilters: () => void;
}

export const ModelsContent: React.FC<ModelsContentProps> = ({
  currentPage,
  setCurrentPage,
  searchQuery,
  filterProvider,
  showAccessibleOnly,
  isStatusMode,
  canManageGroups,
  canViewAnalytics,
  showPricing,
  onClearFilters,
}) => {
  const navigate = useNavigate();
  const [showAccessModal, setShowAccessModal] = useState(false);
  const [accessModelId, setAccessModelId] = useState<string | null>(null);
  const [showApiExamples, setShowApiExamples] = useState(false);
  const [apiExamplesModel, setApiExamplesModel] = useState<Model | null>(null);
  const [itemsPerPage] = useState(12);
  const debouncedSearch = useDebounce(searchQuery, 300);

  const includeParam = useMemo(() => {
    const parts: string[] = ["status"];
    if (canManageGroups) parts.push("groups");
    if (canViewAnalytics) parts.push("metrics");
    if (showPricing) parts.push("pricing");
    return parts.join(",");
  }, [canManageGroups, canViewAnalytics, showPricing]);

  const {
    data: rawModelsData,
    isLoading: modelsLoading,
    error: modelsError,
  } = useModels({
    skip: (currentPage - 1) * itemsPerPage,
    limit: itemsPerPage,
    include: includeParam as ModelsInclude,
    accessible: isStatusMode ? true : !canManageGroups || showAccessibleOnly,
    search: debouncedSearch || undefined, // ← API debounce only
  });

  const {
    data: endpointsData,
    isLoading: endpointsLoading,
    error: endpointsError,
  } = useEndpoints();

  const { data: probesData } = useProbes();

  const loading = modelsLoading || endpointsLoading;
  const error = modelsError
    ? (modelsError as Error).message
    : endpointsError
      ? (endpointsError as Error).message
      : null;

  const { modelsRecord, modelsArray, endpointsRecord, totalCount } =
    useMemo(() => {
      if (!rawModelsData || !endpointsData)
        return {
          modelsRecord: {},
          modelsArray: [],
          endpointsRecord: {},
          totalCount: 0,
        };

      const rec = Object.fromEntries(rawModelsData.data.map((m) => [m.id, m]));

      const sorted = [...rawModelsData.data].sort((a, b) =>
        a.alias.localeCompare(b.alias),
      );

      const epRec = endpointsData.reduce(
        (acc, ep) => {
          acc[ep.id] = ep;
          return acc;
        },
        {} as Record<string, Endpoint>,
      );

      return {
        modelsRecord: rec,
        modelsArray: sorted,
        endpointsRecord: epRec,
        totalCount: rawModelsData.total_count,
      };
    }, [rawModelsData, endpointsData]);

  // TODO: filter providers on the server-side
  const filteredModels = modelsArray.filter((model) => {
    if (isStatusMode && !model.status?.probe_id) {
      return false;
    }

    const matchesProvider =
      filterProvider === "all" ||
      endpointsRecord[model.hosted_on]?.name === filterProvider;

    return matchesProvider;
  });

  const totalItems = totalCount;
  const totalPages = Math.ceil(totalItems / itemsPerPage);
  const startIndex = (currentPage - 1) * itemsPerPage;
  const endIndex = Math.min(startIndex + itemsPerPage, totalItems);
  const paginatedModels = filteredModels;

  const hasNoModels = totalCount === 0 && currentPage === 1;
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
                endpointsRecord={endpointsRecord}
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
            {paginatedModels.map((model) => (
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
                    <div className="space-y-2">
                      {/* ROW 1: Alias on left, groups/chevron on right */}
                      <div className="flex items-center justify-between gap-4">
                        <div className="flex items-center gap-2">
                          <CardTitle className="text-lg truncate break-all">
                            {model.alias}
                          </CardTitle>

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
                              <HoverCardContent className="w-56" sideOffset={5}>
                                <div className="space-y-2">
                                  <div className="flex items-center gap-2">
                                    <div
                                      className={`h-2 w-2 rounded-full ${
                                        model.status.last_success === true
                                          ? "bg-green-500"
                                          : model.status.last_success === false
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
                                    model.status.uptime_percentage !== null && (
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
                            <HoverCardContent className="w-96" sideOffset={5}>
                              <p className="text-sm text-muted-foreground">
                                {model.description || "No description provided"}
                              </p>
                            </HoverCardContent>
                          </HoverCard>
                        </div>

                        {/* Access Groups and Expand Icon */}
                        <div className="flex items-center gap-3">
                          {canManageGroups && (
                            <div
                              className="flex items-center gap-1 max-w-[180px]"
                              onClick={(e) => e.stopPropagation()}
                            >
                              {!model.groups || model.groups.length === 0 ? (
                                <Button
                                  variant="outline"
                                  size="sm"
                                  onClick={() => {
                                    setAccessModelId(model.id);
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
                                    <HoverCard openDelay={200} closeDelay={100}>
                                      <HoverCardTrigger asChild>
                                        <Badge
                                          variant="outline"
                                          className="text-xs hover:bg-gray-50 select-none"
                                          onClick={() => {
                                            setAccessModelId(model.id);
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
                                        setAccessModelId(model.id);
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
                          )}

                          <ChevronRight className="h-5 w-5 text-gray-400 group-hover:text-gray-600 transition-colors" />
                        </div>
                      </div>

                      {/* ROW 2: Pricing, model name, endpoint */}
                      <CardDescription className="flex items-center gap-1.5 min-w-0">
                        {/* Show pricing for all users */}
                        {showPricing && (
                          <>
                            <HoverCard openDelay={200} closeDelay={100}>
                              <HoverCardTrigger asChild>
                                <button
                                  className="flex items-center gap-0.5 shrink-0 hover:opacity-70 transition-opacity"
                                  onClick={(e) => e.stopPropagation()}
                                >
                                  {!model.pricing?.input_price_per_token &&
                                  !model.pricing?.output_price_per_token ? (
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
                                    <span className="flex items-center gap-0.5">
                                      <ArrowDown className="h-2.5 w-2.5 text-gray-500 shrink-0" />
                                      <span className="whitespace-nowrap tabular-nums">
                                        {model.pricing?.input_price_per_token
                                          ? (() => {
                                              const price =
                                                Number(
                                                  model.pricing
                                                    .input_price_per_token,
                                                ) * 1000000;
                                              return `$${price % 1 === 0 ? price.toFixed(0) : price.toFixed(2)}`;
                                            })()
                                          : "$0"}
                                      </span>
                                      <span className="text-[8px] text-gray-400">
                                        /M
                                      </span>
                                      <ArrowUp className="h-2.5 w-2.5 text-gray-500 shrink-0 ml-0.5" />
                                      <span className="whitespace-nowrap tabular-nums">
                                        {model.pricing?.output_price_per_token
                                          ? (() => {
                                              const price =
                                                Number(
                                                  model.pricing
                                                    .output_price_per_token,
                                                ) * 1000000;
                                              return `$${price % 1 === 0 ? price.toFixed(0) : price.toFixed(2)}`;
                                            })()
                                          : "$0"}
                                      </span>
                                      <span className="text-[8px] text-gray-400">
                                        /M
                                      </span>
                                    </span>
                                  )}
                                  <span className="sr-only">
                                    View pricing details
                                  </span>
                                </button>
                              </HoverCardTrigger>
                              <HoverCardContent className="w-48" sideOffset={5}>
                                {!model.pricing?.input_price_per_token &&
                                !model.pricing?.output_price_per_token ? (
                                  <div className="text-sm">
                                    <p className="font-medium text-green-700">
                                      Free
                                    </p>
                                    <p className="text-xs text-muted-foreground mt-1">
                                      No charge for calls to this model
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
                                      {model.pricing?.input_price_per_token
                                        ? (() => {
                                            const price =
                                              Number(
                                                model.pricing
                                                  .input_price_per_token,
                                              ) * 1000000;
                                            return `$${price % 1 === 0 ? price.toFixed(0) : price.toFixed(2)}`;
                                          })()
                                        : "$0"}
                                    </p>
                                    <p>
                                      <span className="font-medium">
                                        Output:
                                      </span>{" "}
                                      {model.pricing?.output_price_per_token
                                        ? (() => {
                                            const price =
                                              Number(
                                                model.pricing
                                                  .output_price_per_token,
                                              ) * 1000000;
                                            return `$${price % 1 === 0 ? price.toFixed(0) : price.toFixed(2)}`;
                                          })()
                                        : "$0"}
                                    </p>
                                  </div>
                                )}
                              </HoverCardContent>
                            </HoverCard>
                            <span>•</span>
                          </>
                        )}
                        <span className="flex items-center gap-1.5 min-w-0">
                          {model.model_name.length > 30 ? (
                            <HoverCard openDelay={200} closeDelay={100}>
                              <HoverCardTrigger asChild>
                                <span className="truncate max-w-[200px] hover:opacity-70 transition-opacity">
                                  {model.model_name}
                                </span>
                              </HoverCardTrigger>
                              <HoverCardContent
                                className="w-auto max-w-sm"
                                sideOffset={5}
                              >
                                <p className="text-sm break-all">
                                  {model.model_name}
                                </p>
                              </HoverCardContent>
                            </HoverCard>
                          ) : (
                            <span className="truncate max-w-[200px]">
                              {model.model_name}
                            </span>
                          )}
                          <span>•</span>
                          {(() => {
                            const endpointName =
                              endpointsRecord[model.hosted_on]?.name ||
                              "Unknown endpoint";
                            return endpointName.length > 25 ? (
                              <HoverCard openDelay={200} closeDelay={100}>
                                <HoverCardTrigger asChild>
                                  <span className="truncate max-w-[150px] hover:opacity-70 transition-opacity">
                                    {endpointName}
                                  </span>
                                </HoverCardTrigger>
                                <HoverCardContent
                                  className="w-auto max-w-sm"
                                  sideOffset={5}
                                >
                                  <p className="text-sm">{endpointName}</p>
                                </HoverCardContent>
                              </HoverCard>
                            ) : (
                              <span className="truncate max-w-[150px]">
                                {endpointName}
                              </span>
                            );
                          })()}
                        </span>
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
                    ) : (
                      <div className="px-6 pb-4" style={{ minHeight: "90px" }}>
                        <p className="text-sm text-gray-700 line-clamp-3">
                          {model.description || "No description provided"}
                        </p>
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
            ))}
          </div>

          {totalPages > 1 && (
            <Pagination className="mt-8">
              <PaginationContent>
                <PaginationItem>
                  <PaginationPrevious
                    href="#"
                    onClick={(e) => {
                      e.preventDefault();
                      setCurrentPage(Math.max(1, currentPage - 1));
                    }}
                    className={
                      currentPage === 1
                        ? "pointer-events-none opacity-50"
                        : "cursor-pointer"
                    }
                  />
                </PaginationItem>

                {(() => {
                  const items = [];
                  let startPage = 1;
                  let endPage = totalPages;

                  if (totalPages > 7) {
                    if (currentPage <= 3) {
                      endPage = 5;
                    } else if (currentPage >= totalPages - 2) {
                      startPage = totalPages - 4;
                    } else {
                      startPage = currentPage - 2;
                      endPage = currentPage + 2;
                    }
                  }

                  if (startPage > 1) {
                    items.push(
                      <PaginationItem key={1}>
                        <PaginationLink
                          href="#"
                          onClick={(e) => {
                            e.preventDefault();
                            setCurrentPage(1);
                          }}
                          isActive={currentPage === 1}
                        >
                          1
                        </PaginationLink>
                      </PaginationItem>,
                    );

                    if (startPage > 2) {
                      items.push(
                        <PaginationItem key="ellipsis-start">
                          <PaginationEllipsis />
                        </PaginationItem>,
                      );
                    }
                  }

                  for (let i = startPage; i <= endPage; i++) {
                    items.push(
                      <PaginationItem key={i}>
                        <PaginationLink
                          href="#"
                          onClick={(e) => {
                            e.preventDefault();
                            setCurrentPage(i);
                          }}
                          isActive={currentPage === i}
                        >
                          {i}
                        </PaginationLink>
                      </PaginationItem>,
                    );
                  }

                  if (endPage < totalPages) {
                    if (endPage < totalPages - 1) {
                      items.push(
                        <PaginationItem key="ellipsis-end">
                          <PaginationEllipsis />
                        </PaginationItem>,
                      );
                    }

                    items.push(
                      <PaginationItem key={totalPages}>
                        <PaginationLink
                          href="#"
                          onClick={(e) => {
                            e.preventDefault();
                            setCurrentPage(totalPages);
                          }}
                          isActive={currentPage === totalPages}
                        >
                          {totalPages}
                        </PaginationLink>
                      </PaginationItem>,
                    );
                  }

                  return items;
                })()}

                <PaginationItem>
                  <PaginationNext
                    href="#"
                    onClick={(e) => {
                      e.preventDefault();
                      setCurrentPage(Math.min(totalPages, currentPage + 1));
                    }}
                    className={
                      currentPage === totalPages
                        ? "pointer-events-none opacity-50"
                        : "cursor-pointer"
                    }
                  />
                </PaginationItem>
              </PaginationContent>
            </Pagination>
          )}

          {filteredModels.length > 0 && (
            <div className="flex items-center justify-center mt-4 text-sm text-gray-600">
              Showing {startIndex + 1}-{Math.min(endIndex, totalItems)} of{" "}
              {totalItems} models
            </div>
          )}
        </>
      )}

      {canManageGroups && accessModelId && modelsRecord[accessModelId] && (
        <AccessManagementModal
          isOpen={showAccessModal}
          onClose={() => {
            setShowAccessModal(false);
            setAccessModelId(null);
          }}
          model={modelsRecord[accessModelId]}
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
    </>
  );
};
