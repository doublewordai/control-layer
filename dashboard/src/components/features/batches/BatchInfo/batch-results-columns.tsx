import { type ColumnDef } from "@tanstack/react-table";
import { ArrowUpDown } from "lucide-react";
import type {
  BatchResultItem,
  BatchStatus,
} from "../../../../api/control-layer/types";

const statusConfig = {
  pending: { bg: "bg-gray-100", text: "text-gray-700", label: "Pending" },
  in_progress: {
    bg: "bg-blue-100",
    text: "text-blue-700",
    label: "In Progress",
  },
  completed: {
    bg: "bg-green-100",
    text: "text-green-800",
    label: "Completed",
  },
  failed: { bg: "bg-red-100", text: "text-red-800", label: "Failed" },
  cancelled: { bg: "bg-gray-100", text: "text-gray-700", label: "Cancelled" },
};

const getContentPreview = (
  result: BatchResultItem,
  contentType: "input" | "response",
): string => {
  let content: unknown;

  if (contentType === "input") {
    content = result.input_body;
  } else if (result.response_body) {
    content = result.response_body;
  } else if (result.error) {
    content = { error: result.error };
  }

  const previewText = content
    ? typeof content === "string"
      ? content
      : JSON.stringify(content)
    : "No content";

  return previewText.length > 40
    ? previewText.substring(0, 40) + "..."
    : previewText;
};

export const createBatchResultsColumns = (
  onViewContent: (
    result: BatchResultItem,
    contentType: "input" | "response",
  ) => void,
  batchStatus?: BatchStatus,
): ColumnDef<BatchResultItem>[] => [
  {
    accessorKey: "custom_id",
    header: ({ column }) => {
      return (
        <button
          onClick={() => column.toggleSorting(column.getIsSorted() === "asc")}
          className="flex items-center text-left font-medium group"
        >
          Custom ID
          <ArrowUpDown className="ml-2 h-4 w-4 text-gray-400 group-hover:text-gray-700 transition-colors" />
        </button>
      );
    },
    cell: ({ row }) => {
      const customId = row.getValue("custom_id") as string | null;
      return (
        <span className="font-medium text-gray-900">
          {customId || <span className="text-gray-400 italic">none</span>}
        </span>
      );
    },
  },
  {
    accessorKey: "model",
    header: "Model",
    cell: ({ row }) => {
      return (
        <span className="text-gray-700 font-mono text-xs">
          {row.getValue("model")}
        </span>
      );
    },
  },
  {
    accessorKey: "status",
    header: "Status",
    cell: ({ row }) => {
      let status = row.getValue("status") as BatchResultItem["status"];
      // Show cancelled for pending/in_progress requests when batch is cancelled
      if (
        batchStatus === "cancelled" &&
        (status === "pending" || status === "in_progress")
      ) {
        status = "cancelled";
      }
      const config = statusConfig[status] || statusConfig.pending;
      return (
        <span
          className={`inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium ${config.bg} ${config.text}`}
        >
          {config.label}
        </span>
      );
    },
  },
  {
    id: "input_body",
    header: "Input",
    cell: ({ row }) => {
      const result = row.original;
      return (
        <button
          onClick={() => onViewContent(result, "input")}
          className="text-left text-sm text-gray-700 hover:text-blue-600 transition-colors cursor-pointer font-mono"
          title="Click to view full content"
        >
          {getContentPreview(result, "input")}
        </button>
      );
    },
  },
  {
    id: "response",
    header: "Response",
    cell: ({ row }) => {
      const result = row.original;
      // Show cancelled for pending/in_progress requests when batch is cancelled
      if (
        batchStatus === "cancelled" &&
        (result.status === "pending" || result.status === "in_progress")
      ) {
        return <span className="text-gray-400 italic text-sm">Cancelled</span>;
      }
      // Don't show anything for pending/in_progress requests
      if (result.status === "pending" || result.status === "in_progress") {
        return (
          <span className="text-gray-400 italic text-sm">Processing...</span>
        );
      }
      // Show cancelled message for cancelled requests
      if (result.status === "cancelled") {
        return <span className="text-gray-400 italic text-sm">Cancelled</span>;
      }
      return (
        <button
          onClick={() => onViewContent(result, "response")}
          className="text-left text-sm text-gray-700 hover:text-blue-600 transition-colors cursor-pointer font-mono"
          title="Click to view full content"
        >
          {getContentPreview(result, "response")}
        </button>
      );
    },
  },
];
