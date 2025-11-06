"use client";

import { type ColumnDef } from "@tanstack/react-table";
import {
  ArrowUpDown,
  MoreHorizontal,
  Download,
  Eye,
  XCircle,
  Clock,
  CheckCircle2,
  AlertCircle,
  Loader2,
} from "lucide-react";
import { Button } from "../../../ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "../../../ui/dropdown-menu";
import { Progress } from "../../../ui/progress"; 
import { formatTimestamp, formatDuration } from "../../../../utils";
import type { Batch, BatchStatus } from "../types";

interface ColumnActions {
  onView: (batch: Batch) => void;
  onCancel: (batch: Batch) => void;
  onDownload: (batch: Batch) => void;
}

const getStatusIcon = (status: BatchStatus) => {
  switch (status) {
    case "completed":
      return <CheckCircle2 className="w-4 h-4 text-green-600" />;
    case "failed":
      return <AlertCircle className="w-4 h-4 text-red-600" />;
    case "cancelled":
      return <XCircle className="w-4 h-4 text-gray-600" />;
    case "in_progress":
    case "finalizing":
      return <Loader2 className="w-4 h-4 text-blue-600 animate-spin" />;
    case "validating":
    case "cancelling":
      return <Clock className="w-4 h-4 text-yellow-600" />;
    default:
      return <Clock className="w-4 h-4 text-gray-600" />;
  }
};

const getStatusColor = (status: BatchStatus) => {
  switch (status) {
    case "completed":
      return "bg-green-100 text-green-800";
    case "failed":
      return "bg-red-100 text-red-800";
    case "cancelled":
      return "bg-gray-100 text-gray-800";
    case "in_progress":
    case "finalizing":
      return "bg-blue-100 text-blue-800";
    case "validating":
    case "cancelling":
      return "bg-yellow-100 text-yellow-800";
    case "expired":
      return "bg-orange-100 text-orange-800";
    default:
      return "bg-gray-100 text-gray-800";
  }
};

export const createBatchColumns = (
  actions: ColumnActions,
): ColumnDef<Batch>[] => [
  {
    accessorKey: "id",
    header: ({ column }) => {
      return (
        <button
          onClick={() => column.toggleSorting(column.getIsSorted() === "asc")}
          className="flex items-center text-left font-medium group"
        >
          Batch ID
          <ArrowUpDown className="ml-2 h-4 w-4 text-gray-400 group-hover:text-gray-700 transition-colors" />
        </button>
      );
    },
    cell: ({ row }) => {
      const id = row.getValue("id") as string;
      return <span className="font-mono text-xs text-gray-600">{id}</span>;
    },
  },
  {
    accessorKey: "status",
    header: "Status",
    cell: ({ row }) => {
      const status = row.getValue("status") as BatchStatus;
      return (
        <div className="flex items-center gap-2">
          {getStatusIcon(status)}
          <span
            className={`inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium ${getStatusColor(status)}`}
          >
            {status.replace("_", " ")}
          </span>
        </div>
      );
    },
  },
  {
    accessorKey: "request_counts",
    header: "Progress",
    cell: ({ row }) => {
      const batch = row.original;
      const { total, completed, failed } = batch.request_counts;
      const progress = total > 0 ? ((completed + failed) / total) * 100 : 0;

      return (
        <div className="space-y-1 min-w-[200px]">
          <div className="flex justify-between text-xs text-gray-600">
            <span>
              {completed + failed} / {total}
            </span>
            <span>{Math.round(progress)}%</span>
          </div>
          <Progress value={progress} className="h-2" />
          <div className="flex gap-3 text-xs text-gray-500">
            <span className="text-green-600">{completed} completed</span>
            {failed > 0 && <span className="text-red-600">{failed} failed</span>}
          </div>
        </div>
      );
    },
  },
  {
    accessorKey: "endpoint",
    header: "Endpoint",
    cell: ({ row }) => {
      const endpoint = row.getValue("endpoint") as string;
      return (
        <span className="font-mono text-xs text-gray-700">{endpoint}</span>
      );
    },
  },
  {
    accessorKey: "created_at",
    header: ({ column }) => {
      return (
        <button
          onClick={() => column.toggleSorting(column.getIsSorted() === "asc")}
          className="flex items-center text-left font-medium group"
        >
          Created
          <ArrowUpDown className="ml-2 h-4 w-4 text-gray-400 group-hover:text-gray-700 transition-colors" />
        </button>
      );
    },
    cell: ({ row }) => {
      const timestamp = row.getValue("created_at") as number;
      return (
        <span className="text-gray-700">
          {formatTimestamp(new Date(timestamp * 1000).toISOString())}
        </span>
      );
    },
  },
  {
    id: "duration",
    header: "Duration",
    cell: ({ row }) => {
      const batch = row.original;
      
      if (batch.status === "validating" || !batch.in_progress_at) {
        return <span className="text-gray-400">-</span>;
      }

      const startTime = batch.in_progress_at * 1000;
      const endTime = batch.completed_at
        ? batch.completed_at * 1000
        : batch.failed_at
          ? batch.failed_at * 1000
          : batch.cancelled_at
            ? batch.cancelled_at * 1000
            : Date.now();

      const duration = endTime - startTime;

      return (
        <div className="flex items-center gap-1 text-sm text-gray-700">
          <Clock className="w-3 h-3" />
          {formatDuration(duration)}
        </div>
      );
    },
  },
  {
    id: "actions",
    cell: ({ row }) => {
      const batch = row.original;
      const canCancel = ["validating", "in_progress", "finalizing"].includes(
        batch.status,
      );
      const canDownload = batch.status === "completed" && batch.output_file_id;

      return (
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button variant="ghost" className="h-8 w-8 p-0">
              <span className="sr-only">Open menu</span>
              <MoreHorizontal className="h-4 w-4" />
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end">
            <DropdownMenuLabel>Actions</DropdownMenuLabel>
            <DropdownMenuItem onClick={() => actions.onView(batch)}>
              <Eye className="mr-2 h-4 w-4" />
              View Requests
            </DropdownMenuItem>
            {canDownload && (
              <DropdownMenuItem onClick={() => actions.onDownload(batch)}>
                <Download className="mr-2 h-4 w-4" />
                Download Results
              </DropdownMenuItem>
            )}
            {canCancel && (
              <>
                <DropdownMenuSeparator />
                <DropdownMenuItem
                  onClick={() => actions.onCancel(batch)}
                  className="text-red-600"
                >
                  <XCircle className="mr-2 h-4 w-4" />
                  Cancel Batch
                </DropdownMenuItem>
              </>
            )}
          </DropdownMenuContent>
        </DropdownMenu>
      );
    },
  },
];