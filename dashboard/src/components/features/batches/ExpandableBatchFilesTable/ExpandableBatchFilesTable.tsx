import { useState } from "react";
import {
  ChevronRight,
  ChevronDown,
  Trash2,
  List,
  Download,
  Play,
  Layers,
  XCircle,
  Clock,
  CheckCircle2,
  AlertCircle,
  Loader2,
  FileCheck,
} from "lucide-react";
import { Button } from "../../../ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "../../../ui/tooltip";
import {
  formatBytes,
  formatTimestamp,
  formatLongDuration,
  formatNumber,
} from "../../../../utils";
import type { FileObject, Batch, BatchStatus } from "../types";

interface ExpandableBatchFilesTableProps {
  files: FileObject[];
  batches: Batch[];
  onViewFileRequests: (file: FileObject) => void;
  onDeleteFile: (file: FileObject) => void;
  onDownloadFileCode: (file: FileObject) => void;
  onTriggerBatch: (file: FileObject) => void;
  onCancelBatch: (batch: Batch) => void;
  getBatchFiles: (batch: Batch) => any[];
  isFileInProgress: (file: FileObject) => boolean;
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

export function ExpandableBatchFilesTable({
  files,
  batches,
  onViewFileRequests,
  onDeleteFile,
  onDownloadFileCode,
  onTriggerBatch,
  onCancelBatch,
  getBatchFiles,
  isFileInProgress,
}: ExpandableBatchFilesTableProps) {
  const [expandedRows, setExpandedRows] = useState<Set<string>>(new Set());

  const toggleRow = (fileId: string) => {
    const newExpanded = new Set(expandedRows);
    if (newExpanded.has(fileId)) {
      newExpanded.delete(fileId);
    } else {
      newExpanded.add(fileId);
    }
    setExpandedRows(newExpanded);
  };

  // Get batches for a specific file
  const getBatchesForFile = (fileId: string) => {
    return batches.filter((batch) => batch.input_file_id === fileId);
  };

  return (
    <div className="border rounded-lg overflow-hidden bg-white">
      <div className="overflow-x-auto">
        <table className="w-full">
          <thead className="bg-gray-50 border-b">
            <tr>
              <th className="w-10 px-2"></th>
              <th className="px-4 py-3 text-left text-sm font-medium text-gray-700">
                Created
              </th>
              <th className="px-4 py-3 text-left text-sm font-medium text-gray-700">
                Filename
              </th>
              <th className="px-4 py-3 text-left text-sm font-medium text-gray-700">
                Size
              </th>
              <th className="px-4 py-3 text-left text-sm font-medium text-gray-700">
                File ID
              </th>
              <th className="px-4 py-3 text-right text-sm font-medium text-gray-700">
                Actions
              </th>
            </tr>
          </thead>
          <tbody>
            {files.map((file) => {
              const fileBatches = getBatchesForFile(file.id);
              const isExpanded = expandedRows.has(file.id);
              const hasBatches = fileBatches.length > 0;
              const isInProgress = isFileInProgress(file);

              return (
                <>
                  {/* File Row */}
                  <tr
                    key={file.id}
                    className="border-b hover:bg-gray-50 transition-colors"
                  >
                    <td className="px-2 py-3">
                      {hasBatches ? (
                        <button
                          className="text-gray-500 hover:text-gray-700 p-1 rounded hover:bg-gray-100 transition-colors"
                          onClick={(e) => {
                            e.stopPropagation();
                            toggleRow(file.id);
                          }}
                        >
                          {isExpanded ? (
                            <ChevronDown className="w-4 h-4" />
                          ) : (
                            <ChevronRight className="w-4 h-4" />
                          )}
                        </button>
                      ) : null}
                    </td>
                    <td className="px-4 py-3 text-sm text-gray-700">
                      {formatTimestamp(
                        new Date(file.created_at * 1000).toISOString(),
                      )}
                    </td>
                    <td className="px-4 py-3 text-sm text-gray-900 font-medium">
                      {file.filename}
                      {hasBatches && (
                        <span className="ml-2 text-xs text-gray-500">
                          ({fileBatches.length}{" "}
                          {fileBatches.length === 1 ? "batch" : "batches"})
                        </span>
                      )}
                    </td>
                    <td className="px-4 py-3 text-sm text-gray-700">
                      {formatBytes(file.bytes)}
                    </td>
                    <td className="px-4 py-3 text-sm font-mono text-gray-600">
                      {file.id}
                    </td>
                    <td className="px-4 py-3">
                      <div className="flex items-center gap-2 justify-end -mr-2">
                        <Tooltip delayDuration={500}>
                          <TooltipTrigger asChild>
                            <Button
                              variant="ghost"
                              size="sm"
                              className="h-7 w-7 p-0 text-gray-700 hover:bg-gray-100 hover:text-gray-900"
                              onClick={(e) => {
                                e.stopPropagation();
                                onViewFileRequests(file);
                              }}
                            >
                              <List className="h-4 w-4" />
                            </Button>
                          </TooltipTrigger>
                          <TooltipContent>View file contents</TooltipContent>
                        </Tooltip>

                        <Tooltip delayDuration={500}>
                          <TooltipTrigger asChild>
                            <Button
                              variant="ghost"
                              size="sm"
                              className="h-7 w-7 p-0 text-gray-700 hover:bg-gray-100 hover:text-gray-900"
                              onClick={(e) => {
                                e.stopPropagation();
                                onDownloadFileCode(file);
                              }}
                            >
                              <Download className="h-4 w-4" />
                            </Button>
                          </TooltipTrigger>
                          <TooltipContent>Download file</TooltipContent>
                        </Tooltip>

                        {file.purpose === "batch" && (
                          <Tooltip delayDuration={500}>
                            <TooltipTrigger asChild>
                              <Button
                                variant="ghost"
                                size="sm"
                                className="h-7 w-7 p-0 text-gray-700 hover:bg-gray-100 hover:text-gray-900"
                                onClick={(e) => {
                                  e.stopPropagation();
                                  onTriggerBatch(file);
                                }}
                              >
                                <Play className="h-4 w-4" />
                              </Button>
                            </TooltipTrigger>
                            <TooltipContent>
                              Create batch from file
                            </TooltipContent>
                          </Tooltip>
                        )}

                        {hasBatches && (
                          <Tooltip delayDuration={500}>
                            <TooltipTrigger asChild>
                              <Button
                                variant="ghost"
                                size="sm"
                                className="h-7 w-7 p-0 text-gray-700 hover:bg-gray-100 hover:text-gray-900"
                                onClick={(e) => {
                                  e.stopPropagation();
                                  toggleRow(file.id);
                                }}
                              >
                                <Layers className="h-4 w-4" />
                              </Button>
                            </TooltipTrigger>
                            <TooltipContent>
                              {isExpanded ? "Hide" : "Show"} batches
                            </TooltipContent>
                          </Tooltip>
                        )}

                        <Tooltip delayDuration={500}>
                          <TooltipTrigger asChild>
                            <Button
                              variant="ghost"
                              size="sm"
                              className="h-8 w-8 p-0 text-gray-700 hover:bg-red-50 hover:text-red-600"
                              onClick={(e) => {
                                e.stopPropagation();
                                onDeleteFile(file);
                              }}
                              disabled={isInProgress}
                            >
                              <Trash2 className="h-4 w-4" />
                            </Button>
                          </TooltipTrigger>
                          <TooltipContent>
                            {isInProgress
                              ? "Cannot delete - batch in progress"
                              : "Delete file"}
                          </TooltipContent>
                        </Tooltip>
                      </div>
                    </td>
                  </tr>

                  {/* Expanded Batch Rows */}
                  {isExpanded &&
                    fileBatches.map((batch, index) => {
                      const batchFiles = getBatchFiles(batch);
                      const outputFile = batchFiles.find(
                        (f: any) => f.purpose === "batch_output",
                      );
                      const errorFile = batchFiles.find(
                        (f: any) => f.purpose === "batch_error",
                      );
                      const canCancel = [
                        "validating",
                        "in_progress",
                        "finalizing",
                      ].includes(batch.status);

                      const outputCount = batch.request_counts.completed;
                      const errorCount = batch.request_counts.failed;

                      const { total, completed, failed } = batch.request_counts;
                      const canceled =
                        batch.status === "cancelled"
                          ? Math.max(0, total - completed - failed)
                          : 0;
                      const completedPercent =
                        total > 0 ? (completed / total) * 100 : 0;
                      const failedPercent =
                        total > 0 ? (failed / total) * 100 : 0;
                      const canceledPercent =
                        total > 0 ? (canceled / total) * 100 : 0;

                      const startTime = batch.in_progress_at
                        ? batch.in_progress_at * 1000
                        : null;
                      const endTime = batch.completed_at
                        ? batch.completed_at * 1000
                        : batch.failed_at
                          ? batch.failed_at * 1000
                          : batch.cancelled_at
                            ? batch.cancelled_at * 1000
                            : Date.now();
                      const duration = startTime ? endTime - startTime : null;

                      return (
                        <tr
                          key={batch.id}
                          className={`bg-blue-50/30 border-b ${index === fileBatches.length - 1 ? "border-b-2" : ""}`}
                        >
                          {/*<td className="px-2 py-3"></td>*/}
                          <td colSpan={5} className="px-4 py-3">
                            <div className="pl-6">
                              <div className="grid grid-cols-12 gap-4 items-center text-sm">
                                {/* Status */}
                                <div className="col-span-2">
                                  <div className="flex items-center gap-2">
                                    {getStatusIcon(batch.status)}
                                    <span
                                      className={`inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium ${getStatusColor(batch.status)}`}
                                    >
                                      {batch.status.replace("_", " ")}
                                    </span>
                                  </div>
                                </div>

                                {/* Progress */}
                                <div className="col-span-3">
                                  <div className="space-y-1">
                                    <div className="flex justify-between text-xs text-gray-600">
                                      <span>
                                        {completed + failed + canceled} /{" "}
                                        {total}
                                      </span>
                                      <span>
                                        {Math.round(
                                          completedPercent +
                                            failedPercent +
                                            canceledPercent,
                                        )}
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
                                </div>

                                {/* Duration */}
                                <div className="col-span-2">
                                  {duration ? (
                                    <div className="flex items-center gap-1 text-sm text-gray-700">
                                      <Clock className="w-3 h-3" />
                                      {formatLongDuration(duration)}
                                    </div>
                                  ) : (
                                    <span className="text-gray-400">-</span>
                                  )}
                                </div>

                                {/* Actions */}
                                <div className="col-span-3 flex items-center gap-2 justify-end">
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
                                        View output file (
                                        {formatNumber(outputCount)} requests)
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
                                        View error file (
                                        {formatNumber(errorCount)} requests)
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
                                      <TooltipContent>
                                        Cancel batch
                                      </TooltipContent>
                                    </Tooltip>
                                  )}
                                </div>
                              </div>
                            </div>
                          </td>
                        </tr>
                      );
                    })}
                </>
              );
            })}
          </tbody>
        </table>
      </div>
    </div>
  );
}
