import { useState, useEffect } from "react";
import { useParams, useNavigate, useSearchParams } from "react-router-dom";
import { ArrowLeft, FileText, Download, Loader2 } from "lucide-react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Button } from "../../../ui/button";
import { DataTable } from "../../../ui/data-table";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../../ui/select";
import { dwctlApi } from "../../../../api/control-layer/client";
import { useFile, useBatches } from "../../../../api/control-layer/hooks";
import { createFileRequestsColumns } from "./columns";
import { DownloadFileModal } from "../../../modals/DownloadFileModal/DownloadFileModal";
import type { FileRequest } from "../../../../api/control-layer/types";

// Extended type to handle both templates and responses
type FileRequestOrResponse =
  | FileRequest
  | {
      id?: string;
      custom_id: string;
      response?: { status_code: number; body: any } | null;
      error?: { code: string; message: string } | null;
    };

export function FileRequests() {
  const { fileId } = useParams<{ fileId: string }>();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const [searchParams, setSearchParams] = useSearchParams();
  const [downloadModalOpen, setDownloadModalOpen] = useState(false);

  // Get pagination from URL or use defaults
  const page = parseInt(searchParams.get("page") || "0", 10);
  const pageSize = parseInt(searchParams.get("pageSize") || "10", 10);

  // Update URL when pagination changes
  const updatePagination = (newPage: number, newPageSize: number) => {
    const params = new URLSearchParams(searchParams);
    params.set("page", newPage.toString());
    params.set("pageSize", newPageSize.toString());
    setSearchParams(params, { replace: true });
  };

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

  // Fetch file content with pagination
  const { data, isLoading } = useQuery({
    queryKey: ["file-content", fileId, page, pageSize],
    queryFn: () =>
      dwctlApi.files.getContent(fileId || "", {
        limit: pageSize,
        offset: page * pageSize,
      }),
    enabled: !!fileId,
  });

  // Prefetch next page
  useEffect(() => {
    if (fileId && data?.incomplete) {
      queryClient.prefetchQuery({
        queryKey: ["file-content", fileId, page + 1, pageSize],
        queryFn: () =>
          dwctlApi.files.getContent(fileId, {
            limit: pageSize,
            offset: (page + 1) * pageSize,
          }),
      });
    }
  }, [fileId, page, pageSize, data?.incomplete, queryClient]);

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

  const columns = createFileRequestsColumns(isOutputFile);

  return (
    <div className="py-4 px-6">
      {/* Compact Header with Back Button */}
      <div className="mb-6 flex items-center gap-4">
        <button
          onClick={() => navigate("/batches")}
          className="p-2 text-gray-500 hover:bg-gray-100 rounded-lg transition-colors flex-shrink-0"
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
              <span className="truncate">
                <span className="font-medium">File:</span> {file.filename}
              </span>
              <span className="flex-shrink-0">
                <span className="font-medium">Showing:</span>{" "}
                {page * pageSize + 1}-{page * pageSize + requests.length}
                {isPartial && (
                  <span className="inline-flex items-center gap-1 text-blue-600 ml-2">
                    <Loader2 className="w-3 h-3 animate-spin" />
                    Partial results (batch in progress)
                  </span>
                )}
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
            <div className="animate-spin rounded-full h-12 w-12 border-b-2 border-blue-600 mx-auto"></div>
            <p className="mt-4 text-gray-600">Loading requests...</p>
          </div>
        </div>
      ) : requests.length === 0 ? (
        <div className="text-center py-12">
          <div className="p-4 bg-doubleword-neutral-100 rounded-full w-16 h-16 mx-auto mb-4 flex items-center justify-center">
            <FileText className="w-8 h-8 text-doubleword-neutral-600" />
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
                    updatePagination(0, Number(value)); // Reset to first page when changing page size
                  }}
                >
                  <SelectTrigger className="w-[80px] h-8">
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
          <div className="flex items-center justify-between px-2 py-4">
            <div className="text-sm text-gray-700">
              Showing {page * pageSize + 1} -{" "}
              {page * pageSize + requests.length}
              {hasMore && " of many"}
            </div>
            <div className="flex items-center gap-2">
              <Button
                variant="outline"
                size="sm"
                onClick={() =>
                  updatePagination(Math.max(0, page - 1), pageSize)
                }
                disabled={page === 0}
              >
                Previous
              </Button>
              <span className="text-sm text-gray-700">Page {page + 1}</span>
              <Button
                variant="outline"
                size="sm"
                onClick={() => updatePagination(page + 1, pageSize)}
                disabled={!hasMore}
              >
                Next
              </Button>
            </div>
          </div>
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
    </div>
  );
}
