import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { Code, Play, X, Filter, Clock } from "lucide-react";
import { type ColumnDef } from "@tanstack/react-table";
import { Button } from "../../ui/button";
import { DataTable } from "../../ui/data-table";
import { Switch } from "../../ui/switch";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../ui/select";
import { DateTimeRangeSelector } from "../../ui/date-time-range-selector";
import { useAsyncRequests } from "../../../api/control-layer/hooks";
import type {
  AsyncRequest,
  AsyncRequestStatus,
} from "../../../api/control-layer/types";
import { CreateAsyncModal } from "../../modals/CreateAsyncModal/CreateAsyncModal";
import { ApiExamples } from "../../modals";
import { useBootstrapContent } from "../../../hooks/use-bootstrap-content";
import { useServerPagination } from "../../../hooks/useServerPagination";

const getStatusColor = (status: string): string => {
  switch (status) {
    case "completed":
      return "bg-green-100 text-green-800";
    case "failed":
      return "bg-red-100 text-red-800";
    case "processing":
    case "claimed":
      return "bg-blue-100 text-blue-800";
    case "pending":
      return "bg-yellow-100 text-yellow-800";
    case "canceled":
      return "bg-gray-100 text-gray-800";
    default:
      return "bg-gray-100 text-gray-800";
  }
};

const statusLabels: Record<string, string> = {
  processing: "running",
  claimed: "running",
  pending: "queued",
  canceled: "cancelled",
};

function formatDuration(ms: number | null): string {
  if (!ms) return "-";
  const seconds = Math.round(ms / 1000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const remainingSeconds = seconds % 60;
  return `${minutes}m ${remainingSeconds}s`;
}

const columns: ColumnDef<AsyncRequest>[] = [
  {
    accessorKey: "created_at",
    header: "Created",
    cell: ({ row }) => {
      const timestamp = row.getValue("created_at") as string;
      return (
        <span className="text-sm text-doubleword-neutral-900">
          {new Date(timestamp).toLocaleString(undefined, {
            month: "short",
            day: "numeric",
            hour: "numeric",
            minute: "2-digit",
          })}
        </span>
      );
    },
  },
  {
    accessorKey: "model",
    header: "Model",
    cell: ({ row }) => (
      <span className="inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium bg-blue-100 text-blue-800 max-w-full truncate">
        {row.getValue("model")}
      </span>
    ),
  },
  {
    accessorKey: "status",
    header: "Status",
    cell: ({ row }) => {
      const status = row.getValue("status") as string;
      return (
        <span
          className={`inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium ${getStatusColor(status)}`}
        >
          {statusLabels[status] || status}
        </span>
      );
    },
  },
  {
    id: "tokens",
    header: "Tokens",
    cell: ({ row }) => {
      const prompt = row.original.prompt_tokens;
      const completion = row.original.completion_tokens;
      if (prompt == null && completion == null) {
        return <span className="text-sm text-doubleword-neutral-600">-</span>;
      }
      return (
        <span className="text-sm text-doubleword-neutral-900 tabular-nums cursor-help">
          {(prompt ?? 0).toLocaleString()} / {(completion ?? 0).toLocaleString()}
        </span>
      );
    },
  },
  {
    id: "duration",
    header: "Duration",
    cell: ({ row }) => (
      <div className="flex items-center gap-1 text-sm text-doubleword-neutral-900">
        <Clock className="w-3 h-3" />
        {formatDuration(row.original.duration_ms)}
      </div>
    ),
  },
];

export function AsyncRequests() {
  const navigate = useNavigate();
  const [createModalOpen, setCreateModalOpen] = useState(false);
  const [showApiExamples, setShowApiExamples] = useState(false);
  const bootstrapBanner = useBootstrapContent();

  // Filters
  const [statusFilter, setStatusFilter] = useState<
    AsyncRequestStatus | "all"
  >("all");
  const [sortActiveFirst, setSortActiveFirst] = useState(true);
  const [dateRange, setDateRange] = useState<
    { from: Date; to: Date } | undefined
  >(undefined);

  // Server-side offset pagination
  const pagination = useServerPagination({ defaultPageSize: 20 });

  const { data, isLoading } = useAsyncRequests({
    completion_window: "1h",
    active_first: sortActiveFirst || undefined,
    status: statusFilter !== "all" ? statusFilter : undefined,
    created_after: dateRange?.from.toISOString(),
    created_before: dateRange?.to.toISOString(),
    ...pagination.queryParams,
  });

  const requests = data?.data ?? [];
  const totalCount = data?.total_count ?? 0;

  return (
    <div className="py-4 px-6">
      <div className="mb-4 flex flex-col sm:flex-row sm:items-end sm:justify-between gap-4">
        <div>
          <h1 className="text-3xl font-bold text-doubleword-neutral-900">
            Async
          </h1>
          <p className="text-doubleword-neutral-600 mt-1">
            View and manage async requests
          </p>
        </div>
        <div className="flex items-center gap-2">
          <Button variant="outline" onClick={() => setShowApiExamples(true)}>
            <Code className="mr-2 h-4 w-4" />
            API
          </Button>
          <Button variant="outline" onClick={() => setCreateModalOpen(true)}>
            <Play className="w-4 h-4 mr-2" />
            Create Async
          </Button>
        </div>
      </div>

      {/* Bootstrap banner - content from trusted server-side source (bootstrap.js) */}
      {bootstrapBanner.content && !bootstrapBanner.isClosed && (
        <div className="relative mb-6">
          <div
            dangerouslySetInnerHTML={{ __html: bootstrapBanner.content }}
          />
          <button
            onClick={bootstrapBanner.close}
            className="absolute top-3 right-3 rounded-sm opacity-50 transition-opacity hover:opacity-100 focus:ring-2 focus:ring-ring focus:ring-offset-2 focus:outline-hidden"
            aria-label="Close banner"
          >
            <X className="h-4 w-4 text-doubleword-neutral-600" />
          </button>
        </div>
      )}

      <DataTable
        columns={columns}
        data={requests}
        isLoading={isLoading}
        onRowClick={(row) => navigate(`/async/${row.id}`)}
        showColumnToggle={true}
        pageSize={pagination.pageSize}
        minRows={pagination.pageSize}
        rowHeight="40px"
        paginationMode="server"
        serverPagination={{
          page: pagination.page,
          pageSize: pagination.pageSize,
          totalItems: totalCount,
          onPageChange: pagination.handlePageChange,
          onPageSizeChange: pagination.handlePageSizeChange,
        }}
        headerActions={
          <div className="flex items-center gap-2 flex-wrap">
            <div className="flex items-center gap-1.5">
              <Switch
                id="active-first-async"
                checked={sortActiveFirst}
                onCheckedChange={(checked) => {
                  setSortActiveFirst(checked);
                  pagination.handleReset();
                }}
              />
              <label
                htmlFor="active-first-async"
                className="text-sm text-gray-600 cursor-pointer select-none"
              >
                Active first
              </label>
            </div>
            <Select
              value={statusFilter}
              onValueChange={(v) => {
                setStatusFilter(v as AsyncRequestStatus | "all");
                pagination.handleReset();
              }}
            >
              <SelectTrigger className="w-[140px] h-9">
                <div className="flex items-center gap-1.5">
                  <Filter className="w-3.5 h-3.5 text-gray-500" />
                  <SelectValue />
                </div>
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="all">All statuses</SelectItem>
                <SelectItem value="pending">Queued</SelectItem>
                <SelectItem value="processing">Running</SelectItem>
                <SelectItem value="completed">Completed</SelectItem>
                <SelectItem value="failed">Failed</SelectItem>
                <SelectItem value="canceled">Cancelled</SelectItem>
              </SelectContent>
            </Select>
            <DateTimeRangeSelector
              value={dateRange}
              onChange={(range) => {
                setDateRange(range);
                pagination.handleReset();
              }}
            />
            <span className="text-sm text-gray-600">Rows:</span>
            <Select
              value={pagination.pageSize.toString()}
              onValueChange={(value) =>
                pagination.handlePageSizeChange(Number(value))
              }
            >
              <SelectTrigger className="w-20 h-9">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="10">10</SelectItem>
                <SelectItem value="20">20</SelectItem>
                <SelectItem value="50">50</SelectItem>
                <SelectItem value="100">100</SelectItem>
              </SelectContent>
            </Select>
          </div>
        }
        emptyState={
          <div className="text-center py-12">
            <div className="p-4 bg-doubleword-neutral-100 rounded-full w-16 h-16 mx-auto mb-4 flex items-center justify-center">
              <Play className="w-8 h-8 text-doubleword-neutral-600" />
            </div>
            <h3 className="text-lg font-medium text-doubleword-neutral-900 mb-2">
              No async requests found
            </h3>
            <p className="text-doubleword-neutral-600">
              Create your first async request to get started
            </p>
          </div>
        }
      />

      <CreateAsyncModal
        isOpen={createModalOpen}
        onClose={() => setCreateModalOpen(false)}
        onSuccess={() => setCreateModalOpen(false)}
      />

      <ApiExamples
        isOpen={showApiExamples}
        onClose={() => setShowApiExamples(false)}
        defaultTab="async"
      />
    </div>
  );
}
