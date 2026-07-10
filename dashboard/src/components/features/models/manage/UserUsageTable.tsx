import React, { useState, useMemo } from "react";
import { type ColumnDef } from "@tanstack/react-table";
import { useRequestsAggregateByUser } from "../../../../api/control-layer";
import { type UserUsage } from "../../../../api/control-layer";
import { DataTable } from "../../../ui/data-table";
import { Button } from "../../../ui/button";
import {
  HoverCard,
  HoverCardContent,
  HoverCardTrigger,
} from "../../../ui/hover-card";
import { ArrowUpDown, Users, Download } from "lucide-react";

// Usage is served from a per-UTC-day rollup (COR-516: get_model_user_usage reads
// user_model_usage_daily), so this table offers whole-UTC-day ranges only — no
// sub-day custom picker. Each value spans N whole UTC days (today inclusive),
// mirroring the /usage page.
const RANGE_DAYS: Record<string, number> = { "1d": 1, "7d": 7, "30d": 30, "90d": 90 };
const RANGE_OPTIONS = [
  { value: "1d", label: "Today" },
  { value: "7d", label: "7d" },
  { value: "30d", label: "30d" },
  { value: "90d", label: "90d" },
];

interface UserUsageTableProps {
  modelAlias: string;
  showPricing?: boolean;
}

const UserUsageTable: React.FC<UserUsageTableProps> = ({
  modelAlias,
  showPricing = true,
}) => {
  const [rangeKey, setRangeKey] = useState("7d");

  // Align the window to whole UTC days (the rollup buckets by UTC usage_date):
  // start at midnight UTC of (today - (days - 1)), end at "now". The backend
  // truncates both bounds to their UTC date.
  const { startDate, endDate } = useMemo(() => {
    const days = RANGE_DAYS[rangeKey] ?? 7;
    const now = new Date();
    const startUtcMidnight = new Date(
      Date.UTC(
        now.getUTCFullYear(),
        now.getUTCMonth(),
        now.getUTCDate() - (days - 1),
      ),
    );
    return {
      startDate: startUtcMidnight.toISOString(),
      endDate: now.toISOString(),
    };
  }, [rangeKey]);

  const { data, isLoading, error } = useRequestsAggregateByUser(
    modelAlias,
    startDate,
    endDate,
  );

  // Formatting helpers
  const formatNumber = (num: number) => {
    return new Intl.NumberFormat().format(num);
  };

  const formatCost = (cost?: number) => {
    if (cost === undefined || cost === null) return "-";
    return `$${cost.toFixed(4)}`;
  };

  const formatDate = (dateStr?: string) => {
    if (!dateStr) return "-";
    const date = new Date(dateStr);
    const now = new Date();
    // last_active_at is day-granular — the UTC midnight of MAX(usage_date)
    // (COR-516), not an exact timestamp — so compare by whole UTC day rather
    // than elapsed hours. Otherwise same-day usage viewed just after UTC
    // midnight reads "1d ago" instead of "Today".
    const utcDay = (d: Date) =>
      Math.floor(
        Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate()) /
          86_400_000,
      );
    const dayDiff = utcDay(now) - utcDay(date);

    if (dayDiff <= 0) return "Today";
    if (dayDiff === 1) return "Yesterday";
    if (dayDiff < 30) return `${dayDiff}d ago`;
    return date.toLocaleDateString();
  };

  // Download CSV function
  const downloadCSV = () => {
    if (!data || !data.users || data.users.length === 0) return;

    // Create CSV headers
    const headers = [
      "User Email",
      "User ID",
      "Requests",
      "Input Tokens",
      "Output Tokens",
      "Total Tokens",
      ...(showPricing ? ["Total Cost"] : []),
      "Last Active",
    ];

    // Create CSV rows
    const rows = data.users.map((user) => [
      user.user_email || "",
      user.user_id || "",
      user.request_count.toString(),
      user.input_tokens.toString(),
      user.output_tokens.toString(),
      user.total_tokens.toString(),
      ...(showPricing ? [user.total_cost?.toString() || "0"] : []),
      user.last_active_at || "",
    ]);

    // Combine headers and rows
    const csvContent = [
      headers.join(","),
      ...rows.map((row) => row.map((cell) => `"${cell}"`).join(",")),
    ].join("\n");

    // Create blob and download
    const blob = new Blob([csvContent], { type: "text/csv;charset=utf-8;" });
    const link = document.createElement("a");
    const url = URL.createObjectURL(blob);

    const fileName = `${modelAlias}_usage_${new Date().toISOString().split("T")[0]}.csv`;

    link.setAttribute("href", url);
    link.setAttribute("download", fileName);
    link.style.visibility = "hidden";
    document.body.appendChild(link);
    link.click();
    document.body.removeChild(link);
  };

  // Define columns for the data table
  const columns: ColumnDef<UserUsage>[] = useMemo(
    () => [
      {
        accessorKey: "user_email",
        header: "User",
        cell: ({ row }) => {
          const user = row.original;
          return (
            <div>
              <p className="font-medium">
                {user.user_email || user.user_id || "Anonymous"}
              </p>
              {user.user_id && user.user_email && (
                <p className="text-xs text-gray-500">{user.user_id}</p>
              )}
            </div>
          );
        },
      },
      {
        accessorKey: "request_count",
        header: ({ column }) => {
          return (
            <Button
              variant="ghost"
              size="sm"
              className="-ml-3 h-8"
              onClick={() =>
                column.toggleSorting(column.getIsSorted() === "asc")
              }
            >
              Requests
              <ArrowUpDown className="ml-2 h-4 w-4" />
            </Button>
          );
        },
        cell: ({ row }) => formatNumber(row.getValue("request_count")),
      },
      {
        id: "tokens",
        header: "Tokens",
        cell: ({ row }) => {
          const user = row.original;
          return (
            <span className="text-sm text-doubleword-neutral-900">
              {formatNumber(user.input_tokens)} in/
              {formatNumber(user.output_tokens)} out
            </span>
          );
        },
      },
      ...(showPricing
        ? ([
            {
              accessorKey: "total_cost" as const,
              header: ({ column }: { column: any }) => {
                return (
                  <Button
                    variant="ghost"
                    size="sm"
                    className="-ml-3 h-8"
                    onClick={() =>
                      column.toggleSorting(column.getIsSorted() === "asc")
                    }
                  >
                    Cost
                    <ArrowUpDown className="ml-2 h-4 w-4" />
                  </Button>
                );
              },
              cell: ({ row }: { row: any }) => (
                <span className="font-medium text-green-700">
                  {formatCost(row.getValue("total_cost"))}
                </span>
              ),
            },
          ] as ColumnDef<UserUsage>[])
        : []),
      {
        accessorKey: "last_active_at",
        header: "Last Active",
        cell: ({ row }) => formatDate(row.getValue("last_active_at")),
      },
    ],
    [showPricing],
  );

  if (isLoading) {
    return (
      <div className="flex items-center justify-center p-8">
        <div
          className="animate-spin rounded-full h-8 w-8 border-b-2 border-doubleword-accent-blue"
          aria-label="Loading"
        />
      </div>
    );
  }

  if (error) {
    return (
      <div className="p-8 text-center">
        <p className="text-red-600">
          Error loading usage data: {(error as Error).message}
        </p>
      </div>
    );
  }

  return (
    <div className="space-y-4">
      {/* Header with date range selector and stats */}
      <div className="flex flex-col lg:flex-row lg:items-center lg:justify-between gap-4">
        <div>
          <h3 className="text-lg font-semibold">User Usage Statistics</h3>
          <p className="text-sm text-muted-foreground">
            Usage breakdown by user for {modelAlias}
          </p>
        </div>
        <div className="flex flex-col sm:flex-row items-stretch sm:items-center gap-3">
          {data && data.users.length > 0 && (
            <div className="flex flex-wrap gap-2">
              <HoverCard openDelay={100} closeDelay={50}>
                <HoverCardTrigger asChild>
                  <div className="flex items-center gap-1.5 px-3 py-1.5 bg-muted/50 rounded-md select-none cursor-default">
                    <span className="text-xs text-muted-foreground">
                      Users:
                    </span>
                    <span className="text-sm font-semibold">
                      {data.users.length}
                    </span>
                  </div>
                </HoverCardTrigger>
                <HoverCardContent className="w-64" sideOffset={5}>
                  <p className="text-sm text-muted-foreground">
                    Number of unique authenticated users who have made requests
                    to this model in the selected time period.
                  </p>
                </HoverCardContent>
              </HoverCard>

              <HoverCard openDelay={100} closeDelay={50}>
                <HoverCardTrigger asChild>
                  <div className="flex items-center gap-1.5 px-3 py-1.5 bg-muted/50 rounded-md select-none cursor-default">
                    <span className="text-xs text-muted-foreground">
                      Avg Requests:
                    </span>
                    <span className="text-sm font-semibold">
                      {formatNumber(
                        Math.round(data.total_requests / data.users.length),
                      )}
                    </span>
                  </div>
                </HoverCardTrigger>
                <HoverCardContent className="w-64" sideOffset={5}>
                  <p className="text-sm text-muted-foreground">
                    Average number of requests per user.
                  </p>
                </HoverCardContent>
              </HoverCard>

              <HoverCard openDelay={100} closeDelay={50}>
                <HoverCardTrigger asChild>
                  <div className="flex items-center gap-1.5 px-3 py-1.5 bg-muted/50 rounded-md select-none cursor-default">
                    <span className="text-xs text-muted-foreground">
                      Avg Tokens:
                    </span>
                    <span className="text-sm font-semibold">
                      {formatNumber(
                        Math.round(data.total_tokens / data.users.length),
                      )}
                    </span>
                  </div>
                </HoverCardTrigger>
                <HoverCardContent className="w-64" sideOffset={5}>
                  <p className="text-sm text-muted-foreground">
                    Average token consumption per user. Shows the mean number of
                    tokens (input + output) processed per user.
                  </p>
                </HoverCardContent>
              </HoverCard>

              {showPricing && data.total_cost !== undefined && (
                <HoverCard openDelay={100} closeDelay={50}>
                  <HoverCardTrigger asChild>
                    <div className="flex items-center gap-1.5 px-3 py-1.5 bg-green-50 rounded-md select-none cursor-default border border-green-200">
                      <span className="text-xs text-green-700 font-medium">
                        Total Cost:
                      </span>
                      <span className="text-sm font-bold text-green-800">
                        {formatCost(data.total_cost)}
                      </span>
                    </div>
                  </HoverCardTrigger>
                  <HoverCardContent className="w-64" sideOffset={5}>
                    <p className="text-sm text-muted-foreground">
                      Total cost for all requests in the selected time period,
                      calculated based on token usage and model pricing.
                    </p>
                  </HoverCardContent>
                </HoverCard>
              )}
            </div>
          )}

          <div className="flex items-center gap-1 rounded-md border bg-muted/50 p-1">
            {RANGE_OPTIONS.map((opt) => (
              <Button
                key={opt.value}
                variant={rangeKey === opt.value ? "secondary" : "ghost"}
                size="sm"
                className="h-7 px-3"
                aria-pressed={rangeKey === opt.value}
                onClick={() => setRangeKey(opt.value)}
              >
                {opt.label}
              </Button>
            ))}
          </div>
        </div>
      </div>

      {/* User table */}
      {data && data.users.length > 0 ? (
        <DataTable
          columns={columns}
          data={data.users}
          searchPlaceholder="Search users..."
          pageSize={10}
          headerActions={
            <Button variant="outline" size="sm" onClick={downloadCSV}>
              <Download className="h-4 w-4 mr-1.5" />
              Export CSV
            </Button>
          }
        />
      ) : (
        <div className="flex flex-col items-center justify-center h-64 bg-muted/30 rounded-lg">
          <Users className="h-12 w-12 text-muted-foreground mb-3" />
          <h3 className="text-lg font-medium text-doubleword-neutral-800 mb-2">
            No User Data Available
          </h3>
          <p className="text-doubleword-neutral-600 text-center max-w-md">
            No authenticated user activity found for this model in the selected
            time period. Data will appear here once users start making requests.
          </p>
        </div>
      )}
    </div>
  );
};

export default UserUsageTable;
