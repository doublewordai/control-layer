import { useCallback, useMemo, useState } from "react";

export interface ImportedDeployment {
  /** Provider model name — unique identifier within an endpoint */
  modelName: string;
  /** Alias (defaults to modelName when unset) */
  alias: string;
  /** Whether this deployment was added in the current session (vs. existed at modal open) */
  isNew: boolean;
}

export interface EndpointModelsState {
  /** Deployments visible in the table for the currently-loaded server window plus session adds. */
  deployments: ImportedDeployment[];
  /** Provider model names staged for removal in this session (may include rows not on the current page). */
  removedModelNames: string[];
  /** Provider model names that were added during this session */
  addedModelNames: string[];
  /** Full session-added deployments (modelName + alias). Use at submit time to rebuild model_filter. */
  addedDeployments: ImportedDeployment[];
  /** Alias edits keyed by server modelName (only entries that differ from the server's alias). */
  aliasEdits: Record<string, string>;
  /** Number of staged changes (additions + removals + alias edits) */
  changeCount: number;
  /** Has anything changed since modal open */
  hasChanges: boolean;

  /** Add a model deployment. No-op if already present (live or staged for removal — restored). */
  addModel: (modelName: string, defaultAlias?: string) => void;
  /**
   * Remove a model deployment.
   *
   * Returns an undo function that restores the deployment exactly as it was —
   * including any pending alias edit. The undo handles both freshly-added
   * deployments (which would otherwise be dropped from `staged.added`) and
   * server-side deployments (which sit in `staged.removed`).
   */
  removeModel: (modelName: string) => () => void;
  /** Update the alias for a deployment. */
  setAlias: (modelName: string, alias: string) => void;
  /** Discard all staged changes and revert to server state. */
  reset: () => void;
}

interface InitialDeployment {
  modelName: string;
  alias: string;
}

interface StagedState {
  /** Edits to existing server deployments. modelName -> new alias. Cleared on remove (preserved via undo closure). */
  aliases: Record<string, string>;
  /** Deployments added in this session that don't exist on the server. */
  added: ImportedDeployment[];
  /** Server-side deployments staged for removal. */
  removed: Set<string>;
}

const EMPTY_STAGED: StagedState = {
  aliases: {},
  added: [],
  removed: new Set(),
};

/**
 * Tracks the staged state of model deployments on an endpoint while a user
 * edits in the modal.
 *
 * `initial` is the currently-loaded server window (a single page when the
 * caller paginates). Staged removals/adds/alias edits are tracked as deltas
 * keyed by modelName so they survive page navigation. Submit-time callers
 * combine the deltas with a full server fetch to produce the authoritative
 * model_filter.
 */
export function useEndpointModelsState(
  initial: InitialDeployment[],
): EndpointModelsState {
  const [staged, setStaged] = useState<StagedState>(EMPTY_STAGED);

  // Index the current server window by modelName for O(1) lookup. Only
  // reflects rows the caller has loaded (e.g. the current page), not all
  // deployments on the endpoint.
  const serverByName = useMemo(() => {
    const m = new Map<string, InitialDeployment>();
    for (const d of initial) m.set(d.modelName, d);
    return m;
  }, [initial]);

  const addModel = useCallback(
    (modelName: string, defaultAlias?: string) => {
      setStaged((prev) => {
        // Restoring a previously-removed server deployment: just lift the removal.
        if (prev.removed.has(modelName)) {
          const next = new Set(prev.removed);
          next.delete(modelName);
          return { ...prev, removed: next };
        }
        // Already in server state or already optimistically added — no-op.
        if (serverByName.has(modelName)) return prev;
        if (prev.added.some((d) => d.modelName === modelName)) return prev;
        return {
          ...prev,
          added: [
            ...prev.added,
            { modelName, alias: defaultAlias ?? modelName, isNew: true },
          ],
        };
      });
    },
    [serverByName],
  );

  const removeModel = useCallback(
    (modelName: string): (() => void) => {
      // Captures (set inside the setStaged updater) so the returned undo
      // function can restore exactly what was removed, including any pending
      // alias edit on a server deployment.
      let capturedAdded: ImportedDeployment | null = null;
      let capturedAliasEdit: string | undefined = undefined;

      setStaged((prev) => {
        const inAdded = prev.added.find((d) => d.modelName === modelName);
        if (inAdded) {
          capturedAdded = { ...inAdded };
          return {
            ...prev,
            added: prev.added.filter((d) => d.modelName !== modelName),
          };
        }
        // Server deployment: stage a removal and pull any pending alias edit
        // out of staged.aliases so the change count reflects the staged remove
        // (not a leftover alias edit on a row that's no longer visible).
        capturedAliasEdit = prev.aliases[modelName];
        const nextRemoved = new Set(prev.removed);
        nextRemoved.add(modelName);
        const nextAliases = { ...prev.aliases };
        delete nextAliases[modelName];
        return { ...prev, removed: nextRemoved, aliases: nextAliases };
      });

      return () => {
        setStaged((prev) => {
          if (capturedAdded) {
            // Was a freshly-added deployment — re-append (idempotent).
            if (prev.added.some((d) => d.modelName === modelName)) return prev;
            return { ...prev, added: [...prev.added, capturedAdded!] };
          }
          // Was a server deployment — un-stage the removal and restore the alias edit if any.
          if (!prev.removed.has(modelName)) return prev;
          const next = new Set(prev.removed);
          next.delete(modelName);
          const aliases =
            capturedAliasEdit !== undefined
              ? { ...prev.aliases, [modelName]: capturedAliasEdit }
              : prev.aliases;
          return { ...prev, removed: next, aliases };
        });
      };
    },
    [],
  );

  const setAlias = useCallback(
    (modelName: string, alias: string) => {
      setStaged((prev) => {
        // Newly-added: update the entry in added directly.
        if (prev.added.some((d) => d.modelName === modelName)) {
          return {
            ...prev,
            added: prev.added.map((d) =>
              d.modelName === modelName ? { ...d, alias } : d,
            ),
          };
        }
        // Server-side deployment: store in aliases map (or clear if same as server).
        const original = serverByName.get(modelName)?.alias;
        if (original !== undefined && alias === original) {
          const next = { ...prev.aliases };
          delete next[modelName];
          return { ...prev, aliases: next };
        }
        return { ...prev, aliases: { ...prev.aliases, [modelName]: alias } };
      });
    },
    [serverByName],
  );

  const reset = useCallback(() => {
    setStaged(EMPTY_STAGED);
  }, []);

  const deployments = useMemo<ImportedDeployment[]>(() => {
    const surviving: ImportedDeployment[] = [];
    for (const d of initial) {
      if (staged.removed.has(d.modelName)) continue;
      surviving.push({
        modelName: d.modelName,
        alias: staged.aliases[d.modelName] ?? d.alias,
        isNew: false,
      });
    }
    // Drop any staged adds that overlap a server name in the current window
    // so the user never sees the same row twice. Cross-window dups (if the
    // user added a name that lives on a different page) are resolved at
    // submit time.
    const safeAdded = staged.added.filter((d) => !serverByName.has(d.modelName));
    return [...surviving, ...safeAdded];
  }, [initial, staged, serverByName]);

  // Removals are tracked as deltas keyed by modelName; with a paginated
  // server window we can't pin them to the rows currently in view, so we
  // surface every staged removal.
  const removedModelNames = useMemo(
    () => Array.from(staged.removed),
    [staged.removed],
  );

  const addedModelNames = useMemo(
    () => staged.added.map((d) => d.modelName),
    [staged.added],
  );

  // setAlias clears entries that match the server's current value, so any
  // entry left in staged.aliases is by definition an edit. Subtract any that
  // overlap with removed rows to avoid double-counting.
  const aliasEditsCount = useMemo(() => {
    let n = 0;
    for (const name of Object.keys(staged.aliases)) {
      if (staged.removed.has(name)) continue;
      n++;
    }
    return n;
  }, [staged.aliases, staged.removed]);

  const changeCount =
    addedModelNames.length + removedModelNames.length + aliasEditsCount;
  const hasChanges = changeCount > 0;

  return {
    deployments,
    removedModelNames,
    addedModelNames,
    addedDeployments: staged.added,
    aliasEdits: staged.aliases,
    changeCount,
    hasChanges,
    addModel,
    removeModel,
    setAlias,
    reset,
  };
}
