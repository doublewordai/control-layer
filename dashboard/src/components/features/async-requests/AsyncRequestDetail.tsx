import { useParams, useNavigate, Link } from "react-router-dom";
import { ArrowLeft } from "lucide-react";
import { useAsyncRequest } from "../../../api/control-layer/hooks";

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
    <div className="h-full">
      {/* Header */}
      <div className="flex items-center gap-4 border-b border-doubleword-border px-6 py-4">
        <button
          onClick={() => navigate("/async")}
          className="p-2 text-doubleword-neutral-600 hover:bg-doubleword-neutral-100 rounded-lg transition-colors shrink-0"
          aria-label="Back to Async"
        >
          <ArrowLeft className="w-5 h-5" />
        </button>
        <div className="flex items-center gap-3">
          <h1 className="text-2xl font-bold text-doubleword-neutral-900">
            Request Detail
          </h1>
          <span
            className={`inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium ${getStatusColor(status)}`}
          >
            {displayStatus}
          </span>
        </div>
      </div>

      {/* Two column layout */}
      <div className="flex h-[calc(100%-65px)]">
        {/* Left: Input/Output */}
        <div className="flex-1 overflow-auto border-r border-doubleword-border p-6 space-y-6">
          <div>
            <h3 className="text-xs uppercase tracking-wide font-medium text-doubleword-neutral-600 mb-3">
              Input
            </h3>
            <div className="rounded-lg border border-doubleword-border bg-doubleword-background-secondary p-4 space-y-3">
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
          </div>

          <div>
            <h3 className="text-xs uppercase tracking-wide font-medium text-doubleword-neutral-600 mb-3">
              Output
            </h3>
            <div className="rounded-lg border border-doubleword-border bg-doubleword-background-secondary p-4">
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
            </div>
          </div>
        </div>

        {/* Right: Metadata sidebar */}
        <div className="w-64 flex-shrink-0 overflow-auto p-6">
          <h3 className="text-xs uppercase tracking-wide font-medium text-doubleword-neutral-600 mb-4">
            Details
          </h3>

          <div className="space-y-4">
            <MetadataField label="Request ID">
              <span className="font-mono text-xs text-doubleword-neutral-700 break-all">
                {request.id}
              </span>
            </MetadataField>

            <MetadataField label="Status">
              <span
                className={`inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium ${getStatusColor(status)}`}
              >
                {displayStatus}
              </span>
            </MetadataField>

            <MetadataField label="Model">
              <span className="inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium bg-blue-100 text-blue-800">
                {request.model}
              </span>
            </MetadataField>

            <MetadataField label="Created">
              <span className="text-sm text-doubleword-neutral-900">
                {new Date(request.created_at).toLocaleString()}
              </span>
            </MetadataField>

            <MetadataField label="Duration">
              <span className="text-sm text-doubleword-neutral-900">
                {formatDuration(request.duration_ms)}
              </span>
            </MetadataField>
          </div>

          {/* Token Metrics */}
          {request.total_tokens != null && (
            <>
              <div className="my-4 h-px bg-doubleword-border" />

              <h3 className="text-xs uppercase tracking-wide font-medium text-doubleword-neutral-600 mb-4">
                Tokens
              </h3>

              <div className="space-y-3">
                <div className="flex justify-between text-sm">
                  <span className="text-doubleword-neutral-600">Prompt</span>
                  <span className="text-doubleword-neutral-900 tabular-nums">
                    {(request.prompt_tokens ?? 0).toLocaleString()}
                  </span>
                </div>
                <div className="flex justify-between text-sm">
                  <span className="text-doubleword-neutral-600">Completion</span>
                  <span className="text-doubleword-neutral-900 tabular-nums">
                    {(request.completion_tokens ?? 0).toLocaleString()}
                  </span>
                </div>
                {request.reasoning_tokens != null && request.reasoning_tokens > 0 && (
                  <div className="flex justify-between text-sm">
                    <span className="text-doubleword-neutral-600">Reasoning</span>
                    <span className="text-doubleword-neutral-900 tabular-nums">
                      {request.reasoning_tokens.toLocaleString()}
                    </span>
                  </div>
                )}
                <div className="flex justify-between text-sm font-medium border-t border-doubleword-border-light pt-2">
                  <span className="text-doubleword-neutral-900">Total</span>
                  <span className="text-doubleword-neutral-900 tabular-nums">
                    {request.total_tokens.toLocaleString()}
                  </span>
                </div>
              </div>
            </>
          )}

          {/* Cost */}
          {request.total_cost != null && (
            <>
              <div className="my-4 h-px bg-doubleword-border" />

              <MetadataField label="Cost">
                <span className="text-sm font-medium text-green-700">
                  {formatCost(request.total_cost)}
                </span>
              </MetadataField>
            </>
          )}

          <div className="my-4 h-px bg-doubleword-border" />

          <Link
            to={`/batches/${request.batch_id}`}
            className="text-xs text-doubleword-neutral-600 hover:text-doubleword-neutral-900 hover:underline"
          >
            View related batch →
          </Link>
        </div>
      </div>
    </div>
  );
}

function MetadataField({
  label,
  children,
}: {
  label: string;
  children: React.ReactNode;
}) {
  return (
    <div>
      <div className="text-xs text-doubleword-neutral-600 mb-1">{label}</div>
      {children}
    </div>
  );
}
