import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { Code, Play, X } from "lucide-react";
import { type ColumnDef } from "@tanstack/react-table";
import { Button } from "../../ui/button";
import { DataTable } from "../../ui/data-table";
import { useAsyncRequests } from "../../../api/control-layer/hooks";
import type { AsyncRequest } from "../../../api/control-layer/types";
import { CreateAsyncModal } from "../../modals/CreateAsyncModal/CreateAsyncModal";
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
    cell: ({ row }) => {
      if (row.original.status !== "completed")
        return <span className="text-gray-600">—</span>;
      return <span className="text-gray-500">—</span>;
    },
  },
  {
    id: "cost",
    header: "Cost",
    cell: ({ row }) => {
      if (row.original.status !== "completed")
        return <span className="text-gray-600">—</span>;
      return <span className="text-gray-500">—</span>;
    },
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
  const bootstrapBanner = useBootstrapContent();
  const { data, isLoading } = useAsyncRequests({
    completion_window: "1h",
    active_first: true,
    limit: 50,
  });

  const requests = data?.data ?? [];

  // Bootstrap banner content comes from a trusted server-side source (bootstrap.js),
  // same pattern as Batches page — not user-supplied content.
  return (
    <div className="py-4 px-6">
      <div className="mb-6 flex items-center justify-between">
        <h1 className="text-3xl font-bold">Async</h1>
        <div className="flex gap-2">
          <Button variant="outline" onClick={() => {}}>
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
        onRowClick={(row) => navigate(`/workloads/async/${row.id}`)}
      />

      <CreateAsyncModal
        isOpen={createModalOpen}
        onClose={() => setCreateModalOpen(false)}
        onSuccess={() => setCreateModalOpen(false)}
      />
    </div>
  );
}
