import { useState } from "react";
import {
  Cable,
  Plus,
  Trash2,
  RefreshCw,
  CheckCircle2,
  Loader2,
  ChevronDown,
  ChevronRight,
  FolderOpen,
} from "lucide-react";
import { toast } from "sonner";
import {
  useConnections,
  useDeleteConnection,
  useTestConnection,
  useTriggerSync,
} from "@/api/control-layer/hooks";
import type { Connection } from "@/api/control-layer/types";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { CreateConnectionDialog } from "./CreateConnectionDialog";
import { SyncPanel } from "./SyncPanel";
import { FileBrowser } from "./FileBrowser";

function formatDate(ts: number) {
  return new Date(ts * 1000).toLocaleString();
}

function CollapsibleSection({
  title,
  icon,
  defaultOpen = false,
  children,
}: {
  title: string;
  icon?: React.ReactNode;
  defaultOpen?: boolean;
  children: React.ReactNode;
}) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <div>
      <button
        className="flex items-center gap-2 text-sm font-medium text-muted-foreground hover:text-foreground transition-colors w-full text-left"
        onClick={() => setOpen(!open)}
      >
        {open ? (
          <ChevronDown className="h-3.5 w-3.5" />
        ) : (
          <ChevronRight className="h-3.5 w-3.5" />
        )}
        {icon}
        {title}
      </button>
      {open && <div className="mt-2">{children}</div>}
    </div>
  );
}

export function Connections() {
  const { data: connections, isLoading } = useConnections("source");
  const deleteMutation = useDeleteConnection();
  const testMutation = useTestConnection();
  const syncMutation = useTriggerSync();

  const [createOpen, setCreateOpen] = useState(false);
  const [deleteTarget, setDeleteTarget] = useState<Connection | null>(null);
  const [expandedId, setExpandedId] = useState<string | null>(null);

  const handleTest = async (conn: Connection) => {
    try {
      const result = await testMutation.mutateAsync(conn.id);
      if (result.ok) {
        toast.success(`Connection "${conn.name}" is healthy`);
      } else {
        toast.error(`Connection test failed: ${result.message}`);
      }
    } catch {
      toast.error("Failed to test connection");
    }
  };

  const handleSync = async (conn: Connection) => {
    try {
      await syncMutation.mutateAsync({
        connectionId: conn.id,
        data: { strategy: "snapshot" },
      });
      toast.success(`Sync started for "${conn.name}"`);
      setExpandedId(conn.id);
    } catch {
      toast.error("Failed to trigger sync");
    }
  };

  const handleDelete = async () => {
    if (!deleteTarget) return;
    try {
      await deleteMutation.mutateAsync(deleteTarget.id);
      toast.success(`Connection "${deleteTarget.name}" deleted`);
      setDeleteTarget(null);
    } catch {
      toast.error("Failed to delete connection");
    }
  };

  return (
    <div className="p-6 max-w-5xl">
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Connections</h1>
          <p className="text-sm text-muted-foreground mt-1">
            Connect external data sources to sync files and create batches automatically.
          </p>
        </div>
        <Button
          onClick={() => setCreateOpen(true)}
          className="bg-doubleword-background-dark hover:bg-doubleword-neutral-900"
        >
          <Plus className="h-4 w-4 mr-2" />
          New Connection
        </Button>
      </div>

      {isLoading ? (
        <div className="flex items-center justify-center py-20 text-muted-foreground">
          <Loader2 className="h-5 w-5 animate-spin mr-2" />
          Loading connections...
        </div>
      ) : !connections?.length ? (
        <div className="flex flex-col items-center justify-center py-20 text-center">
          <Cable className="h-12 w-12 text-muted-foreground/40 mb-4" />
          <h3 className="text-lg font-medium text-muted-foreground">No connections yet</h3>
          <p className="text-sm text-muted-foreground/70 mt-1 max-w-sm">
            Create a connection to an S3 bucket to start syncing files for batch processing.
          </p>
          <Button
            onClick={() => setCreateOpen(true)}
            variant="outline"
            className="mt-4"
          >
            <Plus className="h-4 w-4 mr-2" />
            Create your first connection
          </Button>
        </div>
      ) : (
        <div className="space-y-3">
          {connections.map((conn) => (
            <div key={conn.id} className="border rounded-lg">
              <div className="flex items-center justify-between p-4">
                <button
                  className="flex items-center gap-3 text-left flex-1 min-w-0"
                  onClick={() => setExpandedId(expandedId === conn.id ? null : conn.id)}
                >
                  {expandedId === conn.id ? (
                    <ChevronDown className="h-4 w-4 text-muted-foreground shrink-0" />
                  ) : (
                    <ChevronRight className="h-4 w-4 text-muted-foreground shrink-0" />
                  )}
                  <div className="min-w-0">
                    <div className="font-medium truncate">{conn.name}</div>
                    <div className="text-xs text-muted-foreground">
                      {conn.provider.toUpperCase()} &middot; Created {formatDate(conn.created_at)}
                    </div>
                  </div>
                </button>

                <div className="flex items-center gap-2 shrink-0">
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => handleTest(conn)}
                    disabled={testMutation.isPending}
                  >
                    {testMutation.isPending ? (
                      <Loader2 className="h-3 w-3 animate-spin" />
                    ) : (
                      <CheckCircle2 className="h-3 w-3" />
                    )}
                    <span className="ml-1.5">Test</span>
                  </Button>
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => handleSync(conn)}
                    disabled={syncMutation.isPending}
                  >
                    {syncMutation.isPending ? (
                      <Loader2 className="h-3 w-3 animate-spin" />
                    ) : (
                      <RefreshCw className="h-3 w-3" />
                    )}
                    <span className="ml-1.5">Sync</span>
                  </Button>
                  <Button
                    variant="ghost"
                    size="sm"
                    onClick={() => setDeleteTarget(conn)}
                    className="text-red-500 hover:text-red-700 hover:bg-red-50"
                  >
                    <Trash2 className="h-3 w-3" />
                  </Button>
                </div>
              </div>

              {expandedId === conn.id && (
                <div className="border-t px-4 py-4 space-y-4">
                  <CollapsibleSection title="Source Files" icon={<FolderOpen className="h-4 w-4" />}>
                    <FileBrowser connectionId={conn.id} />
                  </CollapsibleSection>
                  <CollapsibleSection title="Sync History" icon={<RefreshCw className="h-4 w-4" />} defaultOpen>
                    <SyncPanel connectionId={conn.id} />
                  </CollapsibleSection>
                </div>
              )}
            </div>
          ))}
        </div>
      )}

      <CreateConnectionDialog open={createOpen} onOpenChange={setCreateOpen} />

      <Dialog open={!!deleteTarget} onOpenChange={() => setDeleteTarget(null)}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Delete connection?</DialogTitle>
            <DialogDescription>
              This will permanently delete "{deleteTarget?.name}". Existing batches created from
              synced files will not be affected.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setDeleteTarget(null)}>
              Cancel
            </Button>
            <Button
              onClick={handleDelete}
              className="bg-red-600 hover:bg-red-700 text-white"
            >
              Delete
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
