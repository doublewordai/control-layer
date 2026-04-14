import { useParams, useNavigate, Link } from "react-router-dom";
import { ArrowLeft, Clock } from "lucide-react";
import { useAsyncRequest } from "../../../api/control-layer/hooks";
import { Card, CardContent, CardHeader, CardTitle } from "../../ui/card";
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

interface ParsedMessage {
  role: string;
  content: string;
}

function parseRequestBody(body: string): ParsedMessage[] {
  try {
    const parsed = JSON.parse(body);
    if (parsed.body?.messages) return parsed.body.messages;
    if (parsed.messages) return parsed.messages;
    return [{ role: "user", content: body }];
  } catch {
    return [{ role: "user", content: body }];
  }
}

function parseResponseBody(responseBody: string | null): string | null {
  if (!responseBody) return null;
  try {
    const parsed = JSON.parse(responseBody);
    const message = parsed.body?.choices?.[0]?.message?.content;
    if (message) return message;
    const directMessage = parsed.choices?.[0]?.message?.content;
    if (directMessage) return directMessage;
    return responseBody;
  } catch {
    return responseBody;
  }
}

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

const roleColors: Record<string, string> = {
  system: "text-doubleword-purple",
  user: "text-blue-700",
  assistant: "text-green-700",
};

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

  const messages = parseRequestBody(request.body);
  const responseContent = parseResponseBody(request.response_body);
  const status = request.status;
  const displayStatus = statusLabels[status] || status;

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
              <p className="text-doubleword-neutral-600 font-mono text-sm">
                {request.id}
              </p>
              <span
                className={`inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium ${getStatusColor(status)}`}
              >
                {displayStatus}
              </span>
            </div>
          </div>
        </div>
      </div>

      {/* Grid layout matching batch details */}
      <div className="grid grid-cols-1 lg:grid-cols-3 gap-6">
        {/* Left column - Input/Output */}
        <div className="lg:col-span-2 space-y-6">
          {/* Input Card */}
          <Card className="p-0 gap-0 rounded-lg">
            <CardHeader className="px-6 pt-5 pb-4">
              <CardTitle>Input</CardTitle>
            </CardHeader>
            <CardContent className="px-6 pb-6 pt-0">
              <div className="space-y-3">
                {messages.map((msg, i) => (
                  <div key={i}>
                    <span
                      className={`text-[10px] uppercase tracking-wide font-medium block mb-1 ${roleColors[msg.role] || "text-doubleword-neutral-600"}`}
                    >
                      {msg.role}
                    </span>
                    <p className="text-sm leading-relaxed whitespace-pre-wrap text-doubleword-text-primary">
                      {msg.content}
                    </p>
                  </div>
                ))}
              </div>
            </CardContent>
          </Card>

          {/* Output Card */}
          <Card className="p-0 gap-0 rounded-lg">
            <CardHeader className="px-6 pt-5 pb-4">
              <CardTitle>Output</CardTitle>
            </CardHeader>
            <CardContent className="px-6 pb-6 pt-0">
              {status === "completed" && responseContent ? (
                <div>
                  <span className="text-[10px] uppercase tracking-wide font-medium text-green-700 block mb-1">
                    Assistant
                  </span>
                  <p className="text-sm leading-relaxed whitespace-pre-wrap text-doubleword-text-primary">
                    {responseContent}
                  </p>
                </div>
              ) : status === "failed" ? (
                <div>
                  <span className="text-[10px] uppercase tracking-wide font-medium text-red-700 block mb-1">
                    Error
                  </span>
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
              <div className="space-y-4">
                <div className="grid grid-cols-2 gap-4">
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
                </div>
                {request.duration_ms != null && (
                  <div className="border-t pt-4">
                    <div className="flex items-center gap-1 text-sm text-gray-600 mb-1">
                      <Clock className="w-3 h-3" />
                      Duration
                    </div>
                    <p className="text-2xl font-bold text-doubleword-neutral-900">
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
                <div className="space-y-6">
                  {/* Token Usage */}
                  {request.total_tokens != null && (
                    <div>
                      <h4 className="text-sm font-medium text-gray-900 mb-3">
                        Token Usage
                      </h4>
                      <div className="grid grid-cols-2 gap-4">
                        <div className="text-center p-3 rounded-lg">
                          <p className="text-2xl font-bold">
                            {(request.prompt_tokens ?? 0).toLocaleString()}
                          </p>
                          <p className="text-xs text-gray-600 mt-1">
                            Prompt Tokens
                          </p>
                        </div>
                        <div className="text-center p-3 rounded-lg">
                          <p className="text-2xl font-bold">
                            {(request.completion_tokens ?? 0).toLocaleString()}
                          </p>
                          <p className="text-xs text-gray-600 mt-1">
                            Completion Tokens
                          </p>
                        </div>
                        {request.reasoning_tokens != null &&
                          request.reasoning_tokens > 0 && (
                            <div className="text-center p-3 rounded-lg">
                              <p className="text-2xl font-bold">
                                {request.reasoning_tokens.toLocaleString()}
                              </p>
                              <p className="text-xs text-gray-600 mt-1">
                                Reasoning Tokens
                              </p>
                            </div>
                          )}
                        <div className="text-center p-3 rounded-lg">
                          <p className="text-2xl font-bold text-gray-900">
                            {request.total_tokens.toLocaleString()}
                          </p>
                          <p className="text-xs text-gray-600 mt-1">
                            Total Tokens
                          </p>
                        </div>
                      </div>
                    </div>
                  )}

                  {/* Cost */}
                  {request.total_cost != null && request.total_cost > 0 && (
                    <div className="border-t pt-6">
                      <h4 className="text-sm font-medium text-gray-900 mb-3">
                        Cost
                      </h4>
                      <div className="p-4 rounded-lg text-center">
                        <p className="text-3xl font-bold text-green-700">
                          {formatCost(request.total_cost)}
                        </p>
                        <p className="text-xs text-gray-600 mt-1">
                          Total Cost
                        </p>
                      </div>
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
                  <span className="inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium bg-blue-100 text-blue-800">
                    {request.model}
                  </span>
                </div>
                <div>
                  <p className="text-sm text-gray-600 mb-1">Completion Window</p>
                  <p className="font-medium">{request.completion_window}</p>
                </div>
                <div className="border-t pt-4">
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
