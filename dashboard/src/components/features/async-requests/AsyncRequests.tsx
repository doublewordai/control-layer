import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { Code, Play, X, Filter } from "lucide-react";
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
import { cn } from "../../../lib/utils";

const statusStyles: Record<string, string> = {
  completed: "bg-green-500/10 text-green-400",
  failed: "bg-red-500/10 text-red-400",
  processing: "bg-blue-500/10 text-blue-400",
  claimed: "bg-blue-500/10 text-blue-400",
  pending: "bg-yellow-500/10 text-yellow-400",
  canceled: "bg-gray-500/10 text-gray-400",
};

const statusLabels: Record<string, string> = {
  processing: "running",
  claimed: "running",
  pending: "queued",
  canceled: "cancelled",
};

function formatDuration(ms: number | null): string {
  if (!ms) return "—";
  const seconds = Math.round(ms / 1000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const remainingSeconds = seconds % 60;
  return `${minutes}m ${remainingSeconds}s`;
}

function formatTokens(
  prompt: number | null,
  completion: number | null,
): string {
  if (prompt == null && completion == null) return "—";
  const p = prompt?.toLocaleString() ?? "—";
  const c = completion?.toLocaleString() ?? "—";
  return `${p} / ${c}`;
}

const columns: ColumnDef<AsyncRequest>[] = [
  {
    accessorKey: "created_at",
    header: "Created",
    cell: ({ row }) => {
      const timestamp = row.getValue("created_at") as string;
      return (
        <span className="text-gray-500">
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
    cell: ({ row }) => <span>{row.getValue("model")}</span>,
  },
  {
    accessorKey: "status",
    header: "Status",
    cell: ({ row }) => {
      const status = row.getValue("status") as string;
      return (
        <span
          className={cn(
            "inline-flex items-center rounded px-2 py-0.5 text-xs font-medium",
            statusStyles[status] || "bg-gray-500/10 text-gray-400",
          )}
        >
          {statusLabels[status] || status}
        </span>
      );
    },
  },
  {
    id: "tokens",
    header: "Tokens",
    cell: ({ row }) => (
      <span className="text-gray-500 text-xs tabular-nums">
        {formatTokens(
          row.original.prompt_tokens,
          row.original.completion_tokens,
        )}
      </span>
    ),
  },
  {
    id: "duration",
    header: "Duration",
    cell: ({ row }) => (
      <span className="text-gray-500">
        {formatDuration(row.original.duration_ms)}
      </span>
    ),
  },
  {
    id: "actions",
    header: "",
    cell: () => (
      <span className="text-gray-500 cursor-pointer text-xs">View →</span>
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
  const [pageSize, setPageSize] = useState(20);

  const { data, isLoading } = useAsyncRequests({
    completion_window: "1h",
    active_first: sortActiveFirst || undefined,
    status: statusFilter !== "all" ? statusFilter : undefined,
    created_after: dateRange?.from.toISOString(),
    created_before: dateRange?.to.toISOString(),
    limit: pageSize,
  });

  const requests = data?.data ?? [];

  // Bootstrap banner content comes from a trusted server-side source (bootstrap.js),
  // same pattern as Batches page — not user-supplied content.
  return (
    <div className="py-4 px-6">
      <div className="mb-6 flex items-center justify-between">
        <div>
          <h1 className="text-3xl font-bold">Async</h1>
          <p className="text-neutral-600 mt-1">
            View and manage async requests
          </p>
        </div>
        <div className="flex gap-2">
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
        pageSize={pageSize}
        minRows={pageSize}
        rowHeight="40px"
        headerActions={
          <div className="flex items-center gap-2 flex-wrap">
            <div className="flex items-center gap-1.5">
              <Switch
                id="active-first-async"
                checked={sortActiveFirst}
                onCheckedChange={setSortActiveFirst}
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
              onValueChange={(v) =>
                setStatusFilter(v as AsyncRequestStatus | "all")
              }
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
            <DateTimeRangeSelector value={dateRange} onChange={setDateRange} />
            <span className="text-sm text-gray-600">Rows:</span>
            <Select
              value={pageSize.toString()}
              onValueChange={(v) => setPageSize(Number(v))}
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
