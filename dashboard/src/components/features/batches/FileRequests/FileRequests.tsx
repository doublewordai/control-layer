import { useState, useEffect } from "react";
import { useParams, useNavigate, useSearchParams } from "react-router-dom";
import {
  ArrowLeft,
  FileInput,
  FileCheck,
  AlertCircle,
  Download,
  Loader2,
} from "lucide-react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
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
import { dwctlApi } from "../../../../api/control-layer/client";
import { useFile, useBatches } from "../../../../api/control-layer/hooks";
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

export function FileRequests() {
  const { fileId } = useParams<{ fileId: string }>();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const [searchParams, setSearchParams] = useSearchParams();
  const [downloadModalOpen, setDownloadModalOpen] = useState(false);

  // Get the tab to return to (default to "files")
  const returnTab = searchParams.get("returnTab") || "files";

  // Modal state for viewing request bodies - lifted to component level
  const [selectedRequest, setSelectedRequest] =
    useState<FileRequestOrResponse | null>(null);
  const [requestBodyModalOpen, setRequestBodyModalOpen] = useState(false);

  // Pagination state (1-based like Models)
  const [currentPage, setCurrentPage] = useState(
    Number(searchParams.get("page")) || 1,
  );
  const [pageSize, setPageSize] = useState(
    Number(searchParams.get("pageSize")) || 10,
  );

  // Load pagination from URL params on mount
  useEffect(() => {
    const page = searchParams.get("page");
    const size = searchParams.get("pageSize");

    if (page) setCurrentPage(Number(page));
    if (size) setPageSize(Number(size));
  }, [searchParams]);

  // Sync pagination to URL params
  useEffect(() => {
    const params = new URLSearchParams(searchParams);
    params.set("page", String(currentPage));
    params.set("pageSize", String(pageSize));
    // Preserve returnTab
    if (returnTab) params.set("returnTab", returnTab);

    setSearchParams(params, { replace: true });
  }, [currentPage, pageSize, returnTab, searchParams, setSearchParams]);

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

  // Fetch file content with pagination (convert 1-based page to 0-based offset)
  const { data, isLoading } = useQuery({
    queryKey: ["file-content", fileId, currentPage, pageSize],
    queryFn: () =>
      dwctlApi.files.getContent(fileId || "", {
        limit: pageSize,
        offset: (currentPage - 1) * pageSize,
      }),
    enabled: !!fileId,
  });

  // Prefetch next page
  useEffect(() => {
    if (fileId && data?.incomplete) {
      queryClient.prefetchQuery({
        queryKey: ["file-content", fileId, currentPage + 1, pageSize],
        queryFn: () =>
          dwctlApi.files.getContent(fileId, {
            limit: pageSize,
            offset: currentPage * pageSize,
          }),
      });
    }
  }, [fileId, currentPage, pageSize, data?.incomplete, queryClient]);

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
          onClick={() => navigate(`/batches?tab=${returnTab}`)}
          className="p-2 text-gray-500 hover:bg-gray-100 rounded-lg transition-colors shrink-0"
          aria-label="Back to Batches"
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

      {/* Requests Table */}
      {isLoading ? (
        <div className="flex items-center justify-center h-64">
          <div className="text-center">
            <div className="animate-spin rounded-full h-12 w-12 border-b-2 border-doubleword-accent-blue mx-auto"></div>
            <p className="mt-4 text-gray-600">Loading requests...</p>
          </div>
        </div>
      ) : requests.length === 0 ? (
        <div className="text-center py-12">
          <div className="p-4 bg-doubleword-neutral-100 rounded-full w-16 h-16 mx-auto mb-4 flex items-center justify-center">
            <FileInput className="w-8 h-8 text-doubleword-neutral-600" />
          </div>
          <h3 className="text-lg font-medium text-doubleword-neutral-900 mb-2">
            File is empty
          </h3>
          <p className="text-doubleword-neutral-600">
            This file does not contain any data
          </p>
        </div>
      ) : (
        <>
          <DataTable
            columns={columns}
            data={requests}
            searchPlaceholder="Search by custom ID..."
            showPagination={false}
            showColumnToggle={true}
            pageSize={pageSize}
            minRows={pageSize}
            rowHeight="40px"
            headerActions={
              <div className="flex items-center gap-2">
                <span className="text-sm text-gray-600">Rows:</span>
                <Select
                  value={pageSize.toString()}
                  onValueChange={(value) => {
                    setPageSize(Number(value));
                    setCurrentPage(1); // Reset to first page when changing page size
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
                    <SelectItem value="200">200</SelectItem>
                    <SelectItem value="500">500</SelectItem>
                  </SelectContent>
                </Select>
              </div>
            }
          />
          <CursorPagination
            currentPage={currentPage}
            itemsPerPage={pageSize}
            onNextPage={() => setCurrentPage(currentPage + 1)}
            onPrevPage={() => setCurrentPage(Math.max(1, currentPage - 1))}
            onFirstPage={() => setCurrentPage(1)}
            hasNextPage={hasMore}
            hasPrevPage={currentPage > 1}
            currentPageItemCount={requests.length}
            itemName="requests"
          />
        </>
      )}

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
