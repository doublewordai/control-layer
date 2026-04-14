import { useParams, useNavigate, Link } from "react-router-dom";
import { useAsyncRequest } from "../../../api/control-layer/hooks";
import { cn } from "../../../lib/utils";

const statusStyles: Record<string, string> = {
  completed: "bg-green-500/10 text-green-400",
  failed: "bg-red-500/10 text-red-400",
  processing: "bg-blue-500/10 text-blue-400",
  claimed: "bg-blue-500/10 text-blue-400",
  pending: "bg-yellow-500/10 text-yellow-400",
  canceled: "bg-gray-500/10 text-gray-400",
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
  if (!ms) return "—";
  const seconds = Math.round(ms / 1000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const remainingSeconds = seconds % 60;
  return `${minutes}m ${remainingSeconds}s`;
}

const roleColors: Record<string, string> = {
  system: "text-purple-400",
  user: "text-blue-400",
  assistant: "text-green-400",
};

export function AsyncRequestDetail() {
  const { requestId } = useParams<{ requestId: string }>();
  const navigate = useNavigate();
  const { data: request, isLoading } = useAsyncRequest(requestId);

  if (isLoading) {
    return (
      <div className="p-6">
        <div className="animate-pulse space-y-4">
          <div className="h-8 w-48 bg-muted rounded" />
          <div className="h-64 bg-muted rounded" />
        </div>
      </div>
    );
  }

  if (!request) {
    return (
      <div className="p-6">
        <p className="text-muted-foreground">Request not found.</p>
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
      <div className="flex items-center gap-3 border-b px-6 py-4">
        <button
          onClick={() => navigate("/async")}
          className="text-sm text-muted-foreground hover:text-foreground"
        >
          ← Back
        </button>
        <div className="h-4 w-px bg-border" />
        <h1 className="text-lg font-semibold">Request Detail</h1>
        <span
          className={cn(
            "inline-flex items-center rounded px-2 py-0.5 text-xs font-medium",
            statusStyles[status] || "bg-gray-500/10 text-gray-400",
          )}
        >
          {displayStatus}
        </span>
      </div>

      {/* Two column layout */}
      <div className="flex h-[calc(100%-57px)]">
        {/* Left: Input/Output */}
        <div className="flex-1 overflow-auto border-r p-6 space-y-6">
          <div>
            <h3 className="text-xs uppercase tracking-wide text-muted-foreground mb-3">
              Input
            </h3>
            <div className="rounded-lg border bg-muted/30 p-4 space-y-3">
              {messages.map((msg, i) => (
                <div key={i}>
                  <span
                    className={cn(
                      "text-[10px] uppercase tracking-wide font-medium block mb-1",
                      roleColors[msg.role] || "text-gray-400",
                    )}
                  >
                    {msg.role}
                  </span>
                  <p className="text-sm leading-relaxed whitespace-pre-wrap">
                    {msg.content}
                  </p>
                </div>
              ))}
            </div>
          </div>

          <div>
            <h3 className="text-xs uppercase tracking-wide text-muted-foreground mb-3">
              Output
            </h3>
            <div className="rounded-lg border bg-muted/30 p-4">
              {status === "completed" && responseContent ? (
                <div>
                  <span className="text-[10px] uppercase tracking-wide font-medium text-green-400 block mb-1">
                    Assistant
                  </span>
                  <p className="text-sm leading-relaxed whitespace-pre-wrap">
                    {responseContent}
                  </p>
                </div>
              ) : status === "failed" ? (
                <div>
                  <span className="text-[10px] uppercase tracking-wide font-medium text-red-400 block mb-1">
                    Error
                  </span>
                  <p className="text-sm text-red-400">
                    {request.error || "Request failed"}
                  </p>
                </div>
              ) : (
                <p className="text-sm text-muted-foreground">
                  Waiting for response...
                </p>
              )}
            </div>
          </div>
        </div>

        {/* Right: Metadata sidebar */}
        <div className="w-64 flex-shrink-0 overflow-auto p-6">
          <h3 className="text-xs uppercase tracking-wide text-muted-foreground mb-4">
            Details
          </h3>

          <div className="space-y-4">
            <MetadataField label="Status">
              <span
                className={cn(
                  "inline-flex items-center rounded px-2 py-0.5 text-xs font-medium",
                  statusStyles[status] || "bg-gray-500/10 text-gray-400",
                )}
              >
                {displayStatus}
              </span>
            </MetadataField>

            <MetadataField label="Model">
              <span className="text-sm">{request.model}</span>
            </MetadataField>

            <MetadataField label="Created">
              <span className="text-sm">
                {new Date(request.created_at).toLocaleString(undefined, {
                  month: "short",
                  day: "numeric",
                  hour: "numeric",
                  minute: "2-digit",
                })}
              </span>
            </MetadataField>

            <MetadataField label="Duration">
              <span className="text-sm">
                {formatDuration(request.duration_ms)}
              </span>
            </MetadataField>
          </div>

          <div className="my-4 h-px bg-border" />

          <h3 className="text-xs uppercase tracking-wide text-muted-foreground mb-4">
            Batch
          </h3>

          <div className="space-y-4">
            <MetadataField label="Batch ID">
              <Link
                to={`/batches/${request.batch_id}`}
                className="text-xs font-mono text-indigo-400 hover:underline"
              >
                {request.batch_id.slice(0, 12)}...
              </Link>
            </MetadataField>

            <MetadataField label="Completion Window">
              <span className="text-sm">{request.completion_window}</span>
            </MetadataField>
          </div>
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
      <div className="text-xs text-muted-foreground mb-1">{label}</div>
      {children}
    </div>
  );
}
