import { useState } from "react";
import { X, FileText, ChevronDown, ChevronRight } from "lucide-react";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "../../ui/dialog";
import { Button } from "../../ui/button";
import { ScrollArea } from "../../ui/scroll-area";
import { useFileRequests } from "../../../api/control-layer/hooks";
import type { FileObject, FileRequest } from "../../../api/control-layer/types";

interface ViewFileRequestsModalProps {
  isOpen: boolean;
  onClose: () => void;
  file: FileObject | null;
}

function RequestCard({ request }: { request: FileRequest }) {
  const [expanded, setExpanded] = useState(false);

  return (
    <div className="border border-gray-200 rounded-lg overflow-hidden">
      <button
        onClick={() => setExpanded(!expanded)}
        className="w-full px-4 py-3 bg-gray-50 hover:bg-gray-100 transition-colors flex items-center justify-between"
      >
        <div className="flex items-center gap-3">
          {expanded ? (
            <ChevronDown className="w-4 h-4 text-gray-500" />
          ) : (
            <ChevronRight className="w-4 h-4 text-gray-500" />
          )}
          <div className="text-left">
            <p className="font-medium text-sm text-gray-900">
              {request.custom_id}
            </p>
            <p className="text-xs text-gray-500">
              {request.method} {request.url}
            </p>
          </div>
        </div>
        <span className="text-xs font-mono text-gray-600 px-2 py-1 bg-white rounded">
          {request.method}
        </span>
      </button>

      {expanded && (
        <div className="p-4 bg-white space-y-3">
          {/* Request Body */}
          <div>
            <h4 className="text-xs font-semibold text-gray-700 mb-2">
              Request Body
            </h4>
            <pre className="text-xs bg-gray-50 p-3 rounded border border-gray-200 overflow-x-auto">
              {JSON.stringify(request.body, null, 2)}
            </pre>
          </div>
        </div>
      )}
    </div>
  );
}

export function ViewFileRequestsModal({
  isOpen,
  onClose,
  file,
}: ViewFileRequestsModalProps) {
  const [page, setPage] = useState(0);
  const pageSize = 50;

  const { data, isLoading } = useFileRequests(
    file?.id || "",
    {
      limit: pageSize,
      skip: page * pageSize,
    },
  );

  const requests = data?.data || [];
  const hasMore = data?.has_more || false;
  const total = data?.total || 0;

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-3xl max-h-[80vh] flex flex-col">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <FileText className="w-5 h-5" />
            File Requests: {file?.filename || ""}
          </DialogTitle>
        </DialogHeader>

        {file && (
          <div className="flex items-center justify-between py-2 px-4 bg-gray-50 rounded-lg">
            <div className="flex gap-6 text-sm">
              <span className="text-gray-600">
                <span className="font-medium text-gray-900">File ID:</span>{" "}
                <span className="font-mono text-xs">{file.id}</span>
              </span>
              <span className="text-gray-600">
                <span className="font-medium text-gray-900">Size:</span>{" "}
                {(file.bytes / 1024).toFixed(1)} KB
              </span>
              <span className="text-gray-600">
                <span className="font-medium text-gray-900">Total Requests:</span>{" "}
                {total}
              </span>
            </div>
          </div>
        )}

        <ScrollArea className="flex-1 pr-4">
          {isLoading ? (
            <div className="flex items-center justify-center py-12">
              <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-blue-600"></div>
            </div>
          ) : requests.length === 0 ? (
            <div className="text-center py-12 text-gray-500">
              <FileText className="w-12 h-12 mx-auto mb-3 text-gray-400" />
              <p>No requests found in this file</p>
            </div>
          ) : (
            <div className="space-y-2">
              {requests.map((request, index) => (
                <RequestCard
                  key={`${request.custom_id}-${index}`}
                  request={request}
                />
              ))}
            </div>
          )}
        </ScrollArea>

        {/* Pagination */}
        {total > pageSize && (
          <div className="flex items-center justify-between pt-4 border-t">
            <p className="text-sm text-gray-600">
              Showing {page * pageSize + 1} -{" "}
              {Math.min((page + 1) * pageSize, total)} of {total} requests
            </p>
            <div className="flex gap-2">
              <Button
                variant="outline"
                size="sm"
                onClick={() => setPage(page - 1)}
                disabled={page === 0}
              >
                Previous
              </Button>
              <Button
                variant="outline"
                size="sm"
                onClick={() => setPage(page + 1)}
                disabled={!hasMore}
              >
                Next
              </Button>
            </div>
          </div>
        )}

        <div className="flex justify-end pt-4 border-t">
          <Button variant="outline" onClick={onClose}>
            <X className="w-4 h-4 mr-2" />
            Close
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  );
}