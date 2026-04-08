import { useState, useMemo, useDeferredValue, useCallback, useEffect } from "react";
import {
  Loader2,
  FileText,
  ChevronLeft,
  ChevronRight,
  Search,
  RefreshCw,
  CheckCircle2,
} from "lucide-react";
import { toast } from "sonner";
import { useConnectionFiles, useSyncedKeys, useTriggerSync } from "@/api/control-layer/hooks";
import { useQueryClient } from "@tanstack/react-query";
import { queryKeys } from "@/api/control-layer/keys";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import type { ExternalFile } from "@/api/control-layer/types";

function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
}

function formatDate(ts: number) {
  return new Date(ts * 1000).toLocaleDateString(undefined, {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

const PAGE_SIZE = 50;

export function FileBrowser({ connectionId }: { connectionId: string }) {
  const [search, setSearch] = useState("");
  const deferredSearch = useDeferredValue(search);
  const [cursors, setCursors] = useState<string[]>([]);
  const [currentCursor, setCurrentCursor] = useState<string | undefined>();
  const [selected, setSelected] = useState<Set<string>>(new Set());
  // Track keys we've triggered a sync for (optimistic status)
  const [syncingKeys, setSyncingKeys] = useState<Set<string>>(new Set());

  const queryClient = useQueryClient();

  const { data, isLoading, isFetching } = useConnectionFiles(connectionId, {
    search: deferredSearch || undefined,
    cursor: currentCursor,
    limit: PAGE_SIZE,
  });

  const files = useMemo(() => data?.data ?? [], [data]);

  const { data: syncedKeys, isFetching: isFetchingSynced } = useSyncedKeys(connectionId);
  const syncMutation = useTriggerSync();

  // Poll synced-keys while we have files in "syncing" state, to auto-transition them
  useEffect(() => {
    if (syncingKeys.size === 0) return;
    const interval = setInterval(() => {
      queryClient.invalidateQueries({
        queryKey: [...queryKeys.connections.all, connectionId, "synced-keys"],
      });
    }, 3000);
    return () => clearInterval(interval);
  }, [syncingKeys.size, queryClient, connectionId]);

  // When synced keys update, clear syncing state for files that are now fully synced.
  // Only clear if the synced timestamp covers the file's current last_modified —
  // otherwise a modified file would flash from "syncing" back to "modified".
  useEffect(() => {
    if (syncingKeys.size === 0 || !syncedKeys) return;
    const syncedMap = new Map(syncedKeys.map((sk) => [sk.key, sk.last_modified ?? null]));
    const stillSyncing = new Set<string>();
    for (const key of syncingKeys) {
      const syncedTs = syncedMap.get(key);
      const file = files.find((f) => f.key === key);
      const fileTs = file?.last_modified ?? null;
      // Only clear if synced timestamp >= file timestamp (fully synced)
      if (syncedTs != null && fileTs != null && syncedTs >= fileTs) {
        // Fully synced — don't keep in syncing
      } else if (syncedTs != null && fileTs == null) {
        // Synced and no file timestamp to compare — assume done
      } else {
        stillSyncing.add(key);
      }
    }
    if (stillSyncing.size !== syncingKeys.size) {
      setSyncingKeys(stillSyncing);
    }
  }, [syncedKeys, syncingKeys, files]);

  // Track synced keys with their last_modified timestamps.
  // A file is "synced" if its key exists and last_modified matches.
  // A file is "modified" if its key exists but last_modified is newer.
  const syncedKeyMap = useMemo(() => {
    const map = new Map<string, number | null>();
    if (syncedKeys) {
      for (const sk of syncedKeys) {
        map.set(sk.key, sk.last_modified ?? null);
      }
    }
    return map;
  }, [syncedKeys]);

  const isSyncing = (file: ExternalFile) => syncingKeys.has(file.key);

  const getStatus = (file: ExternalFile): "synced" | "modified" | "syncing" | "new" => {
    // Syncing takes priority — user just triggered this
    if (isSyncing(file)) return "syncing";
    if (!syncedKeyMap.has(file.key)) return "new";
    // Key was synced — check if it's been modified since
    const syncedTs = syncedKeyMap.get(file.key);
    const fileTs = file.last_modified ?? null;
    if (syncedTs != null && fileTs != null && fileTs > syncedTs) return "modified";
    return "synced";
  };

  const isSynced = (file: ExternalFile) => getStatus(file) === "synced";

  // "New" and "modified" files can be synced without force
  const selectedSyncableKeys = [...selected].filter((key) => {
    const file = files.find((f) => f.key === key);
    if (!file) return false;
    const s = getStatus(file);
    return s === "new" || s === "modified";
  });
  // Already-synced (unchanged) files need force to re-sync
  const selectedSyncedKeys = [...selected].filter((key) => {
    const file = files.find((f) => f.key === key);
    if (!file) return false;
    return getStatus(file) === "synced";
  });

  const toggleSelect = (key: string) => {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  };

  const toggleSelectAll = () => {
    if (selected.size === files.length) {
      setSelected(new Set());
    } else {
      setSelected(new Set(files.map((f) => f.key)));
    }
  };

  const handleRefresh = useCallback(() => {
    // Invalidate all queries for this connection (files, synced-keys, syncs)
    queryClient.invalidateQueries({
      queryKey: ["connections", connectionId],
    });
    setSyncingKeys(new Set());
  }, [queryClient, connectionId]);

  const handleSyncNew = async () => {
    const newKeys = [...selected].filter((key) => {
      const file = files.find((f) => f.key === key);
      if (!file) return false;
      const s = getStatus(file);
      return s === "new" || s === "modified";
    });
    if (newKeys.length === 0) {
      toast.info("All selected files are already synced");
      return;
    }
    // Optimistically mark as syncing
    setSyncingKeys((prev) => new Set([...prev, ...newKeys]));
    try {
      await syncMutation.mutateAsync({
        connectionId,
        data: { strategy: "select", file_keys: newKeys },
      });
      toast.success(`Syncing ${newKeys.length} file${newKeys.length !== 1 ? "s" : ""}`);
      setSelected(new Set());
    } catch {
      // Revert optimistic status
      setSyncingKeys((prev) => {
        const next = new Set(prev);
        for (const k of newKeys) next.delete(k);
        return next;
      });
      toast.error("Failed to trigger sync");
    }
  };

  const handleResync = async () => {
    const keys = [...selected].filter(
      (key) => files.find((f) => f.key === key && isSynced(f)),
    );
    if (keys.length === 0) return;
    setSyncingKeys((prev) => new Set([...prev, ...keys]));
    try {
      await syncMutation.mutateAsync({
        connectionId,
        data: { strategy: "select", file_keys: keys, force: true },
      });
      toast.success(`Re-syncing ${keys.length} file${keys.length !== 1 ? "s" : ""}`);
      setSelected(new Set());
    } catch {
      setSyncingKeys((prev) => {
        const next = new Set(prev);
        for (const k of keys) next.delete(k);
        return next;
      });
      toast.error("Failed to trigger re-sync");
    }
  };

  const goNextPage = () => {
    if (data?.next_cursor) {
      setCursors((prev) => [...prev, currentCursor ?? ""]);
      setCurrentCursor(data.next_cursor);
      setSelected(new Set());
    }
  };

  const goPrevPage = () => {
    setCursors((prev) => {
      const next = [...prev];
      const prevCursor = next.pop();
      setCurrentCursor(prevCursor || undefined);
      return next;
    });
    setSelected(new Set());
  };

  const resetPagination = () => {
    setCursors([]);
    setCurrentCursor(undefined);
    setSelected(new Set());
  };

  return (
    <div className="space-y-3">
      {/* Search + sync buttons */}
      <div className="flex items-center gap-2">
        <div className="relative flex-1">
          <Search className="absolute left-2.5 top-2.5 h-3.5 w-3.5 text-muted-foreground" />
          <Input
            placeholder="Search files..."
            value={search}
            onChange={(e) => {
              setSearch(e.target.value);
              resetPagination();
            }}
            className="pl-8 h-9 text-sm"
          />
        </div>
        <Button
          variant="ghost"
          size="sm"
          onClick={handleRefresh}
          disabled={isFetchingSynced}
          className="h-9 px-2"
          title="Refresh sync status"
        >
          <RefreshCw className={`h-3.5 w-3.5 ${isFetchingSynced ? "animate-spin" : ""}`} />
        </Button>
        {isFetching && !isLoading && (
          <Loader2 className="h-3.5 w-3.5 text-muted-foreground animate-spin shrink-0" />
        )}
        {selected.size > 0 && (
          <div className="flex items-center gap-1.5 shrink-0">
            {selectedSyncableKeys.length > 0 && (
              <Button
                size="sm"
                onClick={handleSyncNew}
                disabled={syncMutation.isPending}
                className="h-9 bg-doubleword-background-dark hover:bg-doubleword-neutral-900"
              >
                {syncMutation.isPending ? (
                  <Loader2 className="h-3 w-3 animate-spin mr-1.5" />
                ) : (
                  <RefreshCw className="h-3 w-3 mr-1.5" />
                )}
                Sync {selectedSyncableKeys.length}
              </Button>
            )}
            {selectedSyncedKeys.length > 0 && (
              <Button
                variant="outline"
                size="sm"
                onClick={handleResync}
                disabled={syncMutation.isPending}
                className="h-9"
              >
                {syncMutation.isPending ? (
                  <Loader2 className="h-3 w-3 animate-spin mr-1.5" />
                ) : (
                  <RefreshCw className="h-3 w-3 mr-1.5" />
                )}
                Re-sync {selectedSyncedKeys.length}
              </Button>
            )}
          </div>
        )}
      </div>

      {isLoading ? (
        <div className="flex items-center gap-2 py-6 justify-center text-sm text-muted-foreground">
          <Loader2 className="h-4 w-4 animate-spin" />
          Loading files from source...
        </div>
      ) : !files.length ? (
        <p className="text-sm text-muted-foreground py-4 text-center">
          {search ? "No files match your search." : "No JSONL files found in this bucket."}
        </p>
      ) : (
        <>
          <div className="border rounded-md divide-y">
            {/* Header row */}
            <div className="flex items-center px-3 py-1.5 text-xs text-muted-foreground bg-muted/30">
              <div className="w-7 shrink-0">
                <Checkbox
                  checked={selected.size === files.length && files.length > 0}
                  onCheckedChange={toggleSelectAll}
                  aria-label="Select all files"
                />
              </div>
              <div className="flex-1">File</div>
              <div className="w-20 text-right">Size</div>
              <div className="w-44 text-right">Modified</div>
              <div className="w-24 text-right">Status</div>
            </div>

            {files.map((file) => {
              const status = getStatus(file);
              return (
                <div
                  key={file.key}
                  className={`flex items-center px-3 py-2 text-sm transition-colors ${
                    selected.has(file.key) ? "bg-blue-50/50" : "hover:bg-muted/30"
                  }`}
                >
                  <div className="w-7 shrink-0">
                    <Checkbox
                      checked={selected.has(file.key)}
                      onCheckedChange={() => toggleSelect(file.key)}
                      aria-label={`Select ${file.key}`}
                    />
                  </div>
                  <div className="flex items-center gap-2 min-w-0 flex-1">
                    <FileText className="h-3.5 w-3.5 text-muted-foreground shrink-0" />
                    <span
                      className={`truncate font-mono text-xs ${status !== "new" ? "text-muted-foreground" : ""}`}
                      title={file.key}
                    >
                      {file.display_name || file.key}
                    </span>
                  </div>
                  <div className="w-20 text-right text-xs text-muted-foreground">
                    {file.size_bytes != null ? formatBytes(file.size_bytes) : "\u2014"}
                  </div>
                  <div className="w-44 text-right text-xs text-muted-foreground">
                    {file.last_modified != null ? formatDate(file.last_modified) : "\u2014"}
                  </div>
                  <div className="w-24 text-right">
                    {status === "synced" ? (
                      <span className="inline-flex items-center gap-1 text-xs text-green-600">
                        <CheckCircle2 className="h-3 w-3" />
                        Synced
                      </span>
                    ) : status === "syncing" ? (
                      <span className="inline-flex items-center gap-1 text-xs text-blue-500">
                        <Loader2 className="h-3 w-3 animate-spin" />
                        Syncing
                      </span>
                    ) : status === "modified" ? (
                      <span className="inline-flex items-center gap-1 text-xs text-amber-600">
                        <RefreshCw className="h-3 w-3" />
                        Modified
                      </span>
                    ) : (
                      <span className="text-xs text-muted-foreground">New</span>
                    )}
                  </div>
                </div>
              );
            })}
          </div>

          {/* Pagination */}
          <div className="flex items-center justify-between text-xs text-muted-foreground">
            <span>
              {files.length} file{files.length !== 1 ? "s" : ""}
              {data?.has_more ? "+" : ""}
              {cursors.length > 0 ? ` \u00b7 page ${cursors.length + 1}` : ""}
            </span>
            <div className="flex items-center gap-1">
              <Button
                variant="ghost"
                size="sm"
                onClick={goPrevPage}
                disabled={cursors.length === 0}
                className="h-7 px-2"
              >
                <ChevronLeft className="h-3.5 w-3.5" />
              </Button>
              <Button
                variant="ghost"
                size="sm"
                onClick={goNextPage}
                disabled={!data?.has_more}
                className="h-7 px-2"
              >
                <ChevronRight className="h-3.5 w-3.5" />
              </Button>
            </div>
          </div>
        </>
      )}
    </div>
  );
}
