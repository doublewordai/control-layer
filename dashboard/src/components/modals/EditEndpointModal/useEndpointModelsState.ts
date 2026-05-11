import { useCallback, useEffect, useMemo, useState } from "react";

export interface ImportedDeployment {
  /** Provider model name — unique identifier within an endpoint */
  modelName: string;
  /** Alias (defaults to modelName when unset) */
  alias: string;
  /** Whether this deployment was added in the current session (vs. existed at modal open) */
  isNew: boolean;
}

export interface EndpointModelsState {
  /** Deployments visible in the table (server state minus removals plus additions) */
  deployments: ImportedDeployment[];
  /** Provider model names that were present at modal open but are staged for removal */
  removedModelNames: string[];
  /** Provider model names that were added during this session */
  addedModelNames: string[];
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
 * `initial` is the server state. It is consumed live (not snapshotted) so
 * that asynchronously-loaded server data is reflected in the hook's output
 * without needing a remount or reset. Staged user edits are merged on top
 * via {@link EndpointModelsState.deployments}.
 */
export function useEndpointModelsState(
  initial: InitialDeployment[],
): EndpointModelsState {
  const [staged, setStaged] = useState<StagedState>(EMPTY_STAGED);

  // Index server deployments by modelName for O(1) lookup.
  const serverByName = useMemo(() => {
    const m = new Map<string, InitialDeployment>();
    for (const d of initial) m.set(d.modelName, d);
    return m;
  }, [initial]);

  // If `initial` brings in a name the user had optimistically added (e.g.
  // they added a model before the server fetch completed), drop it from
  // `staged.added` — the server snapshot is now authoritative for that name.
  // We also drop any `staged.removed` entries that no longer correspond to a
  // server deployment, so a stale removal can't shadow a name that's been
  // re-imported externally.
  useEffect(() => {
    if (initial.length === 0) return;
    setStaged((prev) => {
      let changed = false;
      let added = prev.added;
      let removed = prev.removed;

      const overlapping = prev.added.filter((d) => serverByName.has(d.modelName));
      if (overlapping.length > 0) {
        added = prev.added.filter((d) => !serverByName.has(d.modelName));
        changed = true;
      }

      let removedChanged = false;
      const nextRemoved = new Set<string>();
      for (const name of prev.removed) {
        if (serverByName.has(name)) nextRemoved.add(name);
        else removedChanged = true;
      }
      if (removedChanged) {
        removed = nextRemoved;
        changed = true;
      }

      return changed ? { ...prev, added, removed } : prev;
    });
  }, [initial, serverByName]);

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
    // Defensive: drop any staged adds that overlap a server name (the effect
    // above prunes these, but during the same render we might still see them).
    const safeAdded = staged.added.filter((d) => !serverByName.has(d.modelName));
    return [...surviving, ...safeAdded];
  }, [initial, staged, serverByName]);

  const removedModelNames = useMemo(() => {
    const list: string[] = [];
    for (const name of staged.removed) {
      if (serverByName.has(name)) list.push(name);
    }
    return list;
  }, [staged.removed, serverByName]);

  const addedModelNames = useMemo(
    () =>
      staged.added
        .filter((d) => !serverByName.has(d.modelName))
        .map((d) => d.modelName),
    [staged.added, serverByName],
  );

  // Alias edits = staged.aliases entries that target an existing, non-removed
  // server deployment AND differ from the server value.
  const aliasEditsCount = useMemo(() => {
    let n = 0;
    for (const [name, alias] of Object.entries(staged.aliases)) {
      if (staged.removed.has(name)) continue;
      const original = serverByName.get(name)?.alias;
      if (original !== undefined && alias !== original) n++;
    }
    return n;
  }, [staged.aliases, staged.removed, serverByName]);

  const changeCount =
    addedModelNames.length + removedModelNames.length + aliasEditsCount;
  const hasChanges = changeCount > 0;

  return {
    deployments,
    removedModelNames,
    addedModelNames,
    changeCount,
    hasChanges,
    addModel,
    removeModel,
    setAlias,
    reset,
  };
}
