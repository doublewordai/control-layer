"use client";

import { type ColumnDef } from "@tanstack/react-table";
import {
  ArrowUpDown,
  XCircle,
  Clock,
  AlertCircle,
  FileCheck,
  Eye,
  Trash2,
  Box,
  Zap,
} from "lucide-react";
import { Button } from "../../../ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "../../../ui/tooltip";
import {
  formatTimestamp,
  formatLongDuration,
  formatNumber,
  copyToClipboard,
} from "../../../../utils";
import type { Batch, BatchStatus } from "../types";

interface ColumnActions {
  onCancel: (batch: Batch) => void;
  onDelete: (batch: Batch) => void;
  getBatchFiles: (batch: Batch) => any[];
  onViewFile: (file: any) => void;
  getInputFile: (batch: Batch) => any | undefined;
  onRowClick?: (batch: Batch) => void;
  /** Show the User column (PlatformManagers or org context) */
  showUserColumn?: boolean;
  /** Show the Type column (hidden when "Batch only" filter is on) */
  showTypeColumn?: boolean;
}

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

const userColumn: ColumnDef<Batch> = {
  id: "user",
  header: "User",
  cell: ({ row }) => {
    const batch = row.original as Batch;
    const email = batch.metadata?.created_by_email;
    if (!email) {
      return <span className="text-gray-400">-</span>;
    }
    return (
      <Tooltip delayDuration={300}>
        <TooltipTrigger asChild>
          <span
            onClick={(e) => {
              e.stopPropagation();
              void copyToClipboard(email, { successMessage: "Copied user" });
            }}
            className="text-sm text-gray-700 truncate max-w-[120px] inline-block align-middle cursor-pointer hover:text-gray-900 transition-colors"
          >
            {email}
          </span>
        </TooltipTrigger>
        <TooltipContent>{email}</TooltipContent>
      </Tooltip>
    );
  },
};

export const createBatchColumns = (
  actions: ColumnActions,
): ColumnDef<Batch>[] => [
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
  ...(actions.showUserColumn ? [userColumn] : []),
  ...(actions.showTypeColumn !== false
    ? [
        {
          id: "type",
          header: "Type",
          cell: ({ row }: { row: { original: Batch } }) => {
            const batch = row.original;
            const isAsync = batch.completion_window !== "24h";
            return (
              <Tooltip>
                <TooltipTrigger asChild>
                  <span className="inline-flex text-doubleword-neutral-600">
                    {isAsync ? (
                      <Zap className="h-4 w-4" />
                    ) : (
                      <Box className="h-4 w-4" />
                    )}
                  </span>
                </TooltipTrigger>
                <TooltipContent side="top">
                  {isAsync ? "Async" : "Batch"}
                </TooltipContent>
              </Tooltip>
            );
          },
        } as ColumnDef<Batch>,
      ]
    : []),
  {
    accessorKey: "status",
    header: "Status",
    cell: ({ row }) => {
      const batch = row.original as Batch;
      const status = row.getValue("status") as BatchStatus;
      const { completed, failed } = batch.request_counts;

      // Check if batch is queued (in_progress but no requests completed yet)
      const isQueued =
        status === "in_progress" && completed === 0 && failed === 0;

      if (isQueued) {
        return (
          <span className="inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium bg-gray-100 text-gray-700">
            queued
          </span>
        );
      }

      return (
        <span
          className={`inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium ${getStatusColor(status)}`}
        >
          {status.replace("_", " ")}
        </span>
      );
    },
  },
  {
    accessorKey: "request_counts",
    header: "Progress",
    cell: ({ row }) => {
      const batch = row.original as Batch;
      const { total, completed, failed } = batch.request_counts;
      // Only infer canceled count if the batch status is cancelled
      const canceled =
        batch.status === "cancelled"
          ? Math.max(0, total - completed - failed)
          : 0;
      const completedPercent = total > 0 ? (completed / total) * 100 : 0;
      const failedPercent = total > 0 ? (failed / total) * 100 : 0;
      const canceledPercent = total > 0 ? (canceled / total) * 100 : 0;

      // Determine if batch is queued (in_progress but no requests completed yet)
      const isQueued =
        batch.status === "in_progress" && completed === 0 && failed === 0;

      return (
        <div className="space-y-1 min-w-[200px]">
          <div className="flex justify-between text-xs text-gray-600">
            <span>
              {completed} / {total}
            </span>
            <span>{Math.floor(completedPercent)}%</span>
          </div>
          <div className="relative h-2 w-full bg-gray-200 rounded-full overflow-hidden">
            {isQueued ? (
              <div className="absolute left-0 top-0 h-full w-full bg-gray-300" />
            ) : (
              <>
                <div
                  className="absolute left-0 top-0 h-full bg-emerald-400 transition-all"
                  style={{ width: `${completedPercent}%` }}
                />
                <div
                  className="absolute top-0 h-full bg-rose-400 transition-all"
                  style={{
                    left: `${completedPercent}%`,
                    width: `${failedPercent}%`,
                  }}
                />
                {canceled > 0 && (
                  <div
                    className="absolute top-0 h-full bg-gray-400 transition-all"
                    style={{
                      left: `${completedPercent + failedPercent}%`,
                      width: `${canceledPercent}%`,
                    }}
                  />
                )}
              </>
            )}
          </div>
        </div>
      );
    },
  },
  {
    id: "duration",
    header: "Duration",
    cell: ({ row }) => {
      const batch = row.original as Batch;

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
          {formatLongDuration(duration)}
        </div>
      );
    },
  },
  {
    id: "cost",
    header: "Cost",
    cell: ({ row }) => {
      const batch = row.original as Batch;
      const cost = batch.analytics?.total_cost;
      if (!cost || parseFloat(cost) === 0) {
        return <span className="text-gray-400">-</span>;
      }
      const value = parseFloat(cost);
      const formatted =
        value < 0.0001
          ? `$${value.toFixed(6)}`
          : value < 0.01
            ? `$${value.toFixed(4)}`
            : `$${value.toFixed(2)}`;
      return (
        <span className="text-sm text-green-700 font-medium">{formatted}</span>
      );
    },
  },
  {
    id: "batch_id",
    header: "Batch ID",
    cell: ({ row }) => {
      const batch = row.original as Batch;
      return (
        <span className="font-mono text-xs text-doubleword-neutral-700 truncate max-w-[100px] block cursor-default" title={batch.id}>
          {batch.id.slice(0, 8)}...
        </span>
      );
    },
  },
  {
    id: "input_file_id",
    header: "Input File",
    cell: ({ row }) => {
      const batch = row.original as Batch;
      if (!batch.input_file_id) return <span className="text-gray-400">-</span>;
      return (
        <span className="font-mono text-xs text-doubleword-neutral-700 truncate max-w-[100px] block cursor-default" title={batch.input_file_id}>
          {batch.input_file_id.slice(0, 8)}...
        </span>
      );
    },
  },
  {
    id: "actions",
    header: "Actions",
    cell: ({ row }) => {
      const batch = row.original as Batch;
      const files = actions.getBatchFiles(batch);
      const outputFile = files.find((f: any) => f.purpose === "batch_output");
      const errorFile = files.find((f: any) => f.purpose === "batch_error");
      const canCancel = ["validating", "in_progress", "finalizing"].includes(
        batch.status,
      );

      // Get request counts - using completed count for output, failed count for errors
      const outputCount = batch.request_counts.completed;
      const errorCount = batch.request_counts.failed;

      return (
        <div className="flex items-center gap-2 -ml-2">
          {outputFile ? (
            <Tooltip delayDuration={500}>
              <TooltipTrigger asChild>
                <Button
                  variant="ghost"
                  size="sm"
                  className="h-7 w-7 p-0 text-gray-600 hover:bg-gray-100 hover:text-gray-900 relative group/output"
                  onClick={(e) => {
                    e.stopPropagation();
                    actions.onViewFile(outputFile);
                  }}
                >
                  <FileCheck className="h-5 w-5" />
                  <span className="absolute top-[5%] left-[55%] text-gray-600 group-hover/output:text-gray-900 text-[8px] font-bold leading-none border border-gray-400 group-hover/output:border-gray-900 rounded-full min-w-3 h-3 flex items-center justify-center bg-white px-0.5 transition-colors">
                    {formatNumber(outputCount)}
                  </span>
                </Button>
              </TooltipTrigger>
              <TooltipContent>
                View output file ({formatNumber(outputCount)} requests)
              </TooltipContent>
            </Tooltip>
          ) : (
            <div className="h-7 w-7" />
          )}
          {errorFile ? (
            <Tooltip delayDuration={500}>
              <TooltipTrigger asChild>
                <Button
                  variant="ghost"
                  size="sm"
                  className="h-7 w-7 p-0 text-gray-600 hover:bg-red-50 hover:text-red-600 relative group/error"
                  onClick={(e) => {
                    e.stopPropagation();
                    actions.onViewFile(errorFile);
                  }}
                >
                  <AlertCircle className="h-5 w-5" />
                  <span className="absolute top-[5%] left-[55%] text-gray-600 group-hover/error:text-red-600 text-[8px] font-bold leading-none border border-gray-400 group-hover/error:border-red-600 rounded-full min-w-3 h-3 flex items-center justify-center bg-white px-0.5 transition-colors">
                    {formatNumber(errorCount)}
                  </span>
                </Button>
              </TooltipTrigger>
              <TooltipContent>
                View error file ({formatNumber(errorCount)} requests)
              </TooltipContent>
            </Tooltip>
          ) : (
            <div className="h-7 w-7" />
          )}
          <Tooltip delayDuration={500}>
            <TooltipTrigger asChild>
              <Button
                variant="ghost"
                size="sm"
                className="h-7 w-7 p-0 text-gray-600 hover:bg-gray-100 hover:text-gray-900"
                onClick={(e) => {
                  e.stopPropagation();
                  actions.onRowClick?.(batch);
                }}
              >
                <Eye className="h-4 w-4" />
              </Button>
            </TooltipTrigger>
            <TooltipContent>View batch details</TooltipContent>
          </Tooltip>
          {canCancel && (
            <Tooltip delayDuration={500}>
              <TooltipTrigger asChild>
                <Button
                  variant="ghost"
                  size="sm"
                  className="h-7 w-7 p-0 text-red-600 hover:bg-red-50 hover:text-red-700"
                  onClick={(e) => {
                    e.stopPropagation();
                    actions.onCancel(batch);
                  }}
                >
                  <XCircle className="h-4 w-4" />
                </Button>
              </TooltipTrigger>
              <TooltipContent>Cancel batch</TooltipContent>
            </Tooltip>
          )}
          <Tooltip delayDuration={500}>
            <TooltipTrigger asChild>
              <Button
                variant="ghost"
                size="sm"
                className="h-7 w-7 p-0 text-red-600 hover:bg-red-50 hover:text-red-700"
                onClick={(e) => {
                  e.stopPropagation();
                  actions.onDelete(batch);
                }}
              >
                <Trash2 className="h-4 w-4" />
              </Button>
            </TooltipTrigger>
            <TooltipContent>Delete batch</TooltipContent>
          </Tooltip>
        </div>
      );
    },
  },
];
