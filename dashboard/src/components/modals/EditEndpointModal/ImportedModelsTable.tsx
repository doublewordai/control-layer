import React from "react";
import { X } from "lucide-react";
import { Input } from "../../ui/input";
import { Button } from "../../ui/button";
import type { ImportedDeployment } from "./useEndpointModelsState";
import type { DeploymentReferences } from "./references";

export interface ImportedModelsTableProps {
  deployments: ImportedDeployment[];
  /** modelName -> references; pass an empty entry for unreferenced deployments */
  referencesByModelName: Map<string, DeploymentReferences>;
  /** Aliases that conflict (case-sensitive) within the current edit session. */
  conflictingAliases: Set<string>;
  onAliasChange: (modelName: string, alias: string) => void;
  onRemove: (modelName: string) => void;
}

export const ImportedModelsTable: React.FC<ImportedModelsTableProps> = ({
  deployments,
  referencesByModelName,
  conflictingAliases,
  onAliasChange,
  onRemove,
}) => {
  if (deployments.length === 0) {
    return (
      <div className="border border-dashed rounded-lg py-10 text-center text-sm text-gray-500">
        No models imported yet. Click <span className="font-medium">Add model</span> to
        get started.
      </div>
    );
  }

  return (
    <div className="border rounded-lg overflow-hidden" role="table" aria-label="Imported models">
      <div
        className="grid grid-cols-12 gap-2 px-3 py-2 bg-gray-50 border-b text-xs font-medium text-gray-600 uppercase tracking-wide"
        role="row"
      >
        <div className="col-span-5" role="columnheader">Model</div>
        <div className="col-span-4" role="columnheader">Alias</div>
        <div className="col-span-2" role="columnheader">References</div>
        <div className="col-span-1" role="columnheader" aria-label="Actions" />
      </div>
      <ul className="divide-y" role="rowgroup">
        {deployments.map((d) => (
          <ImportedModelRow
            key={d.modelName}
            deployment={d}
            references={referencesByModelName.get(d.modelName)}
            isAliasConflict={conflictingAliases.has(d.alias)}
            onAliasChange={onAliasChange}
            onRemove={onRemove}
          />
        ))}
      </ul>
    </div>
  );
};

interface ImportedModelRowProps {
  deployment: ImportedDeployment;
  references: DeploymentReferences | undefined;
  isAliasConflict: boolean;
  onAliasChange: (modelName: string, alias: string) => void;
  onRemove: (modelName: string) => void;
}

const ImportedModelRow: React.FC<ImportedModelRowProps> = ({
  deployment,
  references,
  isAliasConflict,
  onAliasChange,
  onRemove,
}) => {
  const hostedCount = references
    ? references.directHosted.length + references.virtualModels.length
    : 0;
  const ruleCount = references?.trafficRules.length ?? 0;

  return (
    <li
      className="group grid grid-cols-12 gap-2 px-3 py-2 items-center hover:bg-gray-50 transition-colors"
      role="row"
    >
      <div className="col-span-5 min-w-0 flex items-center gap-2" role="cell">
        <span className="font-mono text-sm truncate" title={deployment.modelName}>
          {deployment.modelName}
        </span>
        {deployment.isNew && (
          <span className="text-[10px] uppercase tracking-wide px-1.5 py-0.5 rounded bg-emerald-50 text-emerald-700 border border-emerald-200">
            new
          </span>
        )}
      </div>

      <div className="col-span-4 min-w-0" role="cell">
        <Input
          value={deployment.alias}
          onChange={(e) => onAliasChange(deployment.modelName, e.target.value)}
          className={
            "h-8 text-sm font-mono bg-transparent border-transparent hover:border-input focus:border-input focus:bg-white" +
            (isAliasConflict ? " border-red-400 hover:border-red-400" : "")
          }
          placeholder={deployment.modelName}
          aria-label={`Alias for ${deployment.modelName}`}
        />
      </div>

      <div className="col-span-2 text-xs flex items-center gap-1.5 flex-wrap" role="cell">
        {hostedCount === 0 && ruleCount === 0 ? (
          <span className="text-gray-400">—</span>
        ) : (
          <>
            {hostedCount > 0 && (
              <ReferenceBadge
                label={`${hostedCount} model${hostedCount === 1 ? "" : "s"}`}
                tone="warn"
              />
            )}
            {ruleCount > 0 && (
              <ReferenceBadge
                label={`${ruleCount} rule${ruleCount === 1 ? "" : "s"}`}
                tone="warn"
              />
            )}
          </>
        )}
      </div>

      <div className="col-span-1 flex justify-end" role="cell">
        <Button
          type="button"
          size="icon"
          variant="ghost"
          className="h-7 w-7 opacity-0 group-hover:opacity-100 focus:opacity-100 text-gray-400 hover:text-red-600 hover:bg-red-50 transition-opacity"
          aria-label={`Remove ${deployment.modelName}`}
          onClick={() => onRemove(deployment.modelName)}
        >
          <X className="h-4 w-4" />
        </Button>
      </div>
    </li>
  );
};

const ReferenceBadge: React.FC<{ label: string; tone: "warn" | "neutral" }> = ({
  label,
  tone,
}) => (
  <span
    className={
      tone === "warn"
        ? "px-1.5 py-0.5 rounded bg-amber-50 text-amber-700 border border-amber-200 text-[10px] font-medium"
        : "px-1.5 py-0.5 rounded bg-gray-50 text-gray-600 border border-gray-200 text-[10px]"
    }
  >
    {label}
  </span>
);

export type { ImportedDeployment };
