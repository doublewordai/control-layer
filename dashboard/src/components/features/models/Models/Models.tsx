import React, { useState, useEffect } from "react";
import { useSearchParams } from "react-router-dom";
import { Search, Activity, LayoutGrid } from "lucide-react";
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
import { ModelsContent } from "./ModelsContent";
import { useEndpoints } from "@/api/control-layer/hooks";
import { useDebounce } from "@/hooks/useDebounce";
import { useServerPagination } from "@/hooks/useServerPagination";

const Models: React.FC = () => {
  const [searchParams, setSearchParams] = useSearchParams();
  const { hasPermission } = useAuthorization();
  const canManageGroups = hasPermission("manage-groups");
  const canViewAnalytics = hasPermission("analytics");
  const canViewEndpoints = hasPermission("endpoints");
  const showPricing = true;

  const [filterProvider, setFilterProvider] = useState("all");
  const [searchQuery, setSearchQuery] = useState(
    searchParams.get("search") || "",
  );
  const debouncedSearch = useDebounce(searchQuery, 300);
  const [showAccessibleOnly, setShowAccessibleOnly] = useState(false);

  // Use pagination hook for URL-based pagination state
  const pagination = useServerPagination({ defaultPageSize: 12 });

  const { data: endpointsData } = useEndpoints();
  const providers = [
    ...new Set(["all", ...(endpointsData || []).map((e) => e.name).sort()]),
  ];

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

  // Reset pagination when search query changes
  useEffect(() => {
    pagination.handleReset();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [debouncedSearch]);

  const viewMode = searchParams.get("view") || "grid";
  const isStatusMode = viewMode === "status";

  const handleTabChange = (value: string) => {
    setSearchParams({ view: value }, { replace: true });
  };

  const handleClearFilters = () => {
    setSearchQuery("");
    setFilterProvider("all");
    setShowAccessibleOnly(false);
  };

  return (
    <div className="p-4 md:p-6">
      <Tabs value={viewMode} onValueChange={handleTabChange}>
        {/* Header */}
        <div className="mb-6">
          <div className="flex flex-col lg:flex-row lg:items-center lg:justify-between gap-4">
            <div>
              <h1 className="text-2xl md:text-3xl font-bold text-doubleword-neutral-900">
                Models
              </h1>
              <p className="text-sm md:text-base text-doubleword-neutral-600 mt-1">
                View and monitor your deployed models
              </p>
            </div>
            <div className="flex flex-col sm:flex-row items-stretch sm:items-center gap-3">
              {/* Access toggle for admins (not shown in status mode) */}
              {!isStatusMode && canManageGroups && (
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

              {/* View mode tabs */}
              <TabsList className="w-full sm:w-auto">
                <TabsTrigger
                  value="grid"
                  className="flex items-center gap-2 flex-1 sm:flex-initial"
                >
                  <LayoutGrid className="h-4 w-4" />
                  Grid
                </TabsTrigger>
                <TabsTrigger
                  value="status"
                  className="flex items-center gap-2 flex-1 sm:flex-initial"
                >
                  <Activity className="h-4 w-4" />
                  Status
                </TabsTrigger>
              </TabsList>
            </div>
          </div>
        </div>

        {/* Content - isolated to prevent header re-renders */}
        <ModelsContent
          pagination={pagination}
          searchQuery={debouncedSearch}
          filterProvider={filterProvider}
          showAccessibleOnly={showAccessibleOnly}
          isStatusMode={isStatusMode}
          canManageGroups={canManageGroups}
          canViewAnalytics={canViewAnalytics}
          canViewEndpoints={canViewEndpoints}
          showPricing={showPricing}
          onClearFilters={handleClearFilters}
        />
      </Tabs>
    </div>
  );
};

export default Models;
