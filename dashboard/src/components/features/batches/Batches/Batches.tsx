import { useState, useEffect } from "react";
import * as React from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { useQueryClient } from "@tanstack/react-query";
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
  onBatchCreatedCallback?: (callback: () => void) => void;
}

export function Batches({
  onOpenUploadModal,
  onOpenCreateBatchModal,
  onOpenDownloadModal,
  onOpenDeleteDialog,
  onOpenCancelDialog,
  onBatchCreatedCallback,
}: BatchesProps) {
  const navigate = useNavigate();
  const [searchParams, setSearchParams] = useSearchParams();
  const queryClient = useQueryClient();

  // Drag and drop state (kept locally as it's UI-only)
  const [dragActive, setDragActive] = useState(false);

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

  // Read state from URL
  const activeTab = (searchParams.get("tab") as "files" | "batches") || "files";
  const batchFileFilter = searchParams.get("fileFilter");
  const fileTypeFilter =
    (searchParams.get("fileType") as "input" | "output" | "error") || "input";

  // Pagination state from URL
  const filesPage = parseInt(searchParams.get("filesPage") || "0", 10);
  const filesPageSize = parseInt(searchParams.get("filesPageSize") || "10", 10);
  const [filesAfterCursor, setFilesAfterCursor] = useState<string | undefined>(
    undefined,
  );

  const batchesPage = parseInt(searchParams.get("batchesPage") || "0", 10);
  const batchesPageSize = parseInt(
    searchParams.get("batchesPageSize") || "10",
    10,
  );
  const [batchesAfterCursor, setBatchesAfterCursor] = useState<
    string | undefined
  >(undefined);

  // Cursor history for backwards pagination
  const filesCursorHistory = React.useRef<(string | undefined)[]>([]);
  const batchesCursorHistory = React.useRef<(string | undefined)[]>([]);

  // Update files pagination in URL
  const updateFilesPagination = (newPage: number, newPageSize: number) => {
    const params = new URLSearchParams(searchParams);
    params.set("filesPage", newPage.toString());
    params.set("filesPageSize", newPageSize.toString());
    setSearchParams(params, { replace: true });
  };

  // Update batches pagination in URL
  const updateBatchesPagination = (newPage: number, newPageSize: number) => {
    const params = new URLSearchParams(searchParams);
    params.set("batchesPage", newPage.toString());
    params.set("batchesPageSize", newPageSize.toString());
    setSearchParams(params, { replace: true });
  };

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
    enabled: activeTab === "files", // Only fetch when on files tab
  });

  // Paginated batches query
  const { data: batchesResponse, isLoading: batchesLoading } = useBatches({
    limit: batchesPageSize + 1, // Fetch one extra to detect if there are more
    after: batchesAfterCursor,
    enabled: activeTab === "batches", // Only fetch when on batches tab
  });

  // Process batches response - remove extra item used for hasMore detection
  const batchesData = batchesResponse?.data || [];
  const batchesHasMore = batchesData.length > batchesPageSize;
  const batches = batchesHasMore
    ? batchesData.slice(0, batchesPageSize)
    : batchesData;

  // Process files response - remove extra item used for hasMore detection
  const filesData = filesResponse?.data || [];
  const filesHasMore = filesData.length > filesPageSize;
  const filesForDisplay = filesHasMore
    ? filesData.slice(0, filesPageSize)
    : filesData;

  // Display files as returned by API (server-side filtered by purpose)
  const files = filesForDisplay;

  const isFilesLoading = filesLoading;

  // Filter batches by input file if filter is set
  const filteredBatches = React.useMemo(() => {
    if (!batchFileFilter) return batches;
    return batches.filter((b) => b.input_file_id === batchFileFilter);
  }, [batches, batchFileFilter]);

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

  // Prefetch next page for batches - only if user has already started paginating
  useEffect(() => {
    if (batchesHasMore && batches.length > 0 && batchesPage > 0) {
      const lastBatch = batches[batches.length - 1];
      const nextCursor = lastBatch.id;

      queryClient.prefetchQuery({
        queryKey: [
          "batches",
          "list",
          { limit: batchesPageSize + 1, after: nextCursor },
        ],
        queryFn: () =>
          dwctlApi.batches.list({
            limit: batchesPageSize + 1,
            after: nextCursor,
          }),
      });
    }
  }, [batches, batchesHasMore, batchesPage, batchesPageSize, queryClient]);

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
    navigate(`/batches/files/${file.id}/content?returnTab=${activeTab}`);
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

  // Get input file ID for a batch
  const getInputFile = (batch: Batch) => {
    return { id: batch.input_file_id, purpose: "batch" };
  };

  // Pagination handlers
  const handleFilesNextPage = () => {
    const lastFile = files[files.length - 1];
    if (lastFile && filesHasMore) {
      // Save current cursor to history before moving forward
      filesCursorHistory.current[filesPage] = filesAfterCursor;
      setFilesAfterCursor(lastFile.id);
      updateFilesPagination(filesPage + 1, filesPageSize);
    }
  };

  const handleFilesPrevPage = () => {
    if (filesPage > 0) {
      // Use cursor history to go back one page
      const previousCursor = filesCursorHistory.current[filesPage - 1];
      setFilesAfterCursor(previousCursor);
      updateFilesPagination(filesPage - 1, filesPageSize);
    }
  };

  const handleFilesPageSizeChange = (newSize: number) => {
    setFilesAfterCursor(undefined);
    filesCursorHistory.current = []; // Clear history when changing page size
    updateFilesPagination(0, newSize);
  };

  const handleBatchesNextPage = () => {
    const lastBatch = batches[batches.length - 1];
    if (lastBatch && batchesHasMore) {
      // Save current cursor to history before moving forward
      batchesCursorHistory.current[batchesPage] = batchesAfterCursor;
      setBatchesAfterCursor(lastBatch.id);
      updateBatchesPagination(batchesPage + 1, batchesPageSize);
    }
  };

  const handleBatchesPrevPage = () => {
    if (batchesPage > 0) {
      // Use cursor history to go back one page
      const previousCursor = batchesCursorHistory.current[batchesPage - 1];
      setBatchesAfterCursor(previousCursor);
      updateBatchesPagination(batchesPage - 1, batchesPageSize);
    }
  };

  const handleBatchesPageSizeChange = (newSize: number) => {
    setBatchesAfterCursor(undefined);
    batchesCursorHistory.current = []; // Clear history when changing page size
    updateBatchesPagination(0, newSize);
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
  });

  const batchColumns = createBatchColumns({
    onCancel: handleCancelBatch,
    getBatchFiles,
    onViewFile: handleViewFileRequests,
    getInputFile,
  });

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
      {/* Header with Tabs and Actions */}
      <div className="mb-6 flex flex-col lg:flex-row lg:items-center lg:justify-between gap-4">
        {/* Left: Title */}
        <div className="flex-shrink-0">
          <h1 className="text-3xl font-bold text-doubleword-neutral-900">
            Batch Processing
          </h1>
          <p className="text-doubleword-neutral-600 mt-1">
            Upload files and create batches to process requests at scale
          </p>
        </div>

        {/* Right: Buttons + Tabs */}
        <div className="flex flex-col sm:flex-row items-stretch sm:items-center gap-3 lg:flex-shrink-0">
          {/* Action Button */}
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

          {/* Tabs Selector */}
          <Tabs
            value={activeTab}
            onValueChange={(v) => updateURL(v as "files" | "batches", null)}
            className="w-full sm:w-auto"
          >
            <TabsList className="w-full sm:w-auto">
              <TabsTrigger
                value="files"
                className="flex items-center gap-2 flex-1 sm:flex-none"
              >
                <FileInput className="w-4 h-4" />
                Files ({files.length})
              </TabsTrigger>
              <TabsTrigger
                value="batches"
                className="flex items-center gap-2 flex-1 sm:flex-none"
              >
                <Box className="w-4 h-4" />
                Batches ({batches.length})
              </TabsTrigger>
            </TabsList>
          </Tabs>
        </div>
      </div>

      {/* Content */}
      <Tabs
        value={activeTab}
        onValueChange={(v) => updateURL(v as "files" | "batches", null)}
        className="space-y-4"
      >
        <TabsContent value="files" className="space-y-4">
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
            <>
              <DataTable
                columns={fileColumns}
                data={files}
                searchPlaceholder="Search files..."
                showPagination={false}
                showColumnToggle={true}
                pageSize={filesPageSize}
                minRows={filesPageSize}
                rowHeight="40px"
                initialColumnVisibility={{ id: false }}
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
                        const label =
                          type.charAt(0).toUpperCase() + type.slice(1);

                        return (
                          <button
                            key={type}
                            type="button"
                            title={`${label} files`}
                            onClick={(e) => {
                              e.preventDefault();
                              e.stopPropagation();
                              // Combine URL updates into single setSearchParams call
                              const params = new URLSearchParams(searchParams);
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
                              params.set("filesPage", "0");
                              params.set(
                                "filesPageSize",
                                filesPageSize.toString(),
                              );
                              setSearchParams(params, { replace: false });
                              // Reset cursor and history
                              setFilesAfterCursor(undefined);
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
                    <span className="text-sm text-gray-600">Rows:</span>
                    <Select
                      value={filesPageSize.toString()}
                      onValueChange={(value) =>
                        handleFilesPageSizeChange(Number(value))
                      }
                    >
                      <SelectTrigger className="w-[80px] h-9">
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
                  </div>
                }
              />
              {/* Server-side pagination controls */}
              <div className="flex items-center justify-between px-2 py-0">
                <div className="text-sm text-gray-700">
                  Showing {filesPage * filesPageSize + 1} -{" "}
                  {filesPage * filesPageSize + files.length}
                  {filesHasMore && " of many"}
                </div>
                <div className="flex items-center gap-2">
                  {filesPage > 1 && (
                    <Button
                      variant="outline"
                      size="sm"
                      onClick={() => {
                        setFilesAfterCursor(undefined);
                        filesCursorHistory.current = []; // Clear history when jumping to first page
                        updateFilesPagination(0, filesPageSize);
                      }}
                    >
                      First
                    </Button>
                  )}
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={handleFilesPrevPage}
                    disabled={filesPage === 0}
                  >
                    Previous
                  </Button>
                  <span className="text-sm text-gray-700">
                    Page {filesPage + 1}
                  </span>
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={handleFilesNextPage}
                    disabled={!filesHasMore}
                  >
                    Next
                  </Button>
                </div>
              </div>
            </>
          )}
        </TabsContent>

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
          {filteredBatches.length === 0 ? (
            <div className="text-center py-12">
              <div className="p-4 bg-doubleword-neutral-100 rounded-full w-16 h-16 mx-auto mb-4 flex items-center justify-center">
                <Box className="w-8 h-8 text-doubleword-neutral-600" />
              </div>
              <h3 className="text-lg font-medium text-doubleword-neutral-900 mb-2">
                No batches created
              </h3>
              <p className="text-doubleword-neutral-600 mb-4">
                Create a batch from an uploaded file to start processing
                requests
              </p>
              <Button
                onClick={() => {
                  // Find the file object if we have a filter
                  const file = batchFileFilter
                    ? files.find((f) => f.id === batchFileFilter)
                    : undefined;
                  onOpenCreateBatchModal(file);
                }}
              >
                <Play className="w-4 h-4 mr-2" />
                {batchFileFilter ? "Create Batch" : "Create First Batch"}
              </Button>
            </div>
          ) : (
            <>
              <DataTable
                columns={batchColumns}
                data={filteredBatches}
                searchPlaceholder="Search batches..."
                showPagination={false}
                showColumnToggle={true}
                pageSize={batchesPageSize}
                minRows={batchesPageSize}
                rowHeight="40px"
                initialColumnVisibility={{ id: false }}
                headerActions={
                  <div className="flex items-center gap-2">
                    <span className="text-sm text-gray-600">Rows:</span>
                    <Select
                      value={batchesPageSize.toString()}
                      onValueChange={(value) =>
                        handleBatchesPageSizeChange(Number(value))
                      }
                    >
                      <SelectTrigger className="w-[80px] h-9">
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
                  </div>
                }
              />
              {/* Server-side pagination controls */}
              <div className="flex items-center justify-between px-2 py-0">
                <div className="text-sm text-gray-700">
                  Showing {batchesPage * batchesPageSize + 1} -{" "}
                  {batchesPage * batchesPageSize + filteredBatches.length}
                  {batchesHasMore && " of many"}
                </div>
                <div className="flex items-center gap-2">
                  {batchesPage > 1 && (
                    <Button
                      variant="outline"
                      size="sm"
                      onClick={() => {
                        setBatchesAfterCursor(undefined);
                        batchesCursorHistory.current = []; // Clear history when jumping to first page
                        updateBatchesPagination(0, batchesPageSize);
                      }}
                    >
                      First
                    </Button>
                  )}
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={handleBatchesPrevPage}
                    disabled={batchesPage === 0}
                  >
                    Previous
                  </Button>
                  <span className="text-sm text-gray-700">
                    Page {batchesPage + 1}
                  </span>
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={handleBatchesNextPage}
                    disabled={!batchesHasMore}
                  >
                    Next
                  </Button>
                </div>
              </div>
            </>
          )}
        </TabsContent>
      </Tabs>
    </div>
  );
}
