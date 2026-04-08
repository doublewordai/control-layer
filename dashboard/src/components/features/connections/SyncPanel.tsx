import { useState } from "react";
import {
  Loader2,
  ChevronDown,
  ChevronRight,
  FileText,
  CheckCircle2,
  XCircle,
  Clock,
  SkipForward,
} from "lucide-react";
import { useSyncEntries } from "@/api/control-layer/hooks";
import type { SyncOperation } from "@/api/control-layer/types";
import { Badge } from "@/components/ui/badge";
import { useQuery } from "@tanstack/react-query";
import { dwctlApi } from "@/api/control-layer/client";
import { queryKeys } from "@/api/control-layer/keys";

function formatDate(ts: number) {
  return new Date(ts * 1000).toLocaleString();
}

function formatDuration(startTs: number, endTs: number) {
  const seconds = endTs - startTs;
  if (seconds < 60) return `${seconds}s`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
  return `${Math.floor(seconds / 3600)}h ${Math.floor((seconds % 3600) / 60)}m`;
}

function SyncStatusIcon({ status }: { status: string }) {
  switch (status) {
    case "completed":
      return <CheckCircle2 className="h-4 w-4 text-green-600" />;
    case "failed":
      return <XCircle className="h-4 w-4 text-red-500" />;
    case "cancelled":
      return <SkipForward className="h-4 w-4 text-gray-400" />;
    case "pending":
      return <Clock className="h-4 w-4 text-gray-400" />;
    default:
      return <Loader2 className="h-4 w-4 text-blue-500 animate-spin" />;
  }
}

function EntryStatusBadge({ status }: { status: string }) {
  const config: Record<string, { label: string; className: string }> = {
    pending: { label: "Pending", className: "bg-gray-100 text-gray-600" },
    ingesting: { label: "Ingesting", className: "bg-blue-100 text-blue-700" },
    ingested: { label: "Ingested", className: "bg-cyan-100 text-cyan-700" },
    activating: { label: "Activating", className: "bg-yellow-100 text-yellow-700" },
    activated: { label: "Batch Created", className: "bg-green-100 text-green-700" },
    failed: { label: "Failed", className: "bg-red-100 text-red-700" },
    skipped: { label: "Skipped", className: "bg-gray-100 text-gray-500" },
  };
  const c = config[status] || { label: status, className: "bg-gray-100 text-gray-600" };
  return (
    <Badge variant="outline" className={c.className}>
      {c.label}
    </Badge>
  );
}

// ---------------------------------------------------------------------------
// Progress bar
// ---------------------------------------------------------------------------

function ProgressBar({
  value,
  max,
  label,
  colorClass = "bg-blue-400/70",
}: {
  value: number;
  max: number;
  label: string;
  colorClass?: string;
}) {
  const pct = max > 0 ? Math.round((value / max) * 100) : 0;
  return (
    <div className="space-y-1">
      <div className="flex justify-between text-xs text-muted-foreground">
        <span>{label}</span>
        <span>
          {value} / {max} ({pct}%)
        </span>
      </div>
      <div className="h-1.5 rounded-full bg-muted/60 overflow-hidden">
        <div
          className={`h-full rounded-full transition-all duration-500 ${colorClass}`}
          style={{ width: `${pct}%` }}
        />
      </div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// Sync entry list
// ---------------------------------------------------------------------------

function SyncEntryList({
  connectionId,
  syncId,
  filesExpected,
}: {
  connectionId: string;
  syncId: string;
  filesExpected: number;
}) {
  const { data: entriesResponse, isLoading, isFetching } = useSyncEntries(connectionId, syncId);
  const entries = entriesResponse?.data;

  if (isLoading || (isFetching && !entries?.length)) {
    return (
      <div className="flex items-center gap-2 py-3 text-sm text-muted-foreground">
        <Loader2 className="h-3 w-3 animate-spin" />
        Loading entries...
      </div>
    );
  }

  if (!entries?.length) {
    if (filesExpected > 0) {
      return (
        <div className="flex items-center gap-2 py-3 text-sm text-muted-foreground">
          <Loader2 className="h-3 w-3 animate-spin" />
          Loading entries...
        </div>
      );
    }
    return (
      <p className="text-sm text-muted-foreground py-2">No new files to process — all files were already synced.</p>
    );
  }

  return (
    <div className="space-y-1">
      {entries.map((entry) => (
        <div key={entry.id} className="py-1.5 px-2 rounded text-sm hover:bg-muted/50">
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-2 min-w-0 flex-1">
              <FileText className="h-3.5 w-3.5 text-muted-foreground shrink-0" />
              <span className="truncate font-mono text-xs">{entry.external_key}</span>
            </div>
            <div className="flex items-center gap-3 shrink-0 ml-3">
              {entry.template_count != null && (
                <span className="text-xs text-muted-foreground">
                  {entry.template_count.toLocaleString()} rows
                </span>
              )}
              {entry.external_size_bytes != null && (
                <span className="text-xs text-muted-foreground">
                  {(entry.external_size_bytes / 1024).toFixed(0)} KB
                </span>
              )}
              <EntryStatusBadge status={entry.status} />
              {entry.batch_id && (
                <a
                  href={`/batches?search=${entry.batch_id}`}
                  className="text-xs text-blue-600 hover:underline"
                >
                  batch
                </a>
              )}
            </div>
          </div>
          {entry.error && (
            <p className="text-xs text-red-500 mt-1 ml-6">{entry.error}</p>
          )}
        </div>
      ))}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Single sync row (expandable)
// ---------------------------------------------------------------------------

function SyncRow({
  sync,
  connectionId,
}: {
  sync: SyncOperation;
  connectionId: string;
}) {
  const isTerminal = ["completed", "failed", "cancelled"].includes(sync.status);
  const [expanded, setExpanded] = useState(!isTerminal);

  // Poll this specific sync while it's in progress
  const { data: liveSync } = useQuery({
    queryKey: queryKeys.connections.sync(connectionId, sync.id),
    queryFn: () => dwctlApi.connections.getSync(connectionId, sync.id),
    enabled: !isTerminal,
    refetchInterval: !isTerminal ? 2000 : false,
    initialData: sync,
  });

  const s = liveSync ?? sync;
  const sTerminal = ["completed", "failed", "cancelled"].includes(s.status);
  const newFiles = s.files_found - s.files_skipped;

  return (
    <div className="border rounded-md">
      <button
        className="flex items-center justify-between w-full p-3 text-left hover:bg-muted/30 transition-colors"
        onClick={() => setExpanded(!expanded)}
      >
        <div className="flex items-center gap-2.5">
          {expanded ? (
            <ChevronDown className="h-3.5 w-3.5 text-muted-foreground" />
          ) : (
            <ChevronRight className="h-3.5 w-3.5 text-muted-foreground" />
          )}
          <SyncStatusIcon status={s.status} />
          <span className="text-sm font-medium capitalize">{s.status}</span>
          <span className="text-xs text-muted-foreground">
            {s.strategy}
          </span>
          {!sTerminal && s.files_found > 0 && (
            <span className="text-xs text-muted-foreground">
              &middot; {s.batches_created}/{newFiles} batches
            </span>
          )}
        </div>
        <div className="flex items-center gap-4 text-xs text-muted-foreground">
          <span>{formatDate(s.created_at)}</span>
          {s.started_at && (
            <span>
              {s.completed_at
                ? formatDuration(s.started_at, s.completed_at)
                : formatDuration(s.started_at, Math.floor(Date.now() / 1000)) + "..."}
            </span>
          )}
        </div>
      </button>

      {expanded && (
        <div className="border-t px-3 py-3 space-y-3">
          {s.status === "pending" ? (
            <div className="flex items-center gap-2 text-sm text-muted-foreground py-2">
              <Loader2 className="h-3 w-3 animate-spin" />
              Starting sync...
            </div>
          ) : s.status === "listing" ? (
            <div className="flex items-center gap-2 text-sm text-muted-foreground py-2">
              <Loader2 className="h-3 w-3 animate-spin" />
              Discovering files...
            </div>
          ) : (
            <>
              {/* Progress bars — only show when there are new files to process */}
              {newFiles > 0 && (
                <div className="space-y-2">
                  <ProgressBar
                    label="Files Processed"
                    value={s.files_ingested + s.files_failed}
                    max={newFiles}
                    colorClass={s.files_failed > 0 ? "bg-yellow-400/70" : "bg-blue-400/70"}
                  />
                  <ProgressBar
                    label="Batches Created"
                    value={s.batches_created}
                    max={newFiles > 0 ? newFiles - s.files_failed : 0}
                    colorClass="bg-green-400/70"
                  />
                </div>
              )}

              {/* Summary counters */}
              <div className="flex flex-wrap gap-x-6 gap-y-1 text-sm">
                <div>
                  <span className="text-muted-foreground">Found: </span>
                  <span className="font-medium">{s.files_found}</span>
                </div>
                {s.files_skipped > 0 && (
                  <div>
                    <span className="text-muted-foreground">Skipped: </span>
                    <span className="font-medium">{s.files_skipped}</span>
                  </div>
                )}
                <div>
                  <span className="text-muted-foreground">Ingested: </span>
                  <span className="font-medium">{s.files_ingested}</span>
                </div>
                {s.files_failed > 0 && (
                  <div>
                    <span className="text-muted-foreground text-red-500">Failed: </span>
                    <span className="font-medium text-red-500">{s.files_failed}</span>
                  </div>
                )}
                <div>
                  <span className="text-muted-foreground">Batches Created: </span>
                  <span className="font-medium">{s.batches_created}</span>
                </div>
              </div>

              {/* File entries — show list if there are new files, otherwise show "all synced" */}
              {newFiles > 0 ? (
                <SyncEntryList connectionId={connectionId} syncId={s.id} filesExpected={newFiles} />
              ) : (
                <p className="text-sm text-muted-foreground py-2">
                  No new files to process — all files were already synced.
                </p>
              )}
            </>
          )}
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Sync panel (shows all syncs for a connection)
// ---------------------------------------------------------------------------

export function SyncPanel({ connectionId }: { connectionId: string }) {
  const [page, setPage] = useState(0);

  // Poll the sync list while any sync is in progress
  const { data: syncsResponse, isLoading } = useQuery({
    queryKey: queryKeys.connections.syncs(connectionId),
    queryFn: () => dwctlApi.connections.listSyncs(connectionId),
    refetchInterval: (query) => {
      const list = query.state.data?.data;
      if (list?.some((s: { status: string }) => !["completed", "failed", "cancelled"].includes(s.status))) {
        return 2000;
      }
      return false;
    },
  });

  if (isLoading) {
    return (
      <div className="flex items-center gap-2 text-sm text-muted-foreground">
        <Loader2 className="h-3 w-3 animate-spin" />
        Loading sync history...
      </div>
    );
  }

  const syncs = syncsResponse?.data;

  if (!syncs?.length) {
    return (
      <p className="text-sm text-muted-foreground">
        No syncs yet. Click "Sync" to discover and ingest files from this source.
      </p>
    );
  }

  const PAGE_SIZE = 10;
  const totalPages = Math.ceil(syncs.length / PAGE_SIZE);
  const paginated = syncs.slice(page * PAGE_SIZE, (page + 1) * PAGE_SIZE);

  return (
    <div className="space-y-2">
      {paginated.map((sync) => (
        <SyncRow key={sync.id} sync={sync} connectionId={connectionId} />
      ))}
      {totalPages > 1 && (
        <div className="flex items-center justify-between text-xs text-muted-foreground pt-1">
          <span>
            {syncs.length} sync{syncs.length !== 1 ? "s" : ""} &middot; page {page + 1} of {totalPages}
          </span>
          <div className="flex items-center gap-1">
            <button
              onClick={() => setPage((p) => Math.max(0, p - 1))}
              disabled={page === 0}
              className="px-2 py-1 rounded hover:bg-muted disabled:opacity-30"
            >
              &larr; Newer
            </button>
            <button
              onClick={() => setPage((p) => Math.min(totalPages - 1, p + 1))}
              disabled={page >= totalPages - 1}
              className="px-2 py-1 rounded hover:bg-muted disabled:opacity-30"
            >
              Older &rarr;
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
