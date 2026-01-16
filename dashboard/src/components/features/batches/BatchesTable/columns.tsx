"use client";

import { type ColumnDef } from "@tanstack/react-table";
import {
  ArrowUpDown,
  XCircle,
  Clock,
  CheckCircle2,
  AlertCircle,
  Loader2,
  FileCheck,
  FileText,
  DollarSign,
  Eye,
  Trash2,
} from "lucide-react";
import { Button } from "../../../ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "../../../ui/tooltip";
import {
  formatTimestamp,
  formatLongDuration,
  formatNumber,
} from "../../../../utils";
import type { Batch, BatchStatus, BatchAnalytics } from "../types";

interface ColumnActions {
  onCancel: (batch: Batch) => void;
  onDelete: (batch: Batch) => void;
  getBatchFiles: (batch: Batch) => any[];
  onViewFile: (file: any) => void;
  getInputFile: (batch: Batch) => any | undefined;
  onRowClick?: (batch: Batch) => void;
  batchAnalytics?: Map<string, BatchAnalytics>;
  /** Show the User column (only for PlatformManagers who see all batches) */
  showUserColumn?: boolean;
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
          <span className="text-sm text-gray-700 truncate max-w-[120px] block cursor-default">
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
  {
    id: "input_file",
    header: "Input File ID",
    cell: ({ row }) => {
      const batch = row.original as Batch;
      const inputFile = actions.getInputFile(batch);

      if (!inputFile || !inputFile.id) {
        return <span className="text-gray-400">-</span>;
      }

      // Truncate file ID to show first 8 characters
      const truncatedId = inputFile.id.slice(0, 8) + "...";

      return (
        <Tooltip delayDuration={300}>
          <TooltipTrigger asChild>
            <div
              className="flex items-center gap-2 cursor-pointer hover:text-blue-600 transition-colors"
              onClick={(e) => {
                e.stopPropagation();
                actions.onViewFile(inputFile);
              }}
            >
              <FileText className="w-4 h-4 text-gray-500" />
              <span className="font-mono text-sm">{truncatedId}</span>
            </div>
          </TooltipTrigger>
          <TooltipContent>{inputFile.id}</TooltipContent>
        </Tooltip>
      );
    },
  },
  {
    accessorKey: "completion_window",
    header: "SLA",
    cell: ({ row }) => {
      const completionWindow = row.getValue("completion_window") as string;
      return <span className="text-sm text-gray-700">{completionWindow}</span>;
    },
  },
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
          <div className="flex items-center gap-2">
            <Clock className="w-4 h-4 text-gray-500" />
            <span className="inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium bg-gray-100 text-gray-700">
              queued
            </span>
          </div>
        );
      }

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
      const batch = row.original as Batch;
      return <div className="font-mono text-xs text-gray-600">{batch.id}</div>;
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
      const analytics = actions.batchAnalytics?.get(batch.id);
      if (!analytics) {
        return (
          <div className="flex items-center gap-1 text-sm text-gray-400">
            <Loader2 className="w-3 h-3 animate-spin" />
            <span>...</span>
          </div>
        );
      }

      if (!analytics.total_cost || parseFloat(analytics.total_cost) === 0) {
        return <span className="text-gray-400 text-sm">-</span>;
      }

      const cost = parseFloat(analytics.total_cost);

      return (
        <div className="flex items-center gap-1 text-sm text-gray-700">
          <DollarSign className="w-3 h-3 text-green-600" />
          <span className="font-medium">{cost.toFixed(4)}</span>
        </div>
      );
    },
  },
  {
    id: "files",
    header: "Results",
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
