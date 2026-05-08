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
  /** Remove a model deployment. */
  removeModel: (modelName: string) => void;
  /** Restore a previously-removed model deployment (used by undo toast). */
  undoRemove: (modelName: string) => void;
  /** Update the alias for a deployment. */
  setAlias: (modelName: string, alias: string) => void;
  /** Discard all staged changes and revert to server state. */
  reset: () => void;
}

interface InitialDeployment {
  modelName: string;
  alias: string;
}

/**
 * Tracks the staged state of model deployments on an endpoint while a user
 * edits in the modal. Encapsulates the diff between server state (what's
 * deployed now) and user state (what they want after Save).
 */
export function useEndpointModelsState(
  initial: InitialDeployment[],
): EndpointModelsState {
  const [serverDeployments] = useState<InitialDeployment[]>(() => [...initial]);
  const [staged, setStaged] = useState<{
    aliases: Record<string, string>; // modelName -> new alias
    added: ImportedDeployment[];
    removed: Set<string>;
  }>({ aliases: {}, added: [], removed: new Set() });

  const initialAliasFor = useCallback(
    (modelName: string): string | undefined =>
      serverDeployments.find((d) => d.modelName === modelName)?.alias,
    [serverDeployments],
  );

  const addModel = useCallback(
    (modelName: string, defaultAlias?: string) => {
      setStaged((prev) => {
        // Restoring a previously-removed deployment
        if (prev.removed.has(modelName)) {
          const next = new Set(prev.removed);
          next.delete(modelName);
          return { ...prev, removed: next };
        }
        // Already in server state or already added — no-op
        const inServer = serverDeployments.some((d) => d.modelName === modelName);
        const inAdded = prev.added.some((d) => d.modelName === modelName);
        if (inServer || inAdded) return prev;

        return {
          ...prev,
          added: [
            ...prev.added,
            { modelName, alias: defaultAlias ?? modelName, isNew: true },
          ],
        };
      });
    },
    [serverDeployments],
  );

  const removeModel = useCallback((modelName: string) => {
    setStaged((prev) => {
      // If it was added in this session, just drop it from added.
      if (prev.added.some((d) => d.modelName === modelName)) {
        return {
          ...prev,
          added: prev.added.filter((d) => d.modelName !== modelName),
        };
      }
      // Otherwise mark as removed.
      const next = new Set(prev.removed);
      next.add(modelName);
      const aliases = { ...prev.aliases };
      delete aliases[modelName];
      return { ...prev, removed: next, aliases };
    });
  }, []);

  const undoRemove = useCallback((modelName: string) => {
    setStaged((prev) => {
      if (!prev.removed.has(modelName)) return prev;
      const next = new Set(prev.removed);
      next.delete(modelName);
      return { ...prev, removed: next };
    });
  }, []);

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
        const original = initialAliasFor(modelName);
        if (original !== undefined && alias === original) {
          const next = { ...prev.aliases };
          delete next[modelName];
          return { ...prev, aliases: next };
        }
        return { ...prev, aliases: { ...prev.aliases, [modelName]: alias } };
      });
    },
    [initialAliasFor],
  );

  const reset = useCallback(() => {
    setStaged({ aliases: {}, added: [], removed: new Set() });
  }, []);

  const deployments = useMemo<ImportedDeployment[]>(() => {
    const surviving = serverDeployments
      .filter((d) => !staged.removed.has(d.modelName))
      .map((d) => ({
        modelName: d.modelName,
        alias: staged.aliases[d.modelName] ?? d.alias,
        isNew: false,
      }));
    return [...surviving, ...staged.added];
  }, [serverDeployments, staged]);

  const removedModelNames = useMemo(
    () => Array.from(staged.removed),
    [staged.removed],
  );
  const addedModelNames = useMemo(
    () => staged.added.map((d) => d.modelName),
    [staged.added],
  );

  const changeCount =
    staged.added.length + staged.removed.size + Object.keys(staged.aliases).length;
  const hasChanges = changeCount > 0;

  return {
    deployments,
    removedModelNames,
    addedModelNames,
    changeCount,
    hasChanges,
    addModel,
    removeModel,
    undoRemove,
    setAlias,
    reset,
  };
}
