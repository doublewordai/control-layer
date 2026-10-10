import { useState } from "react";
import { ShieldCheck } from "lucide-react";
import type { PinnedTolerations } from "@/api/control-layer/types";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { Textarea } from "@/components/ui/textarea";
import {
  formatTolerations,
  parseTolerations,
  pinState,
} from "@/utils/schedulingTolerations";

interface SchedulingRestrictionsProps {
  /**
   * The account's current pinned tolerations: `undefined` when the field was
   * absent (older servers), `null` when not pinned, `[]` when kept on
   * dedicated capacity, or a non-empty list when custom.
   */
  pinned: PinnedTolerations | null | undefined;
  /** Called with the next pin whenever the operator changes it. */
  onChange: (next: PinnedTolerations | null) => void;
  /** Whether the viewer may change the pin (platform managers only). */
  canEdit: boolean;
}

/**
 * Platform-manager-only controls for an account's pinned scheduling
 * tolerations. The primary control is a single toggle between "not pinned"
 * and "dedicated capacity" (`[]`); an Advanced disclosure opens a JSON editor
 * for an arbitrary list, validated inline with the same rules the server
 * applies. Wording is deliberately backend-agnostic: it speaks of dedicated
 * capacity and scheduling tolerations, not of any particular serving stack.
 */
export function SchedulingRestrictions({
  pinned,
  onChange,
  canEdit,
}: SchedulingRestrictionsProps) {
  const state = pinState(pinned);
  const [showAdvanced, setShowAdvanced] = useState(state.kind === "custom");
  // Seed the editor from the current custom list, or a sensible empty-array
  // starting point when the operator opens Advanced from a non-custom state.
  const [json, setJson] = useState(() =>
    state.kind === "custom" ? formatTolerations(state.tolerations) : "[]",
  );
  const [error, setError] = useState<string | null>(null);

  const dedicated = state.kind === "dedicated";

  const toggleDedicated = (checked: boolean) => {
    if (checked) {
      // "Keep on dedicated capacity" is the empty-list pin.
      setError(null);
      setShowAdvanced(false);
      onChange([]);
    } else {
      // Off: no pin at all.
      setError(null);
      setShowAdvanced(false);
      onChange(null);
    }
  };

  const applyAdvanced = (text: string) => {
    setJson(text);
    const result = parseTolerations(text);
    if (!result.ok) {
      setError(result.error);
      return;
    }
    setError(null);
    onChange(result.tolerations);
  };

  return (
    <div
      className="space-y-3 border-t pt-4"
      data-testid="scheduling-restrictions"
    >
      <div className="flex items-start justify-between gap-4">
        <div className="flex items-start gap-2">
          <ShieldCheck className="mt-0.5 h-4 w-4 text-gray-500" />
          <div className="grid gap-1">
            <Label
              htmlFor="keep-dedicated-capacity"
              className="text-sm font-medium text-gray-700"
            >
              Scheduling restrictions
            </Label>
            <p className="text-xs text-gray-500">
              Keep this account's requests on dedicated capacity. When on,
              every inference request from this account is pinned to an empty
              toleration list, so it never lands on shared or interrupted
              capacity.
            </p>
          </div>
        </div>
        <Switch
          id="keep-dedicated-capacity"
          checked={dedicated}
          onCheckedChange={toggleDedicated}
          disabled={!canEdit}
          aria-label="Keep this account's requests on dedicated capacity"
        />
      </div>

      {canEdit && (
        <div>
          <button
            type="button"
            className="text-xs font-medium text-doubleword-primary hover:underline"
            onClick={() => {
              setShowAdvanced((prev) => !prev);
              setError(null);
            }}
            aria-expanded={showAdvanced}
          >
            {showAdvanced ? "Hide advanced" : "Advanced: edit tolerations"}
          </button>

          {showAdvanced && (
            <div className="mt-2 space-y-2">
              <Textarea
                aria-label="Scheduling tolerations JSON"
                value={json}
                onChange={(e) => applyAdvanced(e.target.value)}
                rows={6}
                className="font-mono text-xs"
                spellCheck={false}
              />
              {error ? (
                <p className="text-xs text-red-600" role="alert">
                  {error}
                </p>
              ) : (
                <p className="text-xs text-gray-500">
                  A JSON array of tolerations. Each entry needs a{" "}
                  <code>key</code>, and may set <code>operator</code> (
                  <code>Equal</code> or <code>Exists</code>), <code>value</code>{" "}
                  and <code>effect</code> (
                  <code>NoSchedule</code> or <code>PreferNoSchedule</code>). An
                  empty list keeps the account on dedicated capacity.
                </p>
              )}
            </div>
          )}
        </div>
      )}
    </div>
  );
}
