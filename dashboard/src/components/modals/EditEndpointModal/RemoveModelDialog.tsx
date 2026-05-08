import React from "react";
import { AlertTriangle } from "lucide-react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "../../ui/dialog";
import { Button } from "../../ui/button";
import type { DeploymentReferences } from "./references";

export interface RemoveModelDialogProps {
  /** Provider model name being removed (or null when the dialog is closed) */
  modelName: string | null;
  /** References to surface to the user as a warning */
  references: DeploymentReferences;
  /** Called when the user confirms removal */
  onConfirm: () => void;
  /** Called when the user cancels */
  onCancel: () => void;
}

export const RemoveModelDialog: React.FC<RemoveModelDialogProps> = ({
  modelName,
  references,
  onConfirm,
  onCancel,
}) => {
  const open = modelName !== null;

  return (
    <Dialog open={open} onOpenChange={(next) => !next && onCancel()}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <AlertTriangle className="w-4 h-4 text-amber-600" aria-hidden />
            Remove {modelName}?
          </DialogTitle>
          <DialogDescription>
            This deployment is referenced elsewhere. Removing it from the
            endpoint will break the resources below — API requests targeting
            them will fail until you update each one's configuration.
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-4 py-2">
          {references.directHosted.length > 0 && (
            <ReferenceSection
              label={
                references.directHosted.length === 1
                  ? "1 hosted model wraps this deployment"
                  : `${references.directHosted.length} hosted models wrap this deployment`
              }
              note="will become unreachable"
              entries={references.directHosted.map((r) => ({
                key: `direct-${r.modelId}`,
                title: r.modelAlias,
                meta: "standard",
              }))}
            />
          )}

          {references.virtualModels.length > 0 && (
            <ReferenceSection
              label={
                references.virtualModels.length === 1
                  ? "1 virtual model includes this as a component"
                  : `${references.virtualModels.length} virtual models include this as a component`
              }
              note="component will be dropped"
              entries={references.virtualModels.map((r) => ({
                key: `virt-${r.modelId}`,
                title: r.modelAlias,
                meta: "virtual",
              }))}
            />
          )}

          {references.trafficRules.length > 0 && (
            <ReferenceSection
              label={
                references.trafficRules.length === 1
                  ? "1 traffic rule redirects to this deployment"
                  : `${references.trafficRules.length} traffic rules redirect to this deployment`
              }
              note="rule target will be invalidated"
              entries={references.trafficRules.map((r, idx) => ({
                key: `rule-${r.modelId}-${idx}`,
                title: r.modelAlias,
                meta: `${r.rule.api_key_purpose} purpose`,
              }))}
            />
          )}
        </div>

        <DialogFooter>
          <Button type="button" variant="outline" onClick={onCancel}>
            Cancel
          </Button>
          <Button type="button" variant="destructive" onClick={onConfirm}>
            Remove anyway
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};

interface ReferenceSectionProps {
  label: string;
  note: string;
  entries: { key: string; title: string; meta: string }[];
}

const ReferenceSection: React.FC<ReferenceSectionProps> = ({ label, note, entries }) => (
  <div>
    <div className="flex items-baseline justify-between mb-1.5">
      <p className="text-xs font-medium text-gray-700 uppercase tracking-wide">
        {label}
      </p>
      <p className="text-xs text-amber-700">{note}</p>
    </div>
    <ul className="rounded-md border border-gray-200 divide-y divide-gray-100 bg-white">
      {entries.map((entry) => (
        <li
          key={entry.key}
          className="flex items-center justify-between px-3 py-2 text-sm"
        >
          <span className="text-gray-800 truncate">{entry.title}</span>
          <span className="text-xs text-gray-500 shrink-0 ml-2">{entry.meta}</span>
        </li>
      ))}
    </ul>
  </div>
);
