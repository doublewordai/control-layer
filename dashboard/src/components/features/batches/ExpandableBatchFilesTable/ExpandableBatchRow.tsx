import {
  AlertCircle,
  CheckCircle2,
  Clock,
  FileCheck,
  Loader2,
  XCircle,
} from "lucide-react";
import {
  formatLongDuration,
  formatNumber,
  formatTimestamp,
} from "../../../../utils";
import type { Batch, BatchStatus, FileObject } from "../types";
import type { ColumnId } from "./ExpandableBatchFilesTable";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { Button } from "@/components/ui/button";

export const ExpandableBatchRow = ({
  batch,
  getBatchFiles,
  onCancelBatch,
  columnVisibility,
  onViewFileRequests,
}: {
  batch: Batch;
  getBatchFiles: (batch: Batch) => any[];
  onCancelBatch: (batch: Batch) => void;
  columnVisibility: Record<ColumnId, boolean>;
  onViewFileRequests: (file: FileObject) => void;
}) => {
  const batchFiles = getBatchFiles(batch);
  const outputFile = batchFiles.find((f: any) => f.purpose === "batch_output");
  const errorFile = batchFiles.find((f: any) => f.purpose === "batch_error");
  const canCancel = ["validating", "in_progress", "finalizing"].includes(
    batch.status,
  );

  const outputCount = batch.request_counts.completed;
  const errorCount = batch.request_counts.failed;

  const { total, completed, failed } = batch.request_counts;
  const canceled =
    batch.status === "cancelled" ? Math.max(0, total - completed - failed) : 0;
  const completedPercent = total > 0 ? (completed / total) * 100 : 0;
  const failedPercent = total > 0 ? (failed / total) * 100 : 0;
  const canceledPercent = total > 0 ? (canceled / total) * 100 : 0;

  const startTime = batch.in_progress_at ? batch.in_progress_at * 1000 : null;
  const endTime = batch.completed_at
    ? batch.completed_at * 1000
    : batch.failed_at
      ? batch.failed_at * 1000
      : batch.cancelled_at
        ? batch.cancelled_at * 1000
        : Date.now();
  const duration = startTime ? endTime - startTime : null;

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

  return (
    <tr key={batch.id} className={`bg-blue-50/30 border-b`}>
      {/* Empty cell for chevron column */}
      <td className="px-2 py-3"></td>

      {/* Status column (aligned with Created) */}
      {columnVisibility.created && (
        // <td className="px-4 py-3">
        <td className="px-4 py-3 text-sm text-gray-700">
          {formatTimestamp(new Date(batch.created_at * 1000).toISOString())}
        </td>
        // </td>
      )}

      {/* Batch ID (aligned with ID column) */}
      {columnVisibility.id && (
        <td className="px-4 py-3 text-sm font-mono text-gray-600">
          {batch.id}
        </td>
      )}

      {/* Progress bar (aligned with Filename) */}
      {columnVisibility.filename && (
        <td className="px-4 py-3">
          <div className="space-y-1">
            <div className="flex justify-between text-xs text-gray-600">
              <span>
                {completed + failed + canceled} / {total}
              </span>
              <span>
                {Math.round(completedPercent + failedPercent + canceledPercent)}
                %
              </span>
            </div>
            <div className="relative h-2 w-full bg-gray-200 rounded-full overflow-hidden">
              <div
                className="absolute left-0 top-0 h-full bg-emerald-400 transition-all"
                style={{
                  width: `${completedPercent}%`,
                }}
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
            </div>
          </div>
        </td>
      )}

      {/* View results (aligned with Size) */}
      {columnVisibility.size && (
        <td className="px-4 py-3 text-sm">
          <div className="flex items-center gap-2 justify-start">
            {duration ? (
              <div className="flex items-center gap-1 text-sm text-gray-700">
                <Clock className="w-3 h-3" />
                {formatLongDuration(duration)}
              </div>
            ) : (
              <span className="text-sm text-gray-400">-</span>
            )}
          </div>
        </td>
      )}

      {/* Actions */}
      <td className="px-4 py-3">
        <div className="flex items-center gap-2 justify-end -mr-2">
          <div className="flex items-center gap-2">
            {getStatusIcon(batch.status)}
            <span
              className={`inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium ${getStatusColor(batch.status)}`}
            >
              {batch.status.replace("_", " ")}
            </span>
          </div>
          {outputFile && (
            <Tooltip delayDuration={500}>
              <TooltipTrigger asChild>
                <Button
                  variant="ghost"
                  size="sm"
                  className="h-7 w-7 p-0 text-gray-600 hover:bg-gray-100 hover:text-gray-900 relative group/output"
                  onClick={(e) => {
                    e.stopPropagation();
                    onViewFileRequests(outputFile);
                  }}
                >
                  <FileCheck className="h-5 w-5" />
                  <span className="absolute top-[5%] left-[55%] text-gray-600 group-hover/output:text-gray-900 text-[8px] font-bold leading-none border border-gray-400 group-hover/output:border-gray-900 rounded-full min-w-[12px] h-3 flex items-center justify-center bg-white px-0.5 transition-colors">
                    {formatNumber(outputCount)}
                  </span>
                </Button>
              </TooltipTrigger>
              <TooltipContent>
                View output file ({formatNumber(outputCount)} requests)
              </TooltipContent>
            </Tooltip>
          )}
          {errorFile && (
            <Tooltip delayDuration={500}>
              <TooltipTrigger asChild>
                <Button
                  variant="ghost"
                  size="sm"
                  className="h-7 w-7 p-0 text-gray-600 hover:bg-red-50 hover:text-red-600 relative group/error"
                  onClick={(e) => {
                    e.stopPropagation();
                    onViewFileRequests(errorFile);
                  }}
                >
                  <AlertCircle className="h-5 w-5" />
                  <span className="absolute top-[5%] left-[55%] text-gray-600 group-hover/error:text-red-600 text-[8px] font-bold leading-none border border-gray-400 group-hover/error:border-red-600 rounded-full min-w-[12px] h-3 flex items-center justify-center bg-white px-0.5 transition-colors">
                    {formatNumber(errorCount)}
                  </span>
                </Button>
              </TooltipTrigger>
              <TooltipContent>
                View error file ({formatNumber(errorCount)} requests)
              </TooltipContent>
            </Tooltip>
          )}
          {canCancel && (
            <Tooltip delayDuration={500}>
              <TooltipTrigger asChild>
                <Button
                  variant="ghost"
                  size="sm"
                  className="h-7 w-7 p-0 text-red-600 hover:bg-red-50 hover:text-red-700"
                  onClick={(e) => {
                    e.stopPropagation();
                    onCancelBatch(batch);
                  }}
                >
                  <XCircle className="h-4 w-4" />
                </Button>
              </TooltipTrigger>
              <TooltipContent>Cancel batch</TooltipContent>
            </Tooltip>
          )}
        </div>
      </td>
    </tr>
  );
};
