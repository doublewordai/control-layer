import { useState, useEffect } from "react";
import * as React from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { useQueryClient, useQueries } from "@tanstack/react-query";
import {
  Upload,
  Play,
  Box,
  FileInput,
  FileCheck,
  AlertCircle,
} from "lucide-react";
import { Button } from "../../../ui/button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "../../../ui/tabs";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../../ui/select";
import { DataTable } from "../../../ui/data-table";
import { createFileColumns } from "../FilesTable/columns";
import { createBatchColumns } from "../BatchesTable/columns";
import { useFiles, useBatches } from "../../../../api/control-layer/hooks";
import { dwctlApi } from "../../../../api/control-layer/client";
import type { FileObject, Batch } from "../types";
import type {
  BatchAnalytics,
  FileCostEstimate,
} from "../../../../api/control-layer/types";
import { useServerCursorPagination } from "../../../../hooks/useServerCursorPagination";
import { useDebounce } from "../../../../hooks/useDebounce";

/**
 * Props for the Batches component.
 * All modal operations are handled by parent container to prevent
 * modal state from being lost during auto-refresh re-renders.
 */
interface BatchesProps {
  onOpenUploadModal: (file?: File) => void;
  onOpenCreateBatchModal: (file?: File | FileObject) => void;
  onOpenDownloadModal: (resource: {
    type: "file" | "batch-results";
    id: string;
    filename?: string;
    isPartial?: boolean;
  }) => void;
  onOpenDeleteDialog: (file: FileObject) => void;
  onOpenDeleteBatchDialog: (batch: Batch) => void;
  onOpenCancelDialog: (batch: Batch) => void;
  onBatchCreatedCallback?: (callback: () => void) => void;
}

export function Batches({
  onOpenUploadModal,
  onOpenCreateBatchModal,
  onOpenDownloadModal,
  onOpenDeleteDialog,
  onOpenDeleteBatchDialog,
  onOpenCancelDialog,
  onBatchCreatedCallback,
}: BatchesProps) {
  const navigate = useNavigate();
  const [searchParams, setSearchParams] = useSearchParams();
  const queryClient = useQueryClient();

  // Drag and drop state (kept locally as it's UI-only)
  const [dragActive, setDragActive] = useState(false);

  // Search state for files and batches (server-side)
  const [fileSearchQuery, setFileSearchQuery] = useState("");
  const [batchSearchQuery, setBatchSearchQuery] = useState("");
  const debouncedFileSearch = useDebounce(fileSearchQuery, 300);
  const debouncedBatchSearch = useDebounce(batchSearchQuery, 300);

  // Sync URL with state changes
  const updateURL = (
    tab: "files" | "batches",
    fileFilter: string | null,
    fileType?: "input" | "output" | "error",
  ) => {
    const params = new URLSearchParams(searchParams);
    params.set("tab", tab);
    if (fileFilter) {
      params.set("fileFilter", fileFilter);
    } else {
      params.delete("fileFilter");
    }
    if (fileType && fileType !== "input") {
      params.set("fileType", fileType);
    } else {
      params.delete("fileType");
    }
    setSearchParams(params, { replace: false });
  };

  // Register callback for when batch is successfully created
  const handleBatchCreated = () => {
    updateURL("batches", null);
  };

  useEffect(() => {
    if (onBatchCreatedCallback) {
      onBatchCreatedCallback(handleBatchCreated);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [onBatchCreatedCallback]);

  // Read state from URL - default to batches tab
  const activeTab =
    (searchParams.get("tab") as "files" | "batches") || "batches";
  const batchFileFilter = searchParams.get("fileFilter");
  const fileTypeFilter =
    (searchParams.get("fileType") as "input" | "output" | "error") || "input";

  // Pagination hooks with prefixed URL params for multi-table support
  const filesPagination = useServerCursorPagination({
    paramPrefix: "files",
  });

  const batchesPagination = useServerCursorPagination({
    paramPrefix: "batches",
  });

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
    search: debouncedFileSearch.trim() || undefined,
    ...filesPagination.queryParams,
  });

  // Paginated batches query
  const { data: batchesResponse, isLoading: batchesLoading } = useBatches({
    search: debouncedBatchSearch.trim() || undefined,
    ...batchesPagination.queryParams,
    // Always fetch to populate tab counts, but refetch interval is lower when not active
  });

  // Process batches response - remove extra item used for hasMore detection
  const batchesData = batchesResponse?.data || [];
  const batchesHasMore = batchesData.length > batchesPagination.pageSize;
  const batches = batchesHasMore
    ? batchesData.slice(0, batchesPagination.pageSize)
    : batchesData;

  // Process files response - remove extra item used for hasMore detection
  const filesData = filesResponse?.data || [];
  const filesHasMore = filesData.length > filesPagination.pageSize;
  const filesForDisplay = filesHasMore
    ? filesData.slice(0, filesPagination.pageSize)
    : filesData;

  // Display files as returned by API (server-side filtered by purpose)
  const files = filesForDisplay;

  // Filter batches by input file if filter is set
  const filteredBatches = React.useMemo(() => {
    if (!batchFileFilter) return batches;
    return batches.filter((b) => b.input_file_id === batchFileFilter);
  }, [batches, batchFileFilter]);

  // Fetch analytics for all batches in parallel
  const analyticsQueries = useQueries({
    queries: batches.map((batch) => ({
      queryKey: ["batches", "analytics", batch.id],
      queryFn: () => dwctlApi.batches.getAnalytics(batch.id),
      staleTime: 5000, // 5 seconds
      refetchInterval: 5000, // Refetch every 5 seconds for in-progress batches
    })),
  });

  // Create a map of batch ID to analytics for easy lookup
  const batchAnalyticsMap = React.useMemo(() => {
    const map = new Map<string, BatchAnalytics>();
    batches.forEach((batch, index) => {
      const analytics = analyticsQueries[index]?.data;
      if (analytics) {
        map.set(batch.id, analytics);
      }
    });
    return map;
  }, [batches, analyticsQueries]);

  // Fetch file cost estimates for batch input files
  const fileEstimateQueries = useQueries({
    queries: files
      .filter((file) => file.purpose === "batch")
      .map((file) => ({
        queryKey: ["files", "cost-estimate", file.id],
        queryFn: () => dwctlApi.files.getCostEstimate(file.id),
        staleTime: 60000, // 1 minute - cost estimates don't change frequently
      })),
  });

  // Create a map of file ID to cost estimate for easy lookup
  const fileEstimatesMap = React.useMemo(() => {
    const map = new Map<string, FileCostEstimate>();
    const batchFiles = files.filter((file) => file.purpose === "batch");
    batchFiles.forEach((file, index) => {
      const estimate = fileEstimateQueries[index]?.data;
      if (estimate) {
        map.set(file.id, estimate);
      }
    });
    return map;
  }, [files, fileEstimateQueries]);

  // Prefetch next page for files - only if user has already started paginating
  useEffect(() => {
    if (filesHasMore && files.length > 0 && filesPagination.page > 0) {
      const lastFile = files[files.length - 1];
      const nextCursor = lastFile.id;

      const prefetchOptions = {
        purpose: filePurpose,
        limit: filesPagination.pageSize + 1,
        after: nextCursor,
      };

      queryClient.prefetchQuery({
        queryKey: ["files", "list", prefetchOptions],
        queryFn: () => dwctlApi.files.list(prefetchOptions),
      });
    }
  }, [
    files,
    filesHasMore,
    filesPagination.page,
    filesPagination.pageSize,
    filePurpose,
    queryClient,
  ]);

  // Prefetch next page for batches - only if user has already started paginating
  useEffect(() => {
    if (batchesHasMore && batches.length > 0 && batchesPagination.page > 0) {
      const lastBatch = batches[batches.length - 1];
      const nextCursor = lastBatch.id;

      queryClient.prefetchQuery({
        queryKey: [
          "batches",
          "list",
          { limit: batchesPagination.pageSize + 1, after: nextCursor },
        ],
        queryFn: () =>
          dwctlApi.batches.list({
            limit: batchesPagination.pageSize + 1,
            after: nextCursor,
          }),
      });
    }
  }, [
    batches,
    batchesHasMore,
    batchesPagination.page,
    batchesPagination.pageSize,
    queryClient,
  ]);

  // Get output/error file IDs for a batch
  const getBatchFiles = (batch: Batch) => {
    const files: Array<{ id: string; purpose: string }> = [];
    if (batch.output_file_id) {
      files.push({ id: batch.output_file_id, purpose: "batch_output" });
    }
    if (batch.error_file_id) {
      files.push({ id: batch.error_file_id, purpose: "batch_error" });
    }
    return files;
  };

  // File actions
  const handleViewFileRequests = (file: FileObject) => {
    if ((file as any)._isEmpty) return;
    // Preserve current URL params when navigating to file content
    const currentParams = searchParams.toString();
    const fromUrl = currentParams
      ? `/batches?${currentParams}`
      : `/batches?tab=${activeTab}`;
    navigate(
      `/batches/files/${file.id}/content?from=${encodeURIComponent(fromUrl)}`,
    );
  };

  const handleDeleteFile = (file: FileObject) => {
    if ((file as any)._isEmpty) return;
    onOpenDeleteDialog(file);
  };

  const handleDownloadFileCode = (file: FileObject) => {
    if ((file as any)._isEmpty) return;

    // Check if this is a partial file (output/error file from an in-progress batch)
    const isPartial =
      (file.purpose === "batch_output" || file.purpose === "batch_error") &&
      batches.some(
        (b) =>
          (b.output_file_id === file.id || b.error_file_id === file.id) &&
          ["validating", "in_progress", "finalizing"].includes(b.status),
      );

    onOpenDownloadModal({
      type: "file",
      id: file.id,
      filename: file.filename,
      isPartial,
    });
  };

  const handleTriggerBatch = (file: FileObject) => {
    if ((file as any)._isEmpty) return;
    onOpenCreateBatchModal(file);
  };

  const handleFileClick = (file: FileObject) => {
    if ((file as any)._isEmpty) return;
    // Navigate to batches tab with file filter
    updateURL("batches", file.id);
  };

  // Batch actions
  const handleCancelBatch = (batch: Batch) => {
    if ((batch as any)._isEmpty) return;
    onOpenCancelDialog(batch);
  };

  const handleDeleteBatch = (batch: Batch) => {
    if ((batch as any)._isEmpty) return;
    onOpenDeleteBatchDialog(batch);
  };

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
        if (activeTab === "batches") {
          onOpenCreateBatchModal(file);
        } else if (activeTab === "files") {
          onOpenUploadModal(file);
        }
      }
    }
  };

  // Get input file ID for a batch
  const getInputFile = (batch: Batch) => {
    if (!batch.input_file_id) return undefined;
    return { id: batch.input_file_id, purpose: "batch" };
  };

  // Check if a file's associated batch is still in progress
  const isFileInProgress = React.useCallback(
    (file: FileObject) => {
      // Only output and error files can be in progress
      if (file.purpose !== "batch_output" && file.purpose !== "batch_error") {
        return false;
      }

      // Find the batch that created this file
      const batch = batches.find(
        (b) => b.output_file_id === file.id || b.error_file_id === file.id,
      );

      if (!batch) return false;

      // Check if batch is in an active state (NOT completed, failed, cancelled, expired, or cancelling)
      const activeStatuses: string[] = [
        "validating",
        "in_progress",
        "finalizing",
      ];
      return activeStatuses.includes(batch.status);
    },
    [batches],
  );

  // Create columns with actions
  const fileColumns = createFileColumns({
    onView: handleViewFileRequests,
    onDelete: handleDeleteFile,
    onDownloadCode: handleDownloadFileCode,
    onTriggerBatch: handleTriggerBatch,
    onViewBatches: handleFileClick,
    isFileInProgress,
    fileEstimates: fileEstimatesMap,
  });

  const handleBatchClick = (batch: Batch) => {
    if ((batch as any)._isEmpty) return;
    // Preserve current URL params (pagination, search, filters) when navigating to batch detail
    const currentParams = searchParams.toString();
    const fromUrl = currentParams ? `/batches?${currentParams}` : "/batches";
    navigate(`/batches/${batch.id}?from=${encodeURIComponent(fromUrl)}`);
  };

  const batchColumns = createBatchColumns({
    onCancel: handleCancelBatch,
    onDelete: handleDeleteBatch,
    getBatchFiles,
    onViewFile: handleViewFileRequests,
    getInputFile,
    onRowClick: handleBatchClick,
    batchAnalytics: batchAnalyticsMap,
  });

  return (
    <div
      className="py-4 px-6"
      onDragEnter={handleDrag}
      onDragLeave={handleDrag}
      onDragOver={handleDrag}
      onDrop={handleDrop}
    >
      <Tabs
        value={activeTab}
        onValueChange={(v) => updateURL(v as "files" | "batches", null)}
        className="space-y-4"
      >
        {/* Header with Tabs and Actions */}
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

          {/* Right: Buttons + Tabs */}
          <div className="flex flex-col sm:flex-row items-stretch sm:items-center gap-3 lg:shrink-0">
            {/* Action Button - changes based on active tab */}
            {activeTab === "batches" ? (
              <Button
                onClick={() => onOpenCreateBatchModal()}
                variant="outline"
                className="flex-1 sm:flex-none"
              >
                <Play className="w-4 h-4 mr-2" />
                Create Batch
              </Button>
            ) : (
              <Button
                onClick={() => onOpenUploadModal()}
                variant="outline"
                className={`flex-1 sm:flex-none transition-all duration-200 ${
                  dragActive ? "border-blue-500 bg-blue-50 text-blue-700" : ""
                }`}
              >
                <Upload className="w-4 h-4 mr-2" />
                Upload File
              </Button>
            )}

            {/* Tabs Selector */}
            <TabsList className="w-full sm:w-auto">
              <TabsTrigger
                value="batches"
                className="flex items-center gap-2 flex-1 sm:flex-none"
              >
                <Box className="w-4 h-4" />
                Batches
              </TabsTrigger>
              <TabsTrigger
                value="files"
                className="flex items-center gap-2 flex-1 sm:flex-none"
              >
                <FileInput className="w-4 h-4" />
                Files
              </TabsTrigger>
            </TabsList>
          </div>
        </div>

        {/* Content */}
        <TabsContent value="batches" className="space-y-4">
          {/* Show filter indicator if active */}
          {batchFileFilter && (
            <div className="flex items-center gap-2 bg-blue-50 border border-blue-200 rounded-lg p-3">
              <FileInput className="w-4 h-4 text-blue-600" />
              <span className="text-sm text-blue-900">
                Showing batches for file:{" "}
                <span className="font-mono">
                  {files.find((f) => f.id === batchFileFilter)?.filename ||
                    batchFileFilter}
                </span>
              </span>
              <Button
                variant="ghost"
                size="sm"
                onClick={() => updateURL("batches", null)}
                className="ml-auto h-auto py-1 px-2 text-blue-600 hover:text-blue-800 hover:bg-blue-100"
              >
                Clear filter
              </Button>
            </div>
          )}
          <DataTable
            columns={batchColumns}
            data={filteredBatches}
            searchPlaceholder="Search batches..."
            externalSearch={{
              value: batchSearchQuery,
              onChange: (value) => {
                setBatchSearchQuery(value);
                batchesPagination.handleFirstPage();
              },
            }}
            showColumnToggle={true}
            pageSize={batchesPagination.pageSize}
            minRows={batchesPagination.pageSize}
            rowHeight="40px"
            initialColumnVisibility={{ id: false }}
            onRowClick={handleBatchClick}
            isLoading={batchesLoading}
            emptyState={
              <div className="text-center py-12">
                <div className="p-4 bg-doubleword-neutral-100 rounded-full w-16 h-16 mx-auto mb-4 flex items-center justify-center">
                  <Box className="w-8 h-8 text-doubleword-neutral-600" />
                </div>
                <h3 className="text-lg font-medium text-doubleword-neutral-900 mb-2">
                  No batches found
                </h3>
                <p className="text-doubleword-neutral-600 mb-4">
                  {batchSearchQuery
                    ? "Try a different search term"
                    : "Create a batch from an uploaded file to start processing requests"}
                </p>
                {!batchSearchQuery && (
                  <Button
                    onClick={() => {
                      const file = batchFileFilter
                        ? files.find((f) => f.id === batchFileFilter)
                        : undefined;
                      onOpenCreateBatchModal(file);
                    }}
                  >
                    <Play className="w-4 h-4 mr-2" />
                    {batchFileFilter ? "Create Batch" : "Create First Batch"}
                  </Button>
                )}
              </div>
            }
            headerActions={
              <div className="flex items-center gap-2">
                <span className="text-sm text-gray-600">Rows:</span>
                <Select
                  value={batchesPagination.pageSize.toString()}
                  onValueChange={(value) =>
                    batchesPagination.handlePageSizeChange(Number(value))
                  }
                >
                  <SelectTrigger className="w-20p h-9">
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
            paginationMode="server-cursor"
            serverPagination={{
              page: batchesPagination.page,
              pageSize: batchesPagination.pageSize,
              onNextPage: () => {
                const lastBatch = batches[batches.length - 1];
                if (lastBatch) {
                  batchesPagination.handleNextPage(lastBatch.id);
                }
              },
              onPrevPage: batchesPagination.handlePrevPage,
              onFirstPage: batchesPagination.handleFirstPage,
              hasNextPage: batchesHasMore,
              hasPrevPage: batchesPagination.hasPrevPage,
            }}
          />
        </TabsContent>

        <TabsContent value="files" className="space-y-4">
          <DataTable
            columns={fileColumns}
            data={files}
            searchPlaceholder="Search files..."
            externalSearch={{
              value: fileSearchQuery,
              onChange: (value) => {
                setFileSearchQuery(value);
                filesPagination.handleFirstPage();
              },
            }}
            showColumnToggle={true}
            pageSize={filesPagination.pageSize}
            minRows={filesPagination.pageSize}
            rowHeight="40px"
            initialColumnVisibility={{ id: false }}
            isLoading={filesLoading}
            emptyState={
              <div className="text-center py-12">
                <div className="p-4 bg-doubleword-neutral-100 rounded-full w-16 h-16 mx-auto mb-4 flex items-center justify-center">
                  <FileInput className="w-8 h-8 text-doubleword-neutral-600" />
                </div>
                <h3 className="text-lg font-medium text-doubleword-neutral-900 mb-2">
                  {fileSearchQuery ? "No matching files" : "No files uploaded"}
                </h3>
                <p className="text-doubleword-neutral-600 mb-4">
                  {fileSearchQuery
                    ? "No files match your search. Try a different search term."
                    : "Upload a .jsonl file to get started with batch processing"}
                </p>
                {!fileSearchQuery && (
                  <Button onClick={() => onOpenUploadModal()}>
                    <Upload className="w-4 h-4 mr-2" />
                    Upload First File
                  </Button>
                )}
              </div>
            }
            headerActions={
              <div className="flex items-center gap-2">
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
                          // Reset pagination and update file type in a single setSearchParams call
                          setSearchParams(
                            (prev) => {
                              const params = new URLSearchParams(prev);
                              params.set("tab", activeTab);
                              if (batchFileFilter) {
                                params.set("fileFilter", batchFileFilter);
                              } else {
                                params.delete("fileFilter");
                              }
                              if (type !== "input") {
                                params.set("fileType", type);
                              } else {
                                params.delete("fileType");
                              }
                              // Reset pagination
                              params.set("filesPage", "1");
                              params.delete("filesAfter");
                              return params;
                            },
                            { replace: false },
                          );
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
                <span className="text-sm text-gray-600">Rows:</span>
                <Select
                  value={filesPagination.pageSize.toString()}
                  onValueChange={(value) =>
                    filesPagination.handlePageSizeChange(Number(value))
                  }
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
            paginationMode="server-cursor"
            serverPagination={{
              page: filesPagination.page,
              pageSize: filesPagination.pageSize,
              onNextPage: () => {
                const lastFile = files[files.length - 1];
                if (lastFile) {
                  filesPagination.handleNextPage(lastFile.id);
                }
              },
              onPrevPage: filesPagination.handlePrevPage,
              onFirstPage: filesPagination.handleFirstPage,
              hasNextPage: filesHasMore,
              hasPrevPage: filesPagination.hasPrevPage,
            }}
          />
        </TabsContent>
      </Tabs>
    </div>
  );
}
