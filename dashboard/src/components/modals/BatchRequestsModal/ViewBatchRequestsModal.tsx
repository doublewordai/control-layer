import { useState } from "react";
import {
  X,
  ChevronDown,
  ChevronRight,
  CheckCircle2,
  XCircle,
  Clock,
  Loader2,
  AlertCircle,
} from "lucide-react";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "../../ui/dialog";
import { Button } from "../../ui/button";
import { ScrollArea } from "../../ui/scroll-area";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../ui/select";
import { useBatchRequests } from "../../../api/control-layer/hooks";
import { formatTimestamp, formatDuration } from "../../../utils";
import type {
  Batch,
  BatchRequest,
  RequestStatus,
} from "../../../api/control-layer/types";

interface ViewBatchRequestsModalProps {
  isOpen: boolean;
  onClose: () => void;
  batch: Batch | null;
}

const getStatusIcon = (status: RequestStatus) => {
  switch (status) {
    case "completed":
      return <CheckCircle2 className="w-4 h-4 text-green-600" />;
    case "failed":
      return <XCircle className="w-4 h-4 text-red-600" />;
    case "cancelled":
      return <XCircle className="w-4 h-4 text-gray-600" />;
    case "in_progress":
      return <Loader2 className="w-4 h-4 text-blue-600 animate-spin" />;
    case "pending":
      return <Clock className="w-4 h-4 text-yellow-600" />;
    default:
      return <Clock className="w-4 h-4 text-gray-600" />;
  }
};

const getStatusColor = (status: RequestStatus) => {
  switch (status) {
    case "completed":
      return "bg-green-100 text-green-800";
    case "failed":
      return "bg-red-100 text-red-800";
    case "cancelled":
      return "bg-gray-100 text-gray-800";
    case "in_progress":
      return "bg-blue-100 text-blue-800";
    case "pending":
      return "bg-yellow-100 text-yellow-800";
    default:
      return "bg-gray-100 text-gray-800";
  }
};

function RequestCard({ request }: { request: BatchRequest }) {
  const [expanded, setExpanded] = useState(false);

  const duration =
    request.completed_at && request.started_at
      ? (request.completed_at - request.started_at) * 1000
      : null;

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
            <p className="text-xs text-gray-500 font-mono">{request.id}</p>
          </div>
        </div>
        <div className="flex items-center gap-3">
          {duration && (
            <span className="text-xs text-gray-600">
              {formatDuration(duration)}
            </span>
          )}
          {request.usage && (
            <span className="text-xs text-gray-600">
              {request.usage.total_tokens} tokens
            </span>
          )}
          <div className="flex items-center gap-1">
            {getStatusIcon(request.status)}
            <span
              className={`inline-flex items-center px-2 py-0.5 rounded-full text-xs font-medium ${getStatusColor(request.status)}`}
            >
              {request.status}
            </span>
          </div>
        </div>
      </button>

      {expanded && (
        <div className="p-4 bg-white space-y-4">
          {/* Metadata */}
          <div className="grid grid-cols-2 gap-4 text-sm">
            <div>
              <span className="text-gray-600">Created:</span>{" "}
              <span className="text-gray-900">
                {formatTimestamp(new Date(request.created_at * 1000).toISOString())}
              </span>
            </div>
            {request.started_at && (
              <div>
                <span className="text-gray-600">Started:</span>{" "}
                <span className="text-gray-900">
                  {formatTimestamp(new Date(request.started_at * 1000).toISOString())}
                </span>
              </div>
            )}
            {request.completed_at && (
              <div>
                <span className="text-gray-600">Completed:</span>{" "}
                <span className="text-gray-900">
                  {formatTimestamp(new Date(request.completed_at * 1000).toISOString())}
                </span>
              </div>
            )}
            {request.usage && (
              <>
                <div>
                  <span className="text-gray-600">Prompt Tokens:</span>{" "}
                  <span className="text-gray-900">{request.usage.prompt_tokens}</span>
                </div>
                <div>
                  <span className="text-gray-600">Completion Tokens:</span>{" "}
                  <span className="text-gray-900">
                    {request.usage.completion_tokens}
                  </span>
                </div>
              </>
            )}
          </div>

          {/* Request */}
          <div>
            <h4 className="text-xs font-semibold text-gray-700 mb-2">
              Request
            </h4>
            <pre className="text-xs bg-gray-50 p-3 rounded border border-gray-200 overflow-x-auto">
              {JSON.stringify(request.request, null, 2)}
            </pre>
          </div>

          {/* Response or Error */}
          {request.response && (
            <div>
              <h4 className="text-xs font-semibold text-gray-700 mb-2">
                Response
              </h4>
              <pre className="text-xs bg-gray-50 p-3 rounded border border-gray-200 overflow-x-auto">
                {JSON.stringify(request.response, null, 2)}
              </pre>
            </div>
          )}

          {request.error && (
            <div className="bg-red-50 border border-red-200 rounded-lg p-3">
              <div className="flex gap-2">
                <AlertCircle className="w-4 h-4 text-red-600 mt-0.5 flex-shrink-0" />
                <div>
                  <p className="text-sm font-medium text-red-900">
                    {request.error.code}
                  </p>
                  <p className="text-sm text-red-700">{request.error.message}</p>
                </div>
              </div>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

export function ViewBatchRequestsModal({
  isOpen,
  onClose,
  batch,
}: ViewBatchRequestsModalProps) {
  const [page, setPage] = useState(0);
  const [statusFilter, setStatusFilter] = useState<RequestStatus | "all">("all");
  const pageSize = 50;

  const { data, isLoading } = useBatchRequests(
    batch?.id || "",
    {
      limit: pageSize,
      skip: page * pageSize,
      status: statusFilter !== "all" ? statusFilter : undefined,
    },
  );

  const requests = data?.data || [];
  const hasMore = data?.has_more || false;
  const total = data?.total || 0;

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-4xl max-h-[80vh] flex flex-col">
        <DialogHeader>
          <DialogTitle>Batch Requests: {batch?.id || ""}</DialogTitle>
        </DialogHeader>

        {batch && (
          <div className="space-y-3">
            {/* Batch Info */}
            <div className="flex items-center justify-between py-2 px-4 bg-gray-50 rounded-lg">
              <div className="flex gap-6 text-sm">
                <span className="text-gray-600">
                  <span className="font-medium text-gray-900">File:</span>{" "}
                  <span className="font-mono text-xs">{batch.input_file_id}</span>
                </span>
                <span className="text-gray-600">
                  <span className="font-medium text-gray-900">Endpoint:</span>{" "}
                  <span className="font-mono text-xs">{batch.endpoint}</span>
                </span>
              </div>
            </div>

            {/* Progress Bar */}
            <div className="space-y-1">
              <div className="flex justify-between text-sm">
                <span className="text-gray-600">
                  Progress: {batch.request_counts.completed + batch.request_counts.failed} /{" "}
                  {batch.request_counts.total}
                </span>
                <span className="text-gray-600">
                  {Math.round(
                    ((batch.request_counts.completed + batch.request_counts.failed) /
                      batch.request_counts.total) *
                      100,
                  )}
                  %
                </span>
              </div>
              <div className="w-full bg-gray-200 rounded-full h-2">
                <div
                  className="bg-blue-600 h-2 rounded-full transition-all"
                  style={{
                    width: `${((batch.request_counts.completed + batch.request_counts.failed) / batch.request_counts.total) * 100}%`,
                  }}
                />
              </div>
              <div className="flex gap-4 text-xs text-gray-600">
                <span className="text-green-600">
                  ✓ {batch.request_counts.completed} completed
                </span>
                {batch.request_counts.failed > 0 && (
                  <span className="text-red-600">
                    ✗ {batch.request_counts.failed} failed
                  </span>
                )}
              </div>
            </div>

            {/* Filter */}
            <div className="flex items-center gap-2">
              <span className="text-sm text-gray-600">Filter by status:</span>
              <Select
                value={statusFilter}
                onValueChange={(value) => {
                  setStatusFilter(value as RequestStatus | "all");
                  setPage(0);
                }}
              >
                <SelectTrigger className="w-40">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="all">All</SelectItem>
                  <SelectItem value="pending">Pending</SelectItem>
                  <SelectItem value="in_progress">In Progress</SelectItem>
                  <SelectItem value="completed">Completed</SelectItem>
                  <SelectItem value="failed">Failed</SelectItem>
                  <SelectItem value="cancelled">Cancelled</SelectItem>
                </SelectContent>
              </Select>
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
              <Clock className="w-12 h-12 mx-auto mb-3 text-gray-400" />
              <p>No requests found</p>
            </div>
          ) : (
            <div className="space-y-2">
              {requests.map((request) => (
                <RequestCard key={request.id} request={request} />
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