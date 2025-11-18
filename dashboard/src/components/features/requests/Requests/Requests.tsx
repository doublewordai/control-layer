import { Activity, X, BarChart3, List, ArrowLeft } from "lucide-react";
import { useState, useEffect } from "react";
import * as React from "react";
import { useSearchParams, useNavigate } from "react-router-dom";
import {
  useRequests,
  useRequestsAggregate,
} from "../../../../api/control-layer";
import {
  useMockRequests,
  useMockAggregateData,
} from "../../../../api/demo/mockRequests";
import { useSettings } from "../../../../contexts";
import { transformRequestResponsePairs } from "../../../../utils";
import { useAuthorization } from "../../../../utils/authorization";
import { DataTable } from "../../../ui/data-table";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "../../../ui/tabs";
import { Combobox } from "../../../ui/combobox";
import { DateTimeRangeSelector } from "../../../ui/date-time-range-selector";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../../ui/select";
import { RequestsAnalytics } from "../RequestsAnalytics";
import { createRequestColumns } from "./columns";

export function Requests() {
  const { isFeatureEnabled } = useSettings();
  const isDemoMode = isFeatureEnabled("demo");
  const { userRoles } = useAuthorization();
  const [searchParams] = useSearchParams();
  const navigate = useNavigate();
  const [selectedModel, setSelectedModel] = useState<string | undefined>(
    undefined,
  );
  // Initialize with last 24 hours as default
  const getDefaultDateRange = () => {
    const now = new Date();
    const from = new Date(now.getTime() - 24 * 60 * 60 * 1000);
    return { from, to: now };
  };
  const [dateRange, setDateRange] = useState<
    { from: Date; to: Date } | undefined
  >(getDefaultDateRange());

  // Initialize pagination state from URL parameters
  const pageFromUrl = searchParams.get("page");
  const pageSizeFromUrl = searchParams.get("pageSize");
  const [currentPage, setCurrentPage] = useState<number>(
    pageFromUrl ? parseInt(pageFromUrl, 10) : 0,
  );
  const [pageSize, setPageSize] = useState<number>(
    pageSizeFromUrl ? parseInt(pageSizeFromUrl, 10) : 10,
  );

  // Check user permissions
  const hasAnalyticsPermission = userRoles.some(
    (role) => role === "PlatformManager" || role === "RequestViewer",
  );
  const hasRequestsPermission = userRoles.some(
    (role) => role === "RequestViewer",
  );

  // Initialize selectedModel from URL parameter
  useEffect(() => {
    const modelFromUrl = searchParams.get("model");
    if (modelFromUrl) {
      setSelectedModel(modelFromUrl);
    }
  }, [searchParams]);

  // Update activeTab when URL changes (e.g., browser back/forward)
  useEffect(() => {
    const tabFromUrl = searchParams.get("tab");
    if (
      tabFromUrl &&
      (tabFromUrl === "analytics" || tabFromUrl === "requests")
    ) {
      setActiveTab(tabFromUrl);
    }
  }, [searchParams]);

  // Get tab from URL or permissions
  const tabFromUrl = searchParams.get("tab");
  const [activeTab, setActiveTab] = useState<string>(() => {
    return tabFromUrl &&
      (tabFromUrl === "analytics" || tabFromUrl === "requests")
      ? tabFromUrl
      : hasAnalyticsPermission
        ? "analytics"
        : "requests";
  });

  // Sync tab state with URL
  const handleTabChange = (value: string) => {
    setActiveTab(value);
    const newParams = new URLSearchParams(searchParams);
    newParams.set("tab", value);
    navigate(`/analytics?${newParams.toString()}`, { replace: true });
  };

  // Get from parameter for back navigation
  const fromUrl = searchParams.get("from");

  // Calculate offset for server-side pagination
  const offset = currentPage * pageSize;

  // Fetch requests data only if user has requests permission
  const realRequestsQuery = useRequests(
    { limit: pageSize, offset, order_desc: true },
    { enabled: hasRequestsPermission },
    dateRange,
  );
  const mockRequestsQuery = useMockRequests(
    { limit: pageSize, offset, order_desc: true },
    { enabled: hasRequestsPermission },
    dateRange,
  );

  // Choose data source based on demo mode
  const {
    data: requestsResponse,
    isLoading: requestsLoading,
    error: requestsError,
  } = isDemoMode ? mockRequestsQuery : realRequestsQuery;

  // Fetch models that have received requests from analytics
  const realAnalyticsQuery = useRequestsAggregate(undefined, dateRange);
  const mockAnalyticsQuery = useMockAggregateData(undefined, dateRange);
  const { data: analyticsData } = isDemoMode
    ? mockAnalyticsQuery
    : realAnalyticsQuery;
  const modelsWithRequests = analyticsData?.models || [];

  const loading = requestsLoading;
  const error = requestsError;

  // Transform backend data to frontend format
  const requests = requestsResponse
    ? transformRequestResponsePairs(requestsResponse.requests)
    : [];

  // Get total count for pagination
  const totalCount = requestsResponse?.total_count || 0;
  const totalPages = Math.ceil(totalCount / pageSize);

  // Sync pagination state with URL
  React.useEffect(() => {
    const newParams = new URLSearchParams(searchParams);
    newParams.set("page", currentPage.toString());
    newParams.set("pageSize", pageSize.toString());
    navigate(`/analytics?${newParams.toString()}`, { replace: true });
  }, [currentPage, pageSize, navigate, searchParams]);

  // Reset to first page when filters change
  React.useEffect(() => {
    setCurrentPage(0);
  }, [selectedModel, dateRange]);

  const columns = createRequestColumns();

  // Show loading state
  if (loading) {
    return (
      <div className="py-4 px-6">
        <div className="mb-4">
          <h1 className="text-3xl font-bold text-doubleword-neutral-900">
            Traffic
          </h1>
          <p className="text-doubleword-neutral-600 mt-2">
            Loading traffic data...
          </p>
        </div>
        <div className="flex items-center justify-center h-64">
          <div className="text-center">
            <div
              className="animate-spin rounded-full h-12 w-12 border-b-2 border-doubleword-accent-blue mx-auto mb-4"
              role="progressbar"
              aria-label="Loading"
            ></div>
            <p className="text-doubleword-neutral-600">Loading...</p>
          </div>
        </div>
      </div>
    );
  }

  // Show general error
  if (error) {
    return (
      <div className="py-4 px-6">
        <div className="mb-4">
          <h1 className="text-3xl font-bold text-doubleword-neutral-900">
            Traffic
          </h1>
        </div>
        <div className="flex items-center justify-center h-64">
          <div className="text-center">
            <div className="text-red-500 mb-4">
              <X className="h-12 w-12 mx-auto" />
            </div>
            <h3 className="text-lg font-medium text-red-600 mb-2">
              Error Loading Data
            </h3>
            <p className="text-red-600">
              {error instanceof Error
                ? error.message
                : "Failed to load request data"}
            </p>
          </div>
        </div>
      </div>
    );
  }

  // If user has no permissions, show access denied
  if (!hasAnalyticsPermission && !hasRequestsPermission) {
    return (
      <div className="py-4 px-6">
        <div className="mb-4">
          <h1 className="text-3xl font-bold text-doubleword-neutral-900">
            Traffic
          </h1>
        </div>
        <div className="flex items-center justify-center h-64">
          <div className="text-center">
            <div className="text-red-500 mb-4">
              <X className="h-12 w-12 mx-auto" />
            </div>
            <h3 className="text-lg font-medium text-red-600 mb-2">
              Access Denied
            </h3>
            <p className="text-red-600">
              You don't have permission to view traffic data.
            </p>
          </div>
        </div>
      </div>
    );
  }

  // Main render with tabs
  return (
    <div className="py-4 px-6">
      <Tabs
        value={activeTab}
        onValueChange={handleTabChange}
        className="space-y-4"
      >
        <div className="mb-4 flex flex-col sm:flex-row sm:items-end sm:justify-between gap-4">
          <div className="flex items-center gap-4">
            {fromUrl && (
              <button
                onClick={() => navigate(fromUrl)}
                className="p-2 text-gray-500 hover:bg-gray-100 rounded-lg transition-colors"
                aria-label="Go back"
                title="Go back"
              >
                <ArrowLeft className="w-5 h-5" />
              </button>
            )}
            <div>
              <h1 className="text-3xl font-bold text-doubleword-neutral-900">
                Traffic
              </h1>
            </div>
          </div>

          <div className="flex flex-col sm:flex-row items-start sm:items-center gap-4">
            <DateTimeRangeSelector
              value={dateRange}
              onChange={setDateRange}
              className="w-full sm:w-auto"
            />

            <Combobox
              options={[
                { value: "all", label: "All Models" },
                ...modelsWithRequests.map((model) => ({
                  value: model.model,
                  label: model.model,
                })),
              ]}
              value={selectedModel || "all"}
              onValueChange={(value) => {
                const newModel = value === "all" ? undefined : value;
                setSelectedModel(newModel);

                // Update URL parameters
                const newParams = new URLSearchParams(searchParams);
                if (newModel) {
                  newParams.set("model", newModel);
                } else {
                  newParams.delete("model");
                }
                navigate(`/analytics?${newParams.toString()}`, {
                  replace: true,
                });
              }}
              placeholder="Select model..."
              searchPlaceholder="Search models..."
              emptyMessage="No models with requests found."
              className="w-full sm:w-[200px]"
            />

            <TabsList className="w-full sm:w-auto">
              {hasAnalyticsPermission && (
                <TabsTrigger
                  value="analytics"
                  className="flex items-center gap-2 flex-1 sm:flex-initial"
                >
                  <BarChart3 className="h-4 w-4" />
                  Analytics
                </TabsTrigger>
              )}
              {hasRequestsPermission && (
                <TabsTrigger
                  value="requests"
                  className="flex items-center gap-2 flex-1 sm:flex-initial"
                >
                  <List className="h-4 w-4" />
                  Requests
                </TabsTrigger>
              )}
            </TabsList>
          </div>
        </div>

        {hasRequestsPermission && (
          <TabsContent value="requests" className="space-y-4">
            {!requests || requests.length === 0 ? (
              <div className="text-center py-12">
                <div className="p-4 bg-doubleword-neutral-100 rounded-full w-16 h-16 mx-auto mb-4 flex items-center justify-center">
                  <Activity className="w-8 h-8 text-doubleword-neutral-600" />
                </div>
                <h3 className="text-lg font-medium text-doubleword-neutral-900 mb-2">
                  No requests found
                </h3>
                <p className="text-doubleword-neutral-600">
                  No requests found for the selected time period. Try adjusting
                  the date range or check back later once traffic starts flowing
                  through the gateway.
                </p>
              </div>
            ) : (
              <DataTable
                columns={columns}
                data={requests}
                searchPlaceholder="Search requests and responses..."
                showPagination={true}
                showColumnToggle={true}
                pageSize={pageSize}
                rowHeight="40px"
                initialColumnVisibility={{ timestamp: false }}
                serverSidePagination={{
                  currentPage,
                  totalPages,
                  totalCount,
                  onPageChange: setCurrentPage,
                }}
                headerActions={
                  <Select
                    value={pageSize.toString()}
                    onValueChange={(value) => {
                      setPageSize(parseInt(value, 10));
                      setCurrentPage(0); // Reset to first page when changing page size
                    }}
                  >
                    <SelectTrigger className="w-[130px] h-9">
                      <SelectValue placeholder="Page size" />
                    </SelectTrigger>
                    <SelectContent>
                      <SelectItem value="10">10 per page</SelectItem>
                      <SelectItem value="25">25 per page</SelectItem>
                      <SelectItem value="50">50 per page</SelectItem>
                      <SelectItem value="100">100 per page</SelectItem>
                    </SelectContent>
                  </Select>
                }
              />
            )}
          </TabsContent>
        )}

        {hasAnalyticsPermission && (
          <TabsContent value="analytics" className="space-y-4">
            <RequestsAnalytics
              selectedModel={selectedModel}
              dateRange={dateRange}
            />
          </TabsContent>
        )}
      </Tabs>
    </div>
  );
}
