import { useState } from "react";
import { useParams, useNavigate, Link } from "react-router-dom";
import { ArrowLeft, Copy, Check, ChevronRight, RotateCcw } from "lucide-react";
import { useAsyncRequest, useRetryBatchRequests } from "../../../api/control-layer/hooks";
import { Button } from "../../ui/button";
import { toast } from "sonner";
import { useQueryClient } from "@tanstack/react-query";
import { Card, CardContent, CardHeader, CardTitle } from "../../ui/card";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "../../ui/collapsible";
import { CodeBlock } from "../../ui/code-block";
import { copyToClipboard } from "../../../utils";
import { formatTimestamp } from "../../../utils";


const getStatusColor = (status: string): string => {
  switch (status) {
    case "completed":
      return "bg-green-100 text-green-800";
    case "failed":
      return "bg-red-100 text-red-800";
    case "processing":
    case "claimed":
      return "bg-blue-100 text-blue-800";
    case "pending":
      return "bg-yellow-100 text-yellow-800";
    case "canceled":
      return "bg-gray-100 text-gray-800";
    default:
      return "bg-gray-100 text-gray-800";
  }
};

const statusLabels: Record<string, string> = {
  processing: "running",
  claimed: "running",
  pending: "queued",
  canceled: "cancelled",
};

function formatDuration(ms: number | null): string {
  if (!ms) return "-";
  const seconds = Math.round(ms / 1000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const remainingSeconds = seconds % 60;
  return `${minutes}m ${remainingSeconds}s`;
}

function formatCost(cost: number): string {
  if (cost < 0.0001) return `$${cost.toFixed(6)}`;
  if (cost < 0.01) return `$${cost.toFixed(4)}`;
  return `$${cost.toFixed(2)}`;
}

function prettyJson(raw: string): string {
  try {
    return JSON.stringify(JSON.parse(raw), null, 2);
  } catch {
    return raw;
  }
}


function CopyIconButton({ value }: { value: string }) {
  const [copied, setCopied] = useState(false);
  const handleCopy = async () => {
    const ok = await copyToClipboard(value);
    if (ok) {
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    }
  };
  return (
    <button
      onClick={handleCopy}
      className="shrink-0 p-1 text-gray-400 hover:text-gray-600 transition-colors"
      aria-label="Copy to clipboard"
    >
      {copied ? (
        <Check className="h-4 w-4 text-green-600" />
      ) : (
        <Copy className="h-4 w-4" />
      )}
    </button>
  );
}

export function AsyncRequestDetail() {
  const { requestId } = useParams<{ requestId: string }>();
  const navigate = useNavigate();
  const { data: request, isLoading } = useAsyncRequest(requestId);

  if (isLoading) {
    return (
      <div className="py-4 px-6">
        <div className="animate-pulse space-y-4">
          <div className="h-8 w-48 bg-doubleword-neutral-100 rounded" />
          <div className="h-64 bg-doubleword-neutral-100 rounded" />
        </div>
      </div>
    );
  }

  if (!request) {
    return (
      <div className="py-4 px-6">
        <p className="text-doubleword-neutral-600">Request not found.</p>
      </div>
    );
  }

  const status = request.status;
  const displayStatus = statusLabels[status] || status;
  const inputJson = prettyJson(request.body);
  const outputJson = request.response_body
    ? prettyJson(request.response_body)
    : null;

  return (
    <div className="py-4 px-6">
      {/* Header */}
      <div className="mb-4 flex flex-col sm:flex-row sm:items-end sm:justify-between gap-4">
        <div className="flex items-center gap-4">
          <button
            onClick={() => navigate("/async")}
            className="p-2 text-doubleword-neutral-600 hover:bg-doubleword-neutral-100 rounded-lg transition-colors shrink-0"
            aria-label="Back to Async"
          >
            <ArrowLeft className="w-5 h-5" />
          </button>
          <div>
            <h1 className="text-3xl font-bold text-doubleword-neutral-900">
              Request Detail
            </h1>
            <div className="flex items-center gap-2 mt-1">
              <span className="flex items-center gap-1 text-doubleword-neutral-600 font-mono text-sm">
                {request.id}
                <CopyIconButton value={request.id} />
              </span>
              <span
                className={`inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium ${getStatusColor(status)}`}
              >
                {displayStatus}
              </span>
            </div>
          </div>
        </div>
        {status === "failed" && request.batch_id && (
          <RetryButton batchId={request.batch_id} requestId={request.id} />
        )}
      </div>

      {/* Grid layout */}
      <div className="grid grid-cols-1 lg:grid-cols-3 gap-6">
        {/* Left column - Input/Output */}
        <div className="lg:col-span-2 space-y-6">
          {/* Input Card — collapsed by default */}
          <Collapsible defaultOpen={false}>
            <Card className="p-0 gap-0 rounded-lg">
              <CardHeader className="flex w-full justify-between px-6 pt-5 pb-4">
                <CollapsibleTrigger className="flex items-center gap-2 group cursor-pointer">
                  <ChevronRight className="w-4 h-4 text-gray-400 transition-transform group-data-[state=open]:rotate-90" />
                  <CardTitle>Input</CardTitle>
                </CollapsibleTrigger>
                <CopyIconButton value={inputJson} />
              </CardHeader>
              <CollapsibleContent>
                <CardContent className="px-6 pb-6 pt-0">
                  <div className="rounded-lg overflow-hidden border border-doubleword-border">
                    <CodeBlock language="json" variant="light">{inputJson}</CodeBlock>
                  </div>
                </CardContent>
              </CollapsibleContent>
            </Card>
          </Collapsible>

          {/* Output Card */}
          <Card className="p-0 gap-0 rounded-lg">
            <CardHeader className="flex w-full justify-between px-6 pt-5 pb-4">
              <CardTitle>Output</CardTitle>
              {outputJson && (
                <CopyIconButton value={outputJson} />
              )}
            </CardHeader>
            <CardContent className="px-6 pb-6 pt-0">
              {status === "completed" && outputJson ? (
                <div className="rounded-lg overflow-hidden border border-doubleword-border">
                  <CodeBlock language="json" variant="light">{outputJson}</CodeBlock>
                </div>
              ) : status === "failed" ? (
                <div className="rounded-lg border border-red-200 bg-red-50 p-4">
                  <p className="text-sm text-red-700">
                    {request.error || "Request failed"}
                  </p>
                </div>
              ) : (
                <p className="text-sm text-doubleword-neutral-600">
                  Waiting for response...
                </p>
              )}
            </CardContent>
          </Card>
        </div>

        {/* Right column - Timeline, Metrics, Metadata */}
        <div className="space-y-6">
          {/* Timeline Card */}
          <Card className="p-0 gap-0 rounded-lg">
            <CardHeader className="px-6 pt-5 pb-4">
              <CardTitle>Timeline</CardTitle>
            </CardHeader>
            <CardContent className="px-6 pb-6 pt-0">
              <div className="space-y-3">
                <div>
                  <p className="text-sm text-gray-600 mb-1">Created</p>
                  <p className="text-sm font-medium">
                    {formatTimestamp(request.created_at)}
                  </p>
                </div>
                {request.completed_at && (
                  <div>
                    <p className="text-sm text-gray-600 mb-1">Completed</p>
                    <p className="text-sm font-medium">
                      {formatTimestamp(request.completed_at)}
                    </p>
                  </div>
                )}
                {request.failed_at && (
                  <div>
                    <p className="text-sm text-gray-600 mb-1">Failed</p>
                    <p className="text-sm font-medium">
                      {formatTimestamp(request.failed_at)}
                    </p>
                  </div>
                )}
                {request.duration_ms != null && (
                  <div className="border-t border-doubleword-border-light pt-3">
                    <p className="text-sm text-gray-600 mb-1">Duration</p>
                    <p className="text-sm font-medium">
                      {formatDuration(request.duration_ms)}
                    </p>
                  </div>
                )}
              </div>
            </CardContent>
          </Card>

          {/* Metrics Card */}
          {(request.total_tokens != null || request.total_cost != null) && (
            <Card className="p-0 gap-0 rounded-lg">
              <CardHeader className="px-6 pt-5 pb-4">
                <CardTitle>Metrics</CardTitle>
              </CardHeader>
              <CardContent className="px-6 pb-6 pt-0">
                <div className="space-y-2">
                  {request.prompt_tokens != null && (
                    <div className="flex justify-between text-sm">
                      <span className="text-gray-600">Prompt</span>
                      <span className="font-medium tabular-nums">
                        {request.prompt_tokens.toLocaleString()}
                      </span>
                    </div>
                  )}
                  {request.completion_tokens != null && (
                    <div className="flex justify-between text-sm">
                      <span className="text-gray-600">Completion</span>
                      <span className="font-medium tabular-nums">
                        {request.completion_tokens.toLocaleString()}
                      </span>
                    </div>
                  )}
                  {request.reasoning_tokens != null &&
                    request.reasoning_tokens > 0 && (
                      <div className="flex justify-between text-sm">
                        <span className="text-gray-600">Reasoning</span>
                        <span className="font-medium tabular-nums">
                          {request.reasoning_tokens.toLocaleString()}
                        </span>
                      </div>
                    )}
                  {request.total_tokens != null && (
                    <div className="flex justify-between text-sm font-medium border-t border-doubleword-border-light pt-2 mt-2">
                      <span>Total tokens</span>
                      <span className="tabular-nums">
                        {request.total_tokens.toLocaleString()}
                      </span>
                    </div>
                  )}
                  {request.total_cost != null && request.total_cost > 0 && (
                    <div className="flex justify-between text-sm border-t border-doubleword-border-light pt-2 mt-2">
                      <span className="text-gray-600">Cost</span>
                      <span className="font-medium text-green-700">
                        {formatCost(request.total_cost)}
                      </span>
                    </div>
                  )}
                </div>
              </CardContent>
            </Card>
          )}

          {/* Metadata Card */}
          <Card className="p-0 gap-0 rounded-lg">
            <CardHeader className="px-6 pt-5 pb-4">
              <CardTitle>Request Information</CardTitle>
            </CardHeader>
            <CardContent className="px-6 pb-6 pt-0">
              <div className="space-y-4">
                <div>
                  <p className="text-sm text-gray-600 mb-1">Model</p>
                  <p className="font-medium text-sm">{request.model}</p>
                </div>
                {request.created_by_email && (
                  <div>
                    <p className="text-sm text-gray-600 mb-1">Created by</p>
                    <p className="font-medium text-sm wrap-break-word">
                      {request.created_by_email}
                    </p>
                  </div>
                )}
                <div className="border-t border-doubleword-border-light pt-4">
                  <Link
                    to={`/batches/${request.batch_id}`}
                    className="text-sm text-doubleword-neutral-600 hover:text-doubleword-neutral-900 hover:underline"
                  >
                    View related batch →
                  </Link>
                </div>
              </div>
            </CardContent>
          </Card>
        </div>
      </div>
    </div>
  );
}

function RetryButton({ batchId, requestId }: { batchId: string; requestId: string }) {
  const retryMutation = useRetryBatchRequests();
  const queryClient = useQueryClient();

  const handleRetry = async () => {
    try {
      await retryMutation.mutateAsync({ batchId, requestIds: [requestId] });
      queryClient.invalidateQueries({ queryKey: ["asyncRequests"] });
      queryClient.invalidateQueries({ queryKey: ["asyncRequests", "detail", requestId] });
      toast.success("Request queued for retry");
    } catch {
      toast.error("Failed to retry request");
    }
  };

  return (
    <Button
      variant="outline"
      onClick={handleRetry}
      disabled={retryMutation.isPending}
    >
      <RotateCcw className={`h-4 w-4 mr-2 ${retryMutation.isPending ? "animate-spin" : ""}`} />
      {retryMutation.isPending ? "Retrying..." : "Retry"}
    </Button>
  );
}
