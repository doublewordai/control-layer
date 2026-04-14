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
  X,
  Users,
  ChevronsUpDown,
  Check,
  Filter,
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
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "../../../ui/popover";
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "../../../ui/command";
import { Switch } from "../../../ui/switch";
import { DateTimeRangeSelector } from "../../../ui/date-time-range-selector";
import { DataTable } from "../../../ui/data-table";
import { createFileColumns } from "../FilesTable/columns";
import { createBatchColumns } from "../BatchesTable/columns";
import {
  useFiles,
  useBatches,
  useOrganizationMembers,
  useUsers,
} from "../../../../api/control-layer/hooks";
import { dwctlApi } from "../../../../api/control-layer/client";
import type { FileObject, Batch } from "../types";
import type {
  BatchStatus,
} from "../../../../api/control-layer/types";
import { useServerCursorPagination } from "../../../../hooks/useServerCursorPagination";
import { useDebounce } from "../../../../hooks/useDebounce";
import { useAuthorization } from "../../../../utils/authorization";
import { useOrganizationContext } from "../../../../contexts/organization/useOrganizationContext";
import { useBootstrapContent } from "@/hooks/use-bootstrap-content";
import { cn } from "@/lib/utils";

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
  const { userRoles, hasPermission } = useAuthorization();
  const { isOrgContext, activeOrganizationId } = useOrganizationContext();

  // Show User column for PlatformManagers (see all batches) or in org context (see org members)
  const isPlatformManager = userRoles.includes("PlatformManager");
  const showUserColumn = isPlatformManager || isOrgContext;
  // Context and Source columns are used by the file table only
  const showContextColumn = isPlatformManager;
  const showSourceColumn = hasPermission("connections");

  // Member filter:
  // - Org context (all users): show org members dropdown (client-side filtered)
  // - Personal context (PM only): show users via server-side search
  const showMemberFilter = isOrgContext || (isPlatformManager && !isOrgContext);
  const useServerSideMemberSearch = isPlatformManager && !isOrgContext;
  const { data: orgMembers } = useOrganizationMembers(
    activeOrganizationId || "",
  );

  // Server-side user search for PM personal mode
  const [memberSearch, setMemberSearch] = useState("");
  const debouncedMemberSearch = useDebounce(memberSearch, 300);
  const { data: searchedUsers } = useUsers({
    search: debouncedMemberSearch,
    limit: 50,
    enabled: useServerSideMemberSearch,
  });

  const memberList = React.useMemo(() => {
    // Org context: show org members (client-side filtered by Command)
    if (isOrgContext && orgMembers) {
      return orgMembers
        .filter((m) => m.status === "active" && m.user)
        .map((m) => ({ id: m.user!.id, email: m.user!.email }));
    }
    // Personal context + PM: show server-side searched users.
    // Deduplicate by email (a user may appear twice if they have both personal
    // and org-created individual records). The personal member_id is used under
    // the hood — the backend expands it to cover both personal and org contexts.
    if (useServerSideMemberSearch && searchedUsers?.data) {
      const seen = new Set<string>();
      return searchedUsers.data
        .filter((u) => u.user_type !== "organization")
        .filter((u) => {
          if (seen.has(u.email)) return false;
          seen.add(u.email);
          return true;
        })
        .map((u) => ({ id: u.id, email: u.email }));
    }
    return [];
  }, [isOrgContext, useServerSideMemberSearch, orgMembers, searchedUsers]);

  const [selectedMemberId, setSelectedMemberId] = useState<string | undefined>(
    undefined,
  );
  // Track the selected member's email so it persists when search results change
  const [selectedMemberEmail, setSelectedMemberEmail] = useState<
    string | undefined
  >(undefined);
  const [memberPopoverOpen, setMemberPopoverOpen] = useState(false);

  // Batch-specific filters
  const [statusFilter, setStatusFilter] = useState<BatchStatus | "all">("all");
  const [sortActiveFirst, setSortActiveFirst] = useState(true);
  const [dateRange, setDateRange] = useState<
    { from: Date; to: Date } | undefined
  >(undefined);

  // Clear all filters and reset pagination when org context changes
  useEffect(() => {
    setSelectedMemberId(undefined);
    setSelectedMemberEmail(undefined);
    setMemberSearch("");
    setStatusFilter("all");
    setDateRange(undefined);
    filesPagination.handleFirstPage();
    batchesPagination.handleFirstPage();
    // Also clear file filter from URL
    setSearchParams((prev) => {
      const params = new URLSearchParams(prev);
      params.delete("fileFilter");
      return params;
    }, { replace: true });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeOrganizationId]);

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
    defaultPageSize: 10,
  });

  const batchesPagination = useServerCursorPagination({
    paramPrefix: "batches",
    defaultPageSize: 10,
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
    member_id: selectedMemberId,
    ...filesPagination.queryParams,
    enabled: activeTab === "files" || !!batchFileFilter,
  });

  // Paginated batches query - include analytics to avoid N+1 requests
  const { data: batchesResponse, isLoading: batchesLoading } = useBatches({
    search: debouncedBatchSearch.trim() || undefined,
    include: "analytics",
    member_id: selectedMemberId,
    status: statusFilter !== "all" ? statusFilter : undefined,
    created_after: dateRange?.from.toISOString(),
    created_before: dateRange?.to.toISOString(),
    active_first: sortActiveFirst || undefined,
    ...batchesPagination.queryParams,
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

  // Apply client-side filters to batches (sorting is now server-side via active_first param)
  const filteredBatches = React.useMemo(() => {
    let result = batches;

    // Filter by input file (client-side, from file detail view)
    if (batchFileFilter) {
      result = result.filter((b) => b.input_file_id === batchFileFilter);
    }

    return result;
  }, [batches, batchFileFilter]);

  // Prefetch next page for files - only if user has already started paginating
  useEffect(() => {
    if (filesHasMore && files.length > 0 && filesPagination.page > 0) {
      const lastFile = files[files.length - 1];
      const nextCursor = lastFile.id;

      const prefetchOptions = {
        purpose: filePurpose,
        search: debouncedFileSearch.trim() || undefined,
        member_id: selectedMemberId,
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
    debouncedFileSearch,
    selectedMemberId,
    queryClient,
  ]);

  // Prefetch next page for batches - only if user has already started paginating
  useEffect(() => {
    if (batchesHasMore && batches.length > 0 && batchesPagination.page > 0) {
      const lastBatch = batches[batches.length - 1];
      const nextCursor = lastBatch.id;

      const prefetchOptions = {
        search: debouncedBatchSearch.trim() || undefined,
        include: "analytics" as const,
        member_id: selectedMemberId,
        status:
          statusFilter !== "all" ? statusFilter : undefined,
        created_after: dateRange?.from.toISOString(),
        created_before: dateRange?.to.toISOString(),
        active_first: sortActiveFirst || undefined,
        limit: batchesPagination.pageSize + 1,
        after: nextCursor,
      };

      queryClient.prefetchQuery({
        queryKey: ["batches", "list", prefetchOptions],
        queryFn: () => dwctlApi.batches.list(prefetchOptions),
      });
    }
  }, [
    batches,
    batchesHasMore,
    batchesPagination.page,
    batchesPagination.pageSize,
    debouncedBatchSearch,
    selectedMemberId,
    statusFilter,
    dateRange,
    sortActiveFirst,
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
    showUserColumn,
    showContextColumn,
    showSourceColumn,
  });

  const handleBatchClick = (batch: Batch) => {
    if ((batch as any)._isEmpty) return;
    // Preserve current URL params (pagination, search, filters) when navigating to batch detail
    const currentParams = searchParams.toString();
    const fromUrl = currentParams ? `/workloads/batch?${currentParams}` : "/workloads/batch";
    navigate(`/workloads/batch/${batch.id}?from=${encodeURIComponent(fromUrl)}`);
  };

  const batchColumns = createBatchColumns({
    onCancel: handleCancelBatch,
    onDelete: handleDeleteBatch,
    getBatchFiles,
    onViewFile: handleViewFileRequests,
    getInputFile,
    onRowClick: handleBatchClick,
    showUserColumn,
  });

  // Searchable member filter combobox - shared between batches and files tabs
  const displayedMemberEmail =
    selectedMemberEmail ||
    memberList.find((m) => m.id === selectedMemberId)?.email;
  // Show member filter: always for PM personal mode (server-side search),
  // only when org members exist for org context (client-side filtered)
  const showMemberCombobox =
    showMemberFilter &&
    (useServerSideMemberSearch || memberList.length > 0);
  const memberFilterCombobox = showMemberCombobox && (
    <Popover open={memberPopoverOpen} onOpenChange={setMemberPopoverOpen}>
      <PopoverTrigger asChild>
        <Button
          variant="outline"
          role="combobox"
          aria-expanded={memberPopoverOpen}
          className="w-[220px] h-9 justify-between font-normal"
        >
          <div className="flex items-center gap-1.5 truncate">
            <Users className="w-3.5 h-3.5 shrink-0 text-gray-500" />
            <span className="truncate">
              {displayedMemberEmail || "All members"}
            </span>
          </div>
          <ChevronsUpDown className="ml-1 h-3.5 w-3.5 shrink-0 opacity-50" />
        </Button>
      </PopoverTrigger>
      <PopoverContent className="w-[280px] p-0" align="start">
        <Command shouldFilter={!useServerSideMemberSearch}>
          <CommandInput
            placeholder="Search by email..."
            value={useServerSideMemberSearch ? memberSearch : undefined}
            onValueChange={
              useServerSideMemberSearch ? setMemberSearch : undefined
            }
          />
          <CommandList>
            <CommandEmpty>No members found.</CommandEmpty>
            <CommandGroup>
              <CommandItem
                value="all-members"
                onSelect={() => {
                  setSelectedMemberId(undefined);
                  setSelectedMemberEmail(undefined);
                  setMemberSearch("");
                  setMemberPopoverOpen(false);
                  batchesPagination.handleFirstPage();
                  filesPagination.handleFirstPage();
                }}
              >
                <Check
                  className={cn(
                    "mr-2 h-4 w-4",
                    !selectedMemberId ? "opacity-100" : "opacity-0",
                  )}
                />
                All members
              </CommandItem>
              {memberList.map((member) => (
                <CommandItem
                  key={member.id}
                  value={member.email}
                  onSelect={() => {
                    setSelectedMemberId(member.id);
                    setSelectedMemberEmail(member.email);
                    setMemberSearch("");
                    setMemberPopoverOpen(false);
                    batchesPagination.handleFirstPage();
                    filesPagination.handleFirstPage();
                  }}
                >
                  <Check
                    className={cn(
                      "mr-2 h-4 w-4",
                      selectedMemberId === member.id
                        ? "opacity-100"
                        : "opacity-0",
                    )}
                  />
                  <span className="truncate">{member.email}</span>
                </CommandItem>
              ))}
            </CommandGroup>
          </CommandList>
        </Command>
      </PopoverContent>
    </Popover>
  );

  const bootstrapBanner = useBootstrapContent();

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
              {activeTab === "batches" ? "Batches" : "Batch Files"}
            </h1>
            <p className="text-doubleword-neutral-600 mt-1">
              {activeTab === "batches"
                ? "Create and manage batch requests"
                : "Upload and manage files for batch processing"}
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

        {/* Bootstrap Banner */}
        {bootstrapBanner.content && !bootstrapBanner.isClosed && (
          <div className="relative mb-6">
            <div
              dangerouslySetInnerHTML={{ __html: bootstrapBanner.content }}
            />
            <button
              onClick={bootstrapBanner.close}
              className="absolute top-3 right-3 rounded-sm opacity-50 transition-opacity hover:opacity-100 focus:ring-2 focus:ring-ring focus:ring-offset-2 focus:outline-hidden"
              aria-label="Close banner"
            >
              <X className="h-4 w-4 text-doubleword-neutral-600" />
            </button>
          </div>
        )}

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
              <div className="flex items-center gap-2 flex-wrap">
                <div className="flex items-center gap-1.5">
                  <Switch
                    id="active-first"
                    checked={sortActiveFirst}
                    onCheckedChange={(checked) => {
                      setSortActiveFirst(checked);
                      batchesPagination.handleFirstPage();
                    }}
                  />
                  <label
                    htmlFor="active-first"
                    className="text-sm text-gray-600 cursor-pointer select-none"
                  >
                    Active first
                  </label>
                </div>
                {memberFilterCombobox}
                <Select
                  value={statusFilter}
                  onValueChange={(v) => {
                    setStatusFilter(v as BatchStatus | "all");
                    batchesPagination.handleFirstPage();
                  }}
                >
                  <SelectTrigger className="w-[140px] h-9">
                    <div className="flex items-center gap-1.5">
                      <Filter className="w-3.5 h-3.5 text-gray-500" />
                      <SelectValue />
                    </div>
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="all">All statuses</SelectItem>
                    <SelectItem value="in_progress">In Progress</SelectItem>
                    <SelectItem value="completed">Completed</SelectItem>
                    <SelectItem value="failed">Failed</SelectItem>
                    <SelectItem value="cancelled">Cancelled</SelectItem>
                    {isPlatformManager && (
                      <SelectItem value="expired">Expired (SLA)</SelectItem>
                    )}
                  </SelectContent>
                </Select>
                <DateTimeRangeSelector
                  value={dateRange}
                  onChange={(range) => {
                    setDateRange(range);
                    batchesPagination.handleFirstPage();
                  }}
                />
                <span className="text-sm text-gray-600">Rows:</span>
                <Select
                  value={batchesPagination.pageSize.toString()}
                  onValueChange={(value) =>
                    batchesPagination.handlePageSizeChange(Number(value))
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
            initialColumnVisibility={{}}
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
              <div className="flex items-center gap-2 flex-wrap">
                {memberFilterCombobox}
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
