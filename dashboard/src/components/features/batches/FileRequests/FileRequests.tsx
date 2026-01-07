import { useState, useMemo, useEffect } from "react";
import { useParams, useNavigate, useSearchParams } from "react-router-dom";
import {
  ArrowLeft,
  FileInput,
  FileCheck,
  AlertCircle,
  Download,
} from "lucide-react";
import { useDebounce } from "../../../../hooks/useDebounce";
import { Button } from "../../../ui/button";
import { DataTable } from "../../../ui/data-table";
import { CursorPagination } from "../../../ui/cursor-pagination";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../../ui/select";
import {
  useFile,
  useBatches,
  useFileContent,
} from "../../../../api/control-layer/hooks";
import {
  createFileRequestsColumns,
  type FileRequestOrResponse,
} from "./columns";
import { DownloadFileModal } from "../../../modals/DownloadFileModal/DownloadFileModal";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
} from "../../../ui/dialog";
import { CodeBlock } from "../../../ui/code-block";
import type { FileRequest } from "../../../../api/control-layer/types";
import { useServerPagination } from "../../../../hooks/useServerPagination";

export function FileRequests() {
  const { fileId } = useParams<{ fileId: string }>();
  const navigate = useNavigate();
  const [searchParams] = useSearchParams();
  const [downloadModalOpen, setDownloadModalOpen] = useState(false);

  // Get the URL to return to - could be batch details page or batches list
  const fromUrl = searchParams.get("from");
  // Legacy support for returnTab parameter
  const returnTab = searchParams.get("returnTab") || "files";

  // Modal state for viewing request bodies - lifted to component level
  const [selectedRequest, setSelectedRequest] =
    useState<FileRequestOrResponse | null>(null);
  const [requestBodyModalOpen, setRequestBodyModalOpen] = useState(false);

  // Search state with debounce for server-side filtering
  const [searchInput, setSearchInput] = useState("");
  const debouncedSearch = useDebounce(searchInput, 300);

  // Use pagination hook for URL-based pagination state
  const pagination = useServerPagination({ defaultPageSize: 10 });

  // Reset pagination when debounced search changes
  useEffect(() => {
    pagination.handleReset();
    /* eslint-disable-next-line */
  }, [debouncedSearch]);

  // Get file details - works for input, output, or error files
  const { data: file } = useFile(fileId || "");

  // Get all batches to check if this file's batch is in progress
  const { data: batchesResponse } = useBatches();
  const batches = batchesResponse?.data || [];

  // Check if this file is from a batch that's still in progress
  const isPartial =
    file &&
    (file.purpose === "batch_output" || file.purpose === "batch_error") &&
    batches.some(
      (b) =>
        (b.output_file_id === file.id || b.error_file_id === file.id) &&
        ["validating", "in_progress", "finalizing"].includes(b.status),
    );

  // Build query params with search
  const queryParams = useMemo(
    () => ({
      ...pagination.queryParams,
      search: debouncedSearch || undefined,
    }),
    [pagination.queryParams, debouncedSearch],
  );

  // Fetch file content with pagination and search using custom hook
  const { data, isLoading } = useFileContent(fileId || "", queryParams);

  // Parse JSONL into requests (could be templates or responses)
  const requests: FileRequestOrResponse[] = data?.content
    ? data.content
        .split("\n")
        .filter((line) => line.trim())
        .map((line) => JSON.parse(line))
    : [];

  // Detect if this is an output/error file (has response/error fields) or input file (has method/url/body)
  const isOutputFile =
    requests.length > 0 &&
    ("response" in requests[0] || "error" in requests[0]);

  const hasMore = data?.incomplete ?? false;

  // Handler to open request body modal
  const handleViewRequestBody = (request: FileRequestOrResponse) => {
    setSelectedRequest(request);
    setRequestBodyModalOpen(true);
  };

  const columns = createFileRequestsColumns(
    isOutputFile,
    handleViewRequestBody,
  );

  return (
    <div className="py-4 px-6">
      {/* Compact Header with Back Button */}
      <div className="mb-6 flex items-center gap-4">
        <button
          onClick={() => navigate(fromUrl || `/batches?tab=${returnTab}`)}
          className="p-2 text-gray-500 hover:bg-gray-100 rounded-lg transition-colors shrink-0"
          aria-label={fromUrl ? "Go Back" : "Back to Batches"}
          title="Back to Batches"
        >
          <ArrowLeft className="w-5 h-5" />
        </button>
        <div className="min-w-0 flex-1">
          <h1 className="text-2xl font-bold text-doubleword-neutral-900">
            File Content
          </h1>
          {file && (
            <div className="mt-1 flex items-center gap-4 text-sm text-gray-600">
              <span className="truncate flex items-center gap-2">
                <span className="font-medium">File:</span>
                {file.purpose === "batch" && (
                  <FileInput className="w-4 h-4 text-gray-500 shrink-0" />
                )}
                {file.purpose === "batch_output" && (
                  <FileCheck className="w-4 h-4 text-green-600 shrink-0" />
                )}
                {file.purpose === "batch_error" && (
                  <AlertCircle className="w-4 h-4 text-red-500 shrink-0" />
                )}
                {file.filename}
              </span>
            </div>
          )}
        </div>
        <div className="flex items-center gap-2">
          {file && (
            <Button
              variant="outline"
              onClick={() => setDownloadModalOpen(true)}
              className="flex items-center gap-2"
            >
              <Download className="w-4 h-4" />
              Download
            </Button>
          )}
        </div>
      </div>

      {/* Requests Table */}
      <DataTable
        isLoading={isLoading}
        columns={columns}
        data={requests}
        searchPlaceholder="Search by custom ID..."
        externalSearch={{
          value: searchInput,
          onChange: setSearchInput,
        }}
        showColumnToggle={true}
        pageSize={pagination.pageSize}
        minRows={pagination.pageSize}
        rowHeight="40px"
        emptyState={
          <div className="text-center py-12">
            <div className="p-4 bg-doubleword-neutral-100 rounded-full w-16 h-16 mx-auto mb-4 flex items-center justify-center">
              <FileInput className="w-8 h-8 text-doubleword-neutral-600" />
            </div>
            <h3 className="text-lg font-medium text-doubleword-neutral-900 mb-2">
              {debouncedSearch ? "No matching requests" : "File is empty"}
            </h3>
            <p className="text-doubleword-neutral-600">
              {debouncedSearch
                ? "No requests match your search. Try a different custom ID."
                : "This file does not contain any data"}
            </p>
          </div>
        }
        headerActions={
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
                <SelectItem value="25">25</SelectItem>
                <SelectItem value="50">50</SelectItem>
                <SelectItem value="100">100</SelectItem>
              </SelectContent>
            </Select>
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
        currentPageItemCount={requests.length}
        itemName="requests"
      />

      {/* Download Modal */}
      {file && (
        <DownloadFileModal
          isOpen={downloadModalOpen}
          onClose={() => setDownloadModalOpen(false)}
          title={`Download ${file.filename}`}
          description="Choose how you'd like to download this file"
          resourceType="file"
          resourceId={file.id}
          filename={file.filename}
          isPartial={isPartial}
        />
      )}

      {/* Request Body Modal */}
      <Dialog
        open={requestBodyModalOpen}
        onOpenChange={setRequestBodyModalOpen}
      >
        <DialogContent className="sm:max-w-4xl max-h-[80vh] flex flex-col">
          <DialogHeader>
            <DialogTitle>
              {selectedRequest &&
                (() => {
                  if (isOutputFile) {
                    const outputRequest = selectedRequest as {
                      response?: any;
                      error?: any;
                      custom_id: string;
                    };
                    if (outputRequest.error) {
                      return `Error: ${selectedRequest.custom_id}`;
                    } else if (outputRequest.response) {
                      return `Response: ${selectedRequest.custom_id}`;
                    }
                    return selectedRequest.custom_id;
                  } else {
                    return `Request Body: ${selectedRequest.custom_id}`;
                  }
                })()}
            </DialogTitle>
            <DialogDescription>
              View the full{" "}
              {isOutputFile ? "response or error" : "request body"} content
            </DialogDescription>
          </DialogHeader>
          <div className="overflow-auto flex-1 min-h-0">
            {selectedRequest &&
              (() => {
                let content: any;
                if (isOutputFile) {
                  const outputRequest = selectedRequest as {
                    response?: any;
                    error?: any;
                  };
                  if (outputRequest.error) {
                    content = outputRequest.error;
                  } else if (outputRequest.response) {
                    content =
                      outputRequest.response.body || outputRequest.response;
                  } else {
                    content = null;
                  }
                } else {
                  const inputRequest = selectedRequest as FileRequest;
                  content = inputRequest.body;
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
