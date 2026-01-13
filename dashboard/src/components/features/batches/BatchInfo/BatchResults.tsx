import { useState, useMemo, useEffect } from "react";
import { useSearchParams } from "react-router-dom";
import { FileInput, AlertCircle } from "lucide-react";
import { useDebounce } from "../../../../hooks/useDebounce";
import { DataTable } from "../../../ui/data-table";
import { CursorPagination } from "../../../ui/cursor-pagination";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../../ui/select";
import { useBatchResults } from "../../../../api/control-layer/hooks";
import type { BatchResultItem } from "../../../../api/control-layer/types";
import { useServerPagination } from "../../../../hooks/useServerPagination";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
} from "../../../ui/dialog";
import { CodeBlock } from "../../../ui/code-block";
import { createBatchResultsColumns } from "./batch-results-columns";
import type { BatchStatus } from "../../../../api/control-layer/types";

interface BatchResultsProps {
  batchId: string;
  batchStatus?: BatchStatus;
  inputFileDeleted?: boolean;
}

export default function BatchResults({
  batchId,
  batchStatus,
  inputFileDeleted,
}: BatchResultsProps) {
  const [searchParams, setSearchParams] = useSearchParams();

  // Modal state for viewing content
  const [selectedResult, setSelectedResult] = useState<BatchResultItem | null>(
    null,
  );
  const [contentModalOpen, setContentModalOpen] = useState(false);
  const [modalContentType, setModalContentType] = useState<
    "input" | "response"
  >("input");

  // Search state with debounce for server-side filtering
  const [searchInput, setSearchInput] = useState("");
  const debouncedSearch = useDebounce(searchInput, 300);

  // Status filter state - initialized from URL param
  const statusFromUrl = searchParams.get("status");
  const [statusFilter, setStatusFilter] = useState<string>(
    statusFromUrl || "all",
  );

  // Sync status filter with URL param changes
  useEffect(() => {
    const statusFromUrl = searchParams.get("status");
    if (statusFromUrl && statusFromUrl !== statusFilter) {
      setStatusFilter(statusFromUrl);
    } else if (!statusFromUrl && statusFilter !== "all") {
      // If URL param is removed, reset to "all"
      setStatusFilter("all");
    }
  }, [searchParams]);

  // Use pagination hook for URL-based pagination state
  const pagination = useServerPagination({});

  // Build query params with search and status filter
  const queryParams = useMemo(
    () => ({
      ...pagination.queryParams,
      search: debouncedSearch || undefined,
      status: statusFilter !== "all" ? statusFilter : undefined,
      // Skip fetching if input file was deleted - there are no results to show
      enabled: !inputFileDeleted,
    }),
    [pagination.queryParams, debouncedSearch, statusFilter, inputFileDeleted],
  );

  // Fetch batch results with pagination and search
  const { data, isLoading } = useBatchResults(batchId, queryParams);

  // Parse JSONL into results
  const results: BatchResultItem[] = data?.content
    ? data.content
        .split("\n")
        .filter((line) => line.trim())
        .map((line) => JSON.parse(line))
    : [];

  const hasMore = data?.incomplete ?? false;

  // Handler to open content modal
  const handleViewContent = (
    result: BatchResultItem,
    contentType: "input" | "response",
  ) => {
    setSelectedResult(result);
    setModalContentType(contentType);
    setContentModalOpen(true);
  };

  const columns = createBatchResultsColumns(handleViewContent, batchStatus);

  return (
    <div>
      {/* Results Table */}
      <DataTable
        isLoading={isLoading}
        columns={columns}
        data={results}
        searchPlaceholder="Search by custom ID..."
        externalSearch={{
          value: searchInput,
          onChange: (value) => {
            setSearchInput(value);
            pagination.handleReset();
          },
        }}
        showColumnToggle={true}
        pageSize={pagination.pageSize}
        minRows={pagination.pageSize}
        rowHeight="40px"
        emptyState={
          <div className="text-center py-12">
            <div
              className={`p-4 rounded-full w-16 h-16 mx-auto mb-4 flex items-center justify-center ${inputFileDeleted ? "bg-amber-100" : "bg-doubleword-neutral-100"}`}
            >
              {inputFileDeleted ? (
                <AlertCircle className="w-8 h-8 text-amber-600" />
              ) : (
                <FileInput className="w-8 h-8 text-doubleword-neutral-600" />
              )}
            </div>
            <h3 className="text-lg font-medium text-doubleword-neutral-900 mb-2">
              {debouncedSearch || statusFilter !== "all"
                ? "No matching results"
                : inputFileDeleted
                  ? "Input file has been deleted"
                  : "No results yet"}
            </h3>
            <p className="text-doubleword-neutral-600">
              {debouncedSearch
                ? "No results match your search. Try a different custom ID."
                : statusFilter !== "all"
                  ? `No results with status "${statusFilter.replace("_", " ")}".`
                  : inputFileDeleted
                    ? "The original input file for this batch is no longer available."
                    : "Results will appear here as requests are processed."}
            </p>
          </div>
        }
        headerActions={
          <div className="flex items-center gap-4">
            <div className="flex items-center gap-2">
              <span className="text-sm text-gray-600">Status:</span>
              <Select
                value={statusFilter}
                onValueChange={(value) => {
                  setStatusFilter(value);
                  pagination.handleReset();
                  // Update URL param
                  const newParams = new URLSearchParams(searchParams);
                  if (value === "all") {
                    newParams.delete("status");
                  } else {
                    newParams.set("status", value);
                  }
                  setSearchParams(newParams, { replace: true });
                }}
              >
                <SelectTrigger className="w-32 h-8">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="all">All</SelectItem>
                  <SelectItem value="completed">Completed</SelectItem>
                  <SelectItem value="failed">Failed</SelectItem>
                  <SelectItem value="pending">Pending</SelectItem>
                  <SelectItem value="in_progress">In Progress</SelectItem>
                  <SelectItem value="cancelled">Cancelled</SelectItem>
                </SelectContent>
              </Select>
            </div>
            <div className="flex items-center gap-2">
              <span className="text-sm text-gray-600">Rows:</span>
              <Select
                value={pagination.pageSize.toString()}
                onValueChange={(value) => {
                  pagination.handlePageSizeChange(Number(value));
                }}
              >
                <SelectTrigger className="w-20 h-8">
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
          </div>
        }
      />
      <CursorPagination
        currentPage={pagination.page}
        itemsPerPage={pagination.pageSize}
        onNextPage={() => pagination.handlePageChange(pagination.page + 1)}
        onPrevPage={() =>
          pagination.handlePageChange(Math.max(1, pagination.page - 1))
        }
        onFirstPage={() => pagination.handlePageChange(1)}
        hasNextPage={hasMore}
        hasPrevPage={pagination.page > 1}
        currentPageItemCount={results.length}
        itemName="results"
      />

      {/* Content Modal */}
      <Dialog open={contentModalOpen} onOpenChange={setContentModalOpen}>
        <DialogContent className="sm:max-w-4xl max-h-[80vh] flex flex-col">
          <DialogHeader>
            <DialogTitle>
              {selectedResult &&
                (modalContentType === "input"
                  ? `Input: ${selectedResult.custom_id || selectedResult.id}`
                  : selectedResult.error
                    ? `Error: ${selectedResult.custom_id || selectedResult.id}`
                    : `Response: ${selectedResult.custom_id || selectedResult.id}`)}
            </DialogTitle>
            <DialogDescription>
              View the full{" "}
              {modalContentType === "input"
                ? "request body"
                : selectedResult?.error
                  ? "error details"
                  : "response"}{" "}
              content
            </DialogDescription>
          </DialogHeader>
          <div className="overflow-auto flex-1 min-h-0">
            {selectedResult &&
              (() => {
                let content: any;
                if (modalContentType === "input") {
                  content = selectedResult.input_body;
                } else if (selectedResult.error) {
                  content = { error: selectedResult.error };
                } else {
                  content = selectedResult.response_body;
                }

                return content ? (
                  <CodeBlock language="json">
                    {JSON.stringify(content, null, 2)}
                  </CodeBlock>
                ) : (
                  <p className="text-gray-500 text-sm p-4">
                    No content available
                  </p>
                );
              })()}
          </div>
        </DialogContent>
      </Dialog>
    </div>
  );
}
