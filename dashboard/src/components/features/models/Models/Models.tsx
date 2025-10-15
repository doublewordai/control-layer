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
  ChevronRight,
} from "lucide-react";
import {
  useModels,
  type Model,
  useEndpoints,
  type Endpoint,
} from "../../../../api/waycast";
import { AccessManagementModal } from "../../../modals";
import { ApiExamples } from "../../../modals";
import { useAuthorization } from "../../../../utils";
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
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../../ui/select";
import { Input } from "../../../ui/input";
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

// Utility functions for formatting
const formatNumber = (num: number): string => {
  if (num >= 1_000_000_000) {
    return `${(num / 1_000_000_000).toFixed(1)}B`;
  }
  if (num >= 1_000_000) {
    return `${(num / 1_000_000).toFixed(1)}M`;
  }
  if (num >= 1_000) {
    return `${(num / 1_000).toFixed(1)}K`;
  }
  return num.toString();
};

const formatLatency = (ms?: number): string => {
  if (!ms) return "N/A";
  if (ms >= 1000) {
    return `${(ms / 1000).toFixed(1)}s`;
  }
  return `${Math.round(ms)}ms`;
};

const formatRelativeTime = (dateString?: string): string => {
  if (!dateString) return "Never";

  const date = new Date(dateString);
  const now = new Date();
  const diffMs = now.getTime() - date.getTime();

  const diffMinutes = Math.floor(diffMs / (1000 * 60));
  const diffHours = Math.floor(diffMs / (1000 * 60 * 60));
  const diffDays = Math.floor(diffMs / (1000 * 60 * 60 * 24));

  if (diffMinutes < 1) return "Just now";
  if (diffMinutes < 60) return `${diffMinutes}m ago`;
  if (diffHours < 24) return `${diffHours}h ago`;
  if (diffDays < 7) return `${diffDays}d ago`;

  return date.toLocaleDateString();
};

const Models: React.FC = () => {
  const navigate = useNavigate();
  const { hasPermission } = useAuthorization();
  const canManageGroups = hasPermission("manage-groups");
  const canViewAnalytics = hasPermission("analytics");
  const [filterProvider, setFilterProvider] = useState("all");
  const [showAccessModal, setShowAccessModal] = useState(false);
  const [accessModelId, setAccessModelId] = useState<string | null>(null);
  const [showApiExamples, setShowApiExamples] = useState(false);
  const [apiExamplesModel, setApiExamplesModel] = useState<Model | null>(null);
  const [currentPage, setCurrentPage] = useState(1);
  const [itemsPerPage] = useState(12);
  const [searchQuery, setSearchQuery] = useState("");
  const [showAccessibleOnly, setShowAccessibleOnly] = useState(false); // For admin toggle

  // Build include parameter based on permissions
  const includeParam = (() => {
    if (canManageGroups && canViewAnalytics) return "groups,metrics" as const;
    if (canManageGroups) return "groups" as const;
    if (canViewAnalytics) return "metrics" as const;
    return undefined;
  })();

  const {
    data: rawModelsData,
    isLoading: modelsLoading,
    error: modelsError,
  } = useModels({
    include: includeParam,
    accessible: !canManageGroups || showAccessibleOnly, // Users always get filtered, PlatformManagers only when toggle is on
  });

  // TODO: resolve `hosted_on` references in the backend, so we don't need this query.
  const {
    data: endpointsData,
    isLoading: endpointsLoading,
    error: endpointsError,
  } = useEndpoints();

  const loading = modelsLoading || endpointsLoading;
  const error = modelsError
    ? (modelsError as Error).message
    : endpointsError
      ? (endpointsError as Error).message
      : null;

  const { modelsRecord, modelsArray, endpointsRecord } = useMemo(() => {
    if (!rawModelsData || !endpointsData)
      return { modelsRecord: {}, modelsArray: [], endpointsRecord: {} };

    // Create models record and sorted array
    const modelsLookup: Record<string, Model> = Object.fromEntries(
      rawModelsData.map((model) => [model.id, model]),
    );
    const sortedArray = rawModelsData.sort((a, b) =>
      a.alias.localeCompare(b.alias),
    );

    const endpointsRecord = endpointsData.reduce(
      (acc, endpoint) => {
        acc[endpoint.id] = endpoint;
        return acc;
      },
      {} as Record<string, Endpoint>,
    );

    return {
      modelsRecord: modelsLookup,
      modelsArray: sortedArray,
      endpointsRecord: endpointsRecord,
    };
  }, [rawModelsData, endpointsData]);

  // Extract unique providers from the models data dynamically
  const uniqueProviders = [
    ...new Set(
      modelsArray
        .map((model) => endpointsRecord[model.hosted_on]?.name)
        .filter(Boolean),
    ),
  ];
  const providers = ["all", ...uniqueProviders.sort()];

  const filteredModels = modelsArray.filter((model) => {
    const matchesProvider =
      filterProvider === "all" ||
      endpointsRecord[model.hosted_on]?.name === filterProvider;

    const matchesSearch =
      searchQuery === "" ||
      model.alias.toLowerCase().includes(searchQuery.toLowerCase()) ||
      model.model_name.toLowerCase().includes(searchQuery.toLowerCase());

    return matchesProvider && matchesSearch;
  });

  // Reset to page 1 when filter or search changes
  React.useEffect(() => {
    setCurrentPage(1);
  }, [filterProvider, searchQuery]);

  // Pagination calculations
  const totalItems = filteredModels.length;
  const totalPages = Math.ceil(totalItems / itemsPerPage);
  const startIndex = (currentPage - 1) * itemsPerPage;
  const endIndex = startIndex + itemsPerPage;
  const paginatedModels = filteredModels.slice(startIndex, endIndex);

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

  return (
    <div className="p-6">
      {/* Header */}
      <div className="mb-6">
        <div className="flex items-center justify-between">
          <div>
            <h1 className="text-3xl font-bold text-doubleword-neutral-900">
              Models
            </h1>
            <p className="text-doubleword-neutral-600 mt-1">
              View available models by provider
            </p>
          </div>
          <div className="flex items-center gap-3">
            {/* Access toggle for admins */}
            {canManageGroups && (
              <Select
                value={showAccessibleOnly ? "accessible" : "all"}
                onValueChange={(value) =>
                  setShowAccessibleOnly(value === "accessible")
                }
              >
                <SelectTrigger
                  className="w-[180px]"
                  aria-label="Model access filter"
                >
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="all">All Models</SelectItem>
                  <SelectItem value="accessible">
                    My Accessible Models
                  </SelectItem>
                </SelectContent>
              </Select>
            )}
            <div className="relative">
              <Search className="absolute left-3 top-1/2 transform -translate-y-1/2 text-gray-400 w-4 h-4 z-10" />
              <Input
                type="text"
                placeholder="Search models..."
                value={searchQuery}
                onChange={(e) => setSearchQuery(e.target.value)}
                className="pl-10 w-40 sm:w-48 md:w-64"
                aria-label="Search models"
              />
            </div>
            <Select
              value={filterProvider}
              onValueChange={(value) => setFilterProvider(value)}
            >
              <SelectTrigger
                className="w-[180px]"
                aria-label="Filter by endpoint provider"
              >
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {providers.map((provider) => (
                  <SelectItem key={provider} value={provider}>
                    {provider === "all" ? "All Endpoints" : provider}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
        </div>
      </div>

      {/* Model Cards Grid */}
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
              className="cursor-pointer hover:bg-gray-50 transition-colors group flex-grow flex flex-col"
              onClick={() => {
                navigate(
                  `/models/${model.id}?from=${encodeURIComponent("/models")}`,
                );
              }}
            >
              <CardHeader className="px-6 pt-5 pb-0">
                <div className="flex items-start justify-between gap-4">
                  <div className="flex-1 min-w-0">
                    <div className="flex items-center gap-2">
                      <CardTitle className="text-lg">{model.alias}</CardTitle>
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
                    <CardDescription className="mt-1">
                      {model.model_name} •{" "}
                      {endpointsRecord[model.hosted_on]?.name ||
                        "Unknown endpoint"}
                    </CardDescription>
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
                                <span className="max-w-[60px] truncate">
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
                                        className="text-xs"
                                      >
                                        <Users className="h-3 w-3" />
                                        {group.name}
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
              </CardHeader>

              <CardContent className="flex-grow px-0 pt-0 pb-0 flex flex-col">
                {model.metrics ? (
                  <div
                    className="flex gap-6 items-center px-6 pb-4"
                    style={{ minHeight: "90px" }}
                  >
                    {/* Left Half - Key Metrics */}
                    <div className="flex-1">
                      <div className="grid grid-cols-2 gap-2 text-xs">
                        <div className="flex items-center gap-1.5">
                          <HoverCard openDelay={200} closeDelay={100}>
                            <HoverCardTrigger asChild>
                              <BarChart3 className="h-3.5 w-3.5 text-gray-500 " />
                            </HoverCardTrigger>
                            <HoverCardContent className="w-40" sideOffset={5}>
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
                            <HoverCardContent className="w-40" sideOffset={5}>
                              <p className="text-xs text-muted-foreground">
                                Average response time across all requests
                              </p>
                            </HoverCardContent>
                          </HoverCard>
                          <span className="text-gray-600">
                            {formatLatency(model.metrics.avg_latency_ms)} avg
                          </span>
                        </div>

                        <div className="flex items-center gap-1.5">
                          <HoverCard openDelay={200} closeDelay={100}>
                            <HoverCardTrigger asChild>
                              <ArrowUpDown className="h-3.5 w-3.5 text-gray-500 " />
                            </HoverCardTrigger>
                            <HoverCardContent className="w-48" sideOffset={5}>
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
                            <HoverCardContent className="w-36" sideOffset={5}>
                              <p className="text-xs text-muted-foreground">
                                Last request received
                              </p>
                            </HoverCardContent>
                          </HoverCard>
                          <span className="text-gray-600">
                            {formatRelativeTime(model.metrics.last_active_at)}
                          </span>
                        </div>
                      </div>
                    </div>

                    {/* Right Half - Activity Sparkline */}
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
                  // Fallback when metrics not available - show description
                  <div
                    className="flex items-center px-6 pb-4"
                    style={{ minHeight: "90px" }}
                  >
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

      {/* Pagination */}
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

              // First page
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

              // Page numbers
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

              // Last page
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

      {/* Results Info */}
      {filteredModels.length > 0 && (
        <div className="flex items-center justify-center mt-4 text-sm text-gray-600">
          Showing {startIndex + 1}-{Math.min(endIndex, totalItems)} of{" "}
          {totalItems} models
        </div>
      )}

      {/* Access Management Modal - Only for Admin users */}
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

      {/* API Examples Modal */}
      <ApiExamples
        isOpen={showApiExamples}
        onClose={() => {
          setShowApiExamples(false);
          setApiExamplesModel(null);
        }}
        model={apiExamplesModel}
      />
    </div>
  );
};

export default Models;
