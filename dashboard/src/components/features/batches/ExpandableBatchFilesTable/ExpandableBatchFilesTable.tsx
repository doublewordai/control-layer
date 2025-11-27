import { useState } from "react";
import {
  ChevronRight,
  ChevronDown,
  Trash2,
  List,
  Download,
  Play,
} from "lucide-react";
import { Button } from "../../../ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "../../../ui/tooltip";
import { formatBytes, formatTimestamp } from "../../../../utils";
import type { FileObject, Batch } from "../types";
import { ExpandableBatchRow } from "./ExpandableBatchRow";

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
  columnVisibility: Record<ColumnId, boolean>;
}

export type ColumnId = "created" | "id" | "filename" | "size";

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
  columnVisibility,
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
    return batches
      .filter((batch) => batch.input_file_id === fileId)
      .sort((a, b) => b.created_at - a.created_at);
  };

  return (
    <div className="space-y-4">
      <div className="border rounded-lg overflow-hidden bg-white">
        <div className="overflow-x-auto">
          <table className="w-full">
            <thead className="bg-gray-50 border-b">
              <tr>
                <th className="w-10 px-2"></th>
                {columnVisibility.created && (
                  <th className="px-4 py-3 text-left text-sm font-medium text-gray-700">
                    Created
                  </th>
                )}
                {columnVisibility.id && (
                  <th className="px-4 py-3 text-left text-sm font-medium text-gray-700">
                    ID
                  </th>
                )}
                {columnVisibility.filename && (
                  <th className="px-4 py-3 text-left text-sm font-medium text-gray-700">
                    Filename
                  </th>
                )}
                {columnVisibility.size && (
                  <th className="px-4 py-3 text-left text-sm font-medium text-gray-700">
                    Size
                  </th>
                )}
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
                const batch = getBatchesForFile(file.id)[0];

                return (
                  <>
                    {/* File Row */}
                    <tr
                      key={file.id}
                      className={`border-b transition-colors ${hasBatches ? "cursor-pointer hover:bg-gray-50" : ""}`}
                      onClick={() => {
                        if (hasBatches) {
                          toggleRow(file.id);
                        }
                      }}
                    >
                      <td className="px-2 py-3">
                        {hasBatches ? (
                          <div className="text-gray-500 p-1">
                            {isExpanded ? (
                              <ChevronDown className="w-4 h-4" />
                            ) : (
                              <ChevronRight className="w-4 h-4" />
                            )}
                          </div>
                        ) : null}
                      </td>

                      {/* File Created At */}
                      {columnVisibility.created && (
                        <td className="px-4 py-3 text-sm text-gray-700">
                          {formatTimestamp(
                            new Date(file.created_at * 1000).toISOString(),
                          )}
                        </td>
                      )}

                      {/* File ID */}
                      {columnVisibility.id && (
                        <td className="px-4 py-3 text-sm font-mono text-gray-600">
                          {file.id}
                        </td>
                      )}

                      {/* File Name */}
                      {columnVisibility.filename && (
                        <td className="px-4 py-3 text-sm text-gray-900 font-medium">
                          {file.filename}
                        </td>
                      )}

                      {/* File Size */}
                      {columnVisibility.size && (
                        <td className="px-4 py-3 text-sm text-gray-700">
                          {formatBytes(file.bytes)}
                        </td>
                      )}

                      {/* File Actions */}
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
                    {isExpanded && (
                      <ExpandableBatchRow
                        batch={batch}
                        columnVisibility={columnVisibility}
                        getBatchFiles={getBatchFiles}
                        onCancelBatch={onCancelBatch}
                        onViewFileRequests={onViewFileRequests}
                      />
                    )}
                  </>
                );
              })}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  );
}
