import { useState, useEffect } from "react";
import * as React from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { useQueryClient } from "@tanstack/react-query";
import { Upload, FileInput, FileCheck, AlertCircle } from "lucide-react";
import { Button } from "../../../ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../../ui/select";
import { CursorPagination } from "../../../ui/cursor-pagination";
import type { ColumnId } from "../ExpandableBatchFilesTable/ExpandableBatchFilesTable";
import { ExpandableBatchFilesTable } from "../ExpandableBatchFilesTable/ExpandableBatchFilesTable";
import { useFiles, useBatches } from "../../../../api/control-layer/hooks";
import { dwctlApi } from "../../../../api/control-layer/client";
import type { FileObject, Batch } from "../types";
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuTrigger,
} from "../../../ui/dropdown-menu";

/**
 * Props for the Batches component.
 * All modal operations are handled by parent container to prevent
 * modal state from being lost during auto-refresh re-renders.
 */
interface BatchesProps {
  onOpenUploadModal: (file?: File) => void;
  onOpenCreateBatchModal: (file?: FileObject) => void;
  onOpenDownloadModal: (resource: {
    type: "file" | "batch-results";
    id: string;
    filename?: string;
    isPartial?: boolean;
  }) => void;
  onOpenDeleteDialog: (file: FileObject) => void;
  onOpenCancelDialog: (batch: Batch) => void;
}

export function Batches({
  onOpenUploadModal,
  onOpenCreateBatchModal,
  onOpenDownloadModal,
  onOpenDeleteDialog,
  onOpenCancelDialog,
}: BatchesProps) {
  const navigate = useNavigate();
  const [searchParams, setSearchParams] = useSearchParams();
  const queryClient = useQueryClient();

  // Drag and drop state (kept locally as it's UI-only)
  const [dragActive, setDragActive] = useState(false);

  // Read state from URL
  const fileTypeFilter =
    (searchParams.get("fileType") as "input" | "output" | "error") || "input";

  // Pagination state from URL
  const filesPage = parseInt(searchParams.get("filesPage") || "0", 10);
  const filesPageSize = parseInt(searchParams.get("filesPageSize") || "10", 10);
  const filesAfterCursor = searchParams.get("filesAfter") || undefined;

  // Cursor history for backwards pagination
  const filesCursorHistory = React.useRef<(string | undefined)[]>([]);

  // API queries
  // Paginated files query for display in Files tab
  // Map fileType to purpose filter
  const filePurpose =
    fileTypeFilter === "input"
      ? "batch"
      : fileTypeFilter === "output"
        ? "batch_output"
        : "batch_error"; // error

  const { data: filesResponse, isLoading: filesLoading } = useFiles({
    purpose: filePurpose,
    limit: filesPageSize + 1, // Fetch one extra to detect if there are more
    after: filesAfterCursor,
    // Always fetch to populate tab counts, but refetch interval is lower when not active
  });

  // Fetch all batches (they'll be displayed nested under files)
  const { data: batchesResponse, isLoading: batchesLoading } = useBatches({
    // No pagination for batches - we show them nested under their input files
  });

  // Get all batches
  const batches = batchesResponse?.data || [];

  // Process files response - remove extra item used for hasMore detection
  const filesData = filesResponse?.data || [];
  const filesHasMore = filesData.length > filesPageSize;
  const filesForDisplay = filesHasMore
    ? filesData.slice(0, filesPageSize)
    : filesData;

  // Display files as returned by API (server-side filtered by purpose)
  const files = filesForDisplay;

  const isFilesLoading = filesLoading;

  // column selectors
  const [columnVisibility, setColumnVisibility] = useState<
    Record<ColumnId, boolean>
  >({
    created: true,
    id: false, // ID column hidden by default
    filename: true,
    size: true,
  });

  const toggleColumn = (columnId: ColumnId) => {
    setColumnVisibility((prev) => ({
      ...prev,
      [columnId]: !prev[columnId],
    }));
  };

  // Prefetch next page for files - only if user has already started paginating
  useEffect(() => {
    if (filesHasMore && files.length > 0 && filesPage > 0) {
      const lastFile = files[files.length - 1];
      const nextCursor = lastFile.id;

      const prefetchOptions = {
        purpose: filePurpose,
        limit: filesPageSize + 1,
        after: nextCursor,
      };

      queryClient.prefetchQuery({
        queryKey: ["files", "list", prefetchOptions],
        queryFn: () => dwctlApi.files.list(prefetchOptions),
      });
    }
  }, [files, filesHasMore, filesPage, filesPageSize, filePurpose, queryClient]);

  // Drag and drop handlers
  const handleDrag = (e: React.DragEvent) => {
    e.preventDefault();
    e.stopPropagation();
    if (e.type === "dragenter" || e.type === "dragover") {
      setDragActive(true);
    } else if (e.type === "dragleave") {
      setDragActive(false);
    }
  };

  const handleDrop = (e: React.DragEvent) => {
    e.preventDefault();
    e.stopPropagation();
    setDragActive(false);

    if (e.dataTransfer.files && e.dataTransfer.files[0]) {
      const file = e.dataTransfer.files[0];
      if (file.name.endsWith(".jsonl")) {
        onOpenUploadModal(file);
      }
    }
  };

  // Pagination handlers
  const handleFilesNextPage = () => {
    const lastFile = files[files.length - 1];
    if (lastFile && filesHasMore) {
      // Save current cursor to history before moving forward
      filesCursorHistory.current[filesPage] = filesAfterCursor;
      const params = new URLSearchParams(searchParams);
      params.set("filesPage", (filesPage + 1).toString());
      params.set("filesPageSize", filesPageSize.toString());
      params.set("filesAfter", lastFile.id);
      setSearchParams(params, { replace: true });
    }
  };

  const handleFilesPrevPage = () => {
    if (filesPage > 0) {
      // Use cursor history to go back one page
      const previousCursor = filesCursorHistory.current[filesPage - 1];
      const params = new URLSearchParams(searchParams);
      params.set("filesPage", (filesPage - 1).toString());
      params.set("filesPageSize", filesPageSize.toString());
      if (previousCursor) {
        params.set("filesAfter", previousCursor);
      } else {
        params.delete("filesAfter");
      }
      setSearchParams(params, { replace: true });
    }
  };

  const handleFilesPageSizeChange = (newSize: number) => {
    filesCursorHistory.current = []; // Clear history when changing page size
    const params = new URLSearchParams(searchParams);
    params.set("filesPage", "1");
    params.set("filesPageSize", newSize.toString());
    params.delete("filesAfter");
    setSearchParams(params, { replace: true });
  };

  // Loading state
  if (isFilesLoading || batchesLoading) {
    return (
      <div className="py-4 px-6">
        <div className="mb-4">
          <h1 className="text-3xl font-bold text-doubleword-neutral-900">
            Batch Processing
          </h1>
          <p className="text-doubleword-neutral-600 mt-2">Loading...</p>
        </div>
        <div className="flex items-center justify-center h-64">
          <div className="text-center">
            <div
              className="animate-spin rounded-full h-12 w-12 border-b-2 border-doubleword-accent-blue mx-auto mb-4"
              aria-label="Loading"
            ></div>
            <p className="text-doubleword-neutral-600">Loading...</p>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div
      className="py-4 px-6"
      onDragEnter={handleDrag}
      onDragLeave={handleDrag}
      onDragOver={handleDrag}
      onDrop={handleDrop}
    >
      {/* Header */}
      <div className="mb-6 flex flex-col lg:flex-row lg:items-center lg:justify-between gap-4">
        {/* Left: Title */}
        <div className="shrink-0">
          <h1 className="text-3xl font-bold text-doubleword-neutral-900">
            Batch Processing
          </h1>
          <p className="text-doubleword-neutral-600 mt-1">
            Upload files and create batches to process requests at scale
          </p>
        </div>

        {/* Right: Action Button */}
        <div className="flex items-center gap-3 lg:shrink-0">
          <Button
            onClick={() => onOpenUploadModal()}
            variant="outline"
            className={`transition-all duration-200 ${
              dragActive ? "border-blue-500 bg-blue-50 text-blue-700" : ""
            }`}
          >
            <Upload className="w-4 h-4 mr-2" />
            Upload File
          </Button>
        </div>
      </div>

      {/* Content */}
      {files.length === 0 ? (
        <div className="text-center py-12">
          <div className="p-4 bg-doubleword-neutral-100 rounded-full w-16 h-16 mx-auto mb-4 flex items-center justify-center">
            <FileInput className="w-8 h-8 text-doubleword-neutral-600" />
          </div>
          <h3 className="text-lg font-medium text-doubleword-neutral-900 mb-2">
            No files uploaded
          </h3>
          <p className="text-doubleword-neutral-600 mb-4">
            Upload a .jsonl file to get started with batch processing
          </p>
          <Button onClick={() => onOpenUploadModal()}>
            <Upload className="w-4 h-4 mr-2" />
            Upload First File
          </Button>
        </div>
      ) : (
        <div className="space-y-4">
          {/* File Type Filter and Page Size Selector */}
          <div className="flex items-center justify-between">
            <div className="inline-flex h-9 items-center justify-center rounded-md bg-muted p-1 text-muted-foreground">
              {(["input", "output", "error"] as const).map((type) => {
                const Icon =
                  type === "input"
                    ? FileInput
                    : type === "output"
                      ? FileCheck
                      : AlertCircle;
                const label = type.charAt(0).toUpperCase() + type.slice(1);

                return (
                  <button
                    key={type}
                    type="button"
                    title={`${label} files`}
                    onClick={(e) => {
                      e.preventDefault();
                      e.stopPropagation();
                      const params = new URLSearchParams(searchParams);
                      if (type !== "input") {
                        params.set("fileType", type);
                      } else {
                        params.delete("fileType");
                      }
                      // Reset pagination
                      params.set("filesPage", "1");
                      params.set("filesPageSize", filesPageSize.toString());
                      params.delete("filesAfter");
                      setSearchParams(params, { replace: false });
                      // Reset cursor history
                      filesCursorHistory.current = [];
                    }}
                    className={`inline-flex items-center justify-center whitespace-nowrap rounded-sm px-3 py-1 text-sm font-medium ring-offset-background transition-all focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 ${
                      fileTypeFilter === type
                        ? "bg-background text-foreground shadow-sm"
                        : "hover:bg-background/50"
                    }`}
                  >
                    <Icon className="w-4 h-4" />
                  </button>
                );
              })}
            </div>

            <div className="flex items-center gap-2">
              <span className="text-sm text-gray-600">Rows:</span>
              <Select
                value={filesPageSize.toString()}
                onValueChange={(value) =>
                  handleFilesPageSizeChange(Number(value))
                }
              >
                <SelectTrigger className="w-20 h-9">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="10">10</SelectItem>
                  <SelectItem value="25">25</SelectItem>
                  <SelectItem value="50">50</SelectItem>
                  <SelectItem value="100">100</SelectItem>
                  <SelectItem value="200">200</SelectItem>
                  <SelectItem value="500">500</SelectItem>
                </SelectContent>
              </Select>

              <div className="flex justify-end">
                <DropdownMenu>
                  <DropdownMenuTrigger asChild>
                    <Button variant="outline" size="sm">
                      Columns
                    </Button>
                  </DropdownMenuTrigger>
                  <DropdownMenuContent align="end" className="w-[150px]">
                    <DropdownMenuCheckboxItem
                      key={"created"}
                      className="capitalize"
                      checked={columnVisibility.created}
                      onCheckedChange={() => toggleColumn("created")}
                    >
                      Created
                    </DropdownMenuCheckboxItem>
                    <DropdownMenuCheckboxItem
                      checked={columnVisibility.id}
                      onCheckedChange={() => toggleColumn("id")}
                    >
                      ID
                    </DropdownMenuCheckboxItem>
                    <DropdownMenuCheckboxItem
                      checked={columnVisibility.filename}
                      onCheckedChange={() => toggleColumn("filename")}
                    >
                      Filename
                    </DropdownMenuCheckboxItem>
                    <DropdownMenuCheckboxItem
                      checked={columnVisibility.size}
                      onCheckedChange={() => toggleColumn("size")}
                    >
                      Size
                    </DropdownMenuCheckboxItem>
                  </DropdownMenuContent>
                </DropdownMenu>
              </div>
            </div>
          </div>

          {/* Expandable Table */}
          <ExpandableBatchFilesTable
            files={files}
            batches={batches}
            onViewFileRequests={(file) =>
              navigate(`/batches/files/${file.id}/content`)
            }
            onDeleteFile={onOpenDeleteDialog}
            onDownloadFileCode={(file) => {
              const isPartial =
                (file.purpose === "batch_output" ||
                  file.purpose === "batch_error") &&
                batches.some(
                  (b) =>
                    (b.output_file_id === file.id ||
                      b.error_file_id === file.id) &&
                    ["validating", "in_progress", "finalizing"].includes(
                      b.status,
                    ),
                );

              onOpenDownloadModal({
                type: "file",
                id: file.id,
                filename: file.filename,
                isPartial,
              });
            }}
            onTriggerBatch={onOpenCreateBatchModal}
            onCancelBatch={onOpenCancelDialog}
            getBatchFiles={(batch) => {
              const files: Array<{ id: string; purpose: string }> = [];
              if (batch.output_file_id) {
                files.push({
                  id: batch.output_file_id,
                  purpose: "batch_output",
                });
              }
              if (batch.error_file_id) {
                files.push({ id: batch.error_file_id, purpose: "batch_error" });
              }
              return files;
            }}
            isFileInProgress={(file) => {
              if (
                file.purpose !== "batch_output" &&
                file.purpose !== "batch_error"
              ) {
                return false;
              }

              const batch = batches.find(
                (b) =>
                  b.output_file_id === file.id || b.error_file_id === file.id,
              );

              if (!batch) return false;

              const activeStatuses: string[] = [
                "validating",
                "in_progress",
                "finalizing",
              ];
              return activeStatuses.includes(batch.status);
            }}
            columnVisibility={columnVisibility}
          />

          {/* Pagination */}
          <CursorPagination
            currentPage={filesPage}
            itemsPerPage={filesPageSize}
            onNextPage={handleFilesNextPage}
            onPrevPage={handleFilesPrevPage}
            onFirstPage={() => {
              filesCursorHistory.current = [];
              const params = new URLSearchParams(searchParams);
              params.set("filesPage", "1");
              params.set("filesPageSize", filesPageSize.toString());
              params.delete("filesAfter");
              setSearchParams(params, { replace: true });
            }}
            hasNextPage={filesHasMore}
            hasPrevPage={filesPage > 1}
            currentPageItemCount={files.length}
            itemName="files"
          />
        </div>
      )}
    </div>
  );
}
