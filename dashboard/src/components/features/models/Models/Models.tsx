import React, { useState, useEffect } from "react";
import { useSearchParams } from "react-router-dom";
import {
  Search,
  Activity,
  LayoutGrid,
  MoreHorizontal,
  SlidersHorizontal,
  Layers,
  ChevronDown,
  X,
  Check,
} from "lucide-react";
import { useAuthorization } from "../../../../utils";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../../ui/select";
import { Tabs, TabsList, TabsTrigger } from "../../../ui/tabs";
import { Input } from "../../../ui/input";
import { Button } from "../../../ui/button";
import { Popover, PopoverContent, PopoverTrigger } from "../../../ui/popover";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "../../../ui/dropdown-menu";
import { Label } from "../../../ui/label";
import { Switch } from "../../../ui/switch";
import { ModelsContent } from "./ModelsContent";
import { SupportRequestModal, CreateVirtualModelModal } from "../../../modals";
import { useEndpoints, useGroups } from "@/api/control-layer/hooks";
import { useDebounce } from "@/hooks/useDebounce";
import { useServerPagination } from "@/hooks/useServerPagination";
import {
  usePersistedFilter,
  clearPersistedFilters,
} from "@/hooks/usePersistedFilter";

const EMPTY_GROUPS: string[] = [];
const MODEL_TYPES = ["all", "virtual", "hosted"] as const;
type ModelType = (typeof MODEL_TYPES)[number];
const FILTER_PARAM_NAMES = ["endpoint", "groups", "type", "accessible"];

const Models: React.FC = () => {
  const [searchParams, setSearchParams] = useSearchParams();
  const { hasPermission } = useAuthorization();
  const [isSupportModalOpen, setIsSupportModalOpen] = useState(false);
  const [isCreateVirtualModalOpen, setIsCreateVirtualModalOpen] =
    useState(false);
  const canManageGroups = hasPermission("manage-groups");
  const canViewAnalytics = hasPermission("analytics");
  const canViewEndpoints = hasPermission("endpoints");
  const showPricing = true;
  const canManageModels = hasPermission("manage-models");

  const [filterProvider, setFilterProvider] = usePersistedFilter("endpoint", "all");
  const [filterGroups, setFilterGroups] = usePersistedFilter("groups", EMPTY_GROUPS);
  const [rawModelType, setFilterModelType] = usePersistedFilter("type", "all");
  const filterModelType: ModelType = (MODEL_TYPES as readonly string[]).includes(rawModelType)
    ? (rawModelType as ModelType)
    : "all";
  const [accessibleOnly, setAccessibleOnly] = usePersistedFilter("accessible", "false");
  const showAccessibleOnly = accessibleOnly === "true";

  const [searchQuery, setSearchQuery] = useState(
    searchParams.get("search") || "",
  );
  const debouncedSearch = useDebounce(searchQuery, 300);

  // Use pagination hook for URL-based pagination state
  const pagination = useServerPagination({ defaultPageSize: 12 });

  const { data: endpointsData } = useEndpoints();
  const { data: groupsData } = useGroups();
  const providers = [
    ...new Set(["all", ...(endpointsData || []).map((e) => e.name).sort()]),
  ];

  // Get endpoint ID from selected provider name for server-side filtering
  const selectedEndpointId =
    filterProvider !== "all"
      ? endpointsData?.find((e) => e.name === filterProvider)?.id
      : undefined;

  // Get group IDs from selected groups for server-side filtering (comma-separated)
  const selectedGroupIds =
    filterGroups.length > 0 ? filterGroups.join(",") : undefined;

  const groups = groupsData?.data || [];

  // Sync search query to URL params
  useEffect(() => {
    setSearchParams(
      (prev) => {
        const params = new URLSearchParams(prev);
        if (searchQuery) {
          params.set("search", searchQuery);
        } else {
          params.delete("search");
        }
        return params;
      },
      { replace: true },
    );
  }, [searchQuery, setSearchParams]);

  // Reset pagination when search query, endpoint, groups, or model type filter changes
  useEffect(() => {
    pagination.handleReset();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [debouncedSearch, selectedEndpointId, selectedGroupIds, filterModelType]);

  const viewMode = searchParams.get("view") || "grid";
  const isStatusMode = viewMode === "status";

  const handleTabChange = (value: string) => {
    setSearchParams(
      (prev) => {
        const next = new URLSearchParams(prev);
        next.set("view", value);
        return next;
      },
      { replace: true },
    );
  };

  const handleClearFilters = () => {
    setSearchQuery("");
    clearPersistedFilters(setSearchParams, FILTER_PARAM_NAMES);
  };

  return (
    <div className="p-4 md:p-6">
      <Tabs value={viewMode} onValueChange={handleTabChange}>
        {/* Header */}
        <div className="mb-6">
          <div className="flex flex-col lg:flex-row lg:items-center lg:justify-between gap-4">
            <div className="flex items-baseline gap-3">
              <h1 className="text-2xl md:text-3xl font-bold text-doubleword-neutral-900">
                Models
              </h1>
              <button
                onClick={() => setIsSupportModalOpen(true)}
                className="text-sm text-blue-600 hover:text-blue-700"
              >
                Request a model
              </button>
            </div>
            <div className="flex flex-col sm:flex-row items-stretch sm:items-center gap-2 sm:gap-3">
              {/* Search - most frequently used */}
              <div className="relative">
                <Search className="absolute left-3 top-1/2 transform -translate-y-1/2 text-gray-400 w-4 h-4 z-10 pointer-events-none" />
                <Input
                  type="text"
                  placeholder="Search models..."
                  value={searchQuery}
                  onChange={(e) => setSearchQuery(e.target.value)}
                  className="pl-10 w-40 sm:w-48 md:w-64"
                  aria-label="Search models"
                />
              </div>

              {/* View mode tabs - only for platform managers */}
              {canManageGroups && (
                <TabsList className="w-full sm:w-auto">
                  <TabsTrigger
                    value="grid"
                    className="flex items-center gap-2 flex-1 sm:flex-initial"
                  >
                    <LayoutGrid className="h-4 w-4" />
                    <span className="hidden sm:inline">Grid</span>
                  </TabsTrigger>
                  <TabsTrigger
                    value="status"
                    className="flex items-center gap-2 flex-1 sm:flex-initial"
                  >
                    <Activity className="h-4 w-4" />
                    <span className="hidden sm:inline">Status</span>
                  </TabsTrigger>
                </TabsList>
              )}

              {/* Filter popover - consolidates endpoint, type, and access filters */}
              {(canViewEndpoints || (!isStatusMode && canManageGroups)) && (
                <Popover>
                  <PopoverTrigger asChild>
                    <Button
                      variant="outline"
                      size="sm"
                      className="relative gap-1"
                      aria-label="Filter models"
                    >
                      <SlidersHorizontal className="h-4 w-4" />
                      <span className="hidden sm:inline">Filter</span>
                      <ChevronDown className="h-3 w-3 opacity-50" />
                      {(filterProvider !== "all" ||
                        filterGroups.length > 0 ||
                        (canManageGroups && filterModelType !== "all") ||
                        showAccessibleOnly) && (
                        <span className="absolute -top-1 -right-1 h-2 w-2 rounded-full bg-blue-500" />
                      )}
                    </Button>
                  </PopoverTrigger>
                  <PopoverContent align="end" className="w-56">
                    <div className="space-y-4">
                      {canViewEndpoints && (
                        <div className="space-y-2">
                          <Label htmlFor="provider-filter">Endpoint</Label>
                          <Select
                            value={filterProvider}
                            onValueChange={(value) => setFilterProvider(value)}
                          >
                            <SelectTrigger
                              id="provider-filter"
                              className="w-full"
                              aria-label="Filter by endpoint provider"
                            >
                              <SelectValue />
                            </SelectTrigger>
                            <SelectContent>
                              {providers.map((provider) => (
                                <SelectItem key={provider} value={provider}>
                                  {provider === "all"
                                    ? "All Endpoints"
                                    : provider}
                                </SelectItem>
                              ))}
                            </SelectContent>
                          </Select>
                        </div>
                      )}
                      {canManageGroups && (
                        <div className="space-y-2">
                          <Label htmlFor="group-filter">Groups</Label>
                          <Popover>
                            <PopoverTrigger asChild>
                              <button
                                id="group-filter"
                                className="w-full inline-flex items-center justify-between rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background focus:outline-none focus:ring-2 focus:ring-ring focus:ring-offset-2 disabled:cursor-not-allowed disabled:opacity-50"
                                style={{ whiteSpace: "nowrap" }}
                              >
                                <span className="flex-1 text-left truncate">
                                  {filterGroups.length === 0 ? (
                                    <span className="text-muted-foreground">
                                      Select groups...
                                    </span>
                                  ) : (
                                    <span className="flex gap-1 flex-wrap">
                                      {filterGroups.map((groupId) => {
                                        const group = groups.find(
                                          (g) => g.id === groupId,
                                        );
                                        return group ? (
                                          <span
                                            key={groupId}
                                            className="inline-flex items-center gap-1 px-2 py-0.5 rounded-md bg-secondary text-secondary-foreground text-xs"
                                          >
                                            {group.name}
                                            <X
                                              className="h-3 w-3 cursor-pointer hover:opacity-70"
                                              onClick={(e) => {
                                                e.stopPropagation();
                                                setFilterGroups(
                                                  filterGroups.filter(
                                                    (id) => id !== groupId,
                                                  ),
                                                );
                                              }}
                                            />
                                          </span>
                                        ) : null;
                                      })}
                                    </span>
                                  )}
                                </span>
                                <ChevronDown className="h-4 w-4 opacity-50 shrink-0 ml-2" />
                              </button>
                            </PopoverTrigger>
                            <PopoverContent align="start" className="w-56 p-0">
                              <div className="max-h-64 overflow-y-auto">
                                {groups.length === 0 ? (
                                  <div className="p-3 text-sm text-muted-foreground">
                                    No groups available
                                  </div>
                                ) : (
                                  groups.map((group) => {
                                    const isSelected = filterGroups.includes(
                                      group.id,
                                    );
                                    return (
                                      <button
                                        key={group.id}
                                        onClick={() => {
                                          if (isSelected) {
                                            setFilterGroups(
                                              filterGroups.filter(
                                                (id) => id !== group.id,
                                              ),
                                            );
                                          } else {
                                            setFilterGroups([
                                              ...filterGroups,
                                              group.id,
                                            ]);
                                          }
                                        }}
                                        className="w-full flex items-center gap-2 rounded-sm py-1.5 pl-2 pr-2 text-sm hover:bg-accent hover:text-accent-foreground transition-colors text-left cursor-default"
                                      >
                                        <div className="w-4 h-4 shrink-0 flex items-center justify-center">
                                          {isSelected && (
                                            <Check className="h-4 w-4 text-primary" />
                                          )}
                                        </div>
                                        <span>{group.name}</span>
                                      </button>
                                    );
                                  })
                                )}
                              </div>
                            </PopoverContent>
                          </Popover>
                        </div>
                      )}
                      {canManageGroups && (
                        <div className="space-y-2">
                          <Label>Model Type</Label>
                          <div className="flex rounded-md border border-input overflow-hidden">
                            {(["all", "virtual", "hosted"] as const).map(
                              (type) => (
                                <button
                                  key={type}
                                  onClick={() => setFilterModelType(type)}
                                  className={`flex-1 px-2 py-1.5 text-xs font-medium transition-colors ${
                                    filterModelType === type
                                      ? "bg-primary text-primary-foreground"
                                      : "bg-background hover:bg-muted text-muted-foreground"
                                  }`}
                                  aria-label={`Show ${type === "all" ? "all models" : type + " models"}`}
                                >
                                  {type === "all"
                                    ? "All"
                                    : type === "virtual"
                                      ? "Virtual"
                                      : "Hosted"}
                                </button>
                              ),
                            )}
                          </div>
                        </div>
                      )}
                      {!isStatusMode && canManageGroups && (
                        <div className="flex items-center justify-between">
                          <Label
                            htmlFor="access-toggle"
                            className="cursor-pointer"
                          >
                            Accessible only
                          </Label>
                          <Switch
                            id="access-toggle"
                            checked={showAccessibleOnly}
                            onCheckedChange={(checked) =>
                              setAccessibleOnly(checked ? "true" : "false")
                            }
                            aria-label="Show only my accessible models"
                          />
                        </div>
                      )}
                    </div>
                  </PopoverContent>
                </Popover>
              )}

              {/* Actions dropdown - for model management actions */}
              {canManageModels && (
                <DropdownMenu>
                  <DropdownMenuTrigger asChild>
                    <Button
                      variant="outline"
                      size="sm"
                      aria-label="Model actions"
                    >
                      <MoreHorizontal className="h-4 w-4" />
                      <span className="hidden sm:inline ml-1.5">Actions</span>
                    </Button>
                  </DropdownMenuTrigger>
                  <DropdownMenuContent align="end">
                    <DropdownMenuItem
                      onClick={() => setIsCreateVirtualModalOpen(true)}
                    >
                      <Layers className="h-4 w-4 mr-2" />
                      Create Virtual Model
                    </DropdownMenuItem>
                  </DropdownMenuContent>
                </DropdownMenu>
              )}
            </div>
          </div>
        </div>

        {/* Content - isolated to prevent header re-renders */}
        <ModelsContent
          pagination={pagination}
          searchQuery={debouncedSearch}
          filterProvider={filterProvider}
          filterModelType={filterModelType}
          endpointId={selectedEndpointId}
          groupId={selectedGroupIds}
          showAccessibleOnly={showAccessibleOnly}
          isStatusMode={isStatusMode}
          canManageGroups={canManageGroups}
          canViewAnalytics={canViewAnalytics}
          canViewEndpoints={canViewEndpoints}
          showPricing={showPricing}
          canManageModels={canManageModels}
          onClearFilters={handleClearFilters}
        />
      </Tabs>

      <SupportRequestModal
        isOpen={isSupportModalOpen}
        onClose={() => setIsSupportModalOpen(false)}
        defaultSubject="Model/Feature Request"
      />

      <CreateVirtualModelModal
        isOpen={isCreateVirtualModalOpen}
        onClose={() => setIsCreateVirtualModalOpen(false)}
      />
    </div>
  );
};

export default Models;
