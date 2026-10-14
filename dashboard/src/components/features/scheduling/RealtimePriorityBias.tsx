import { Timer } from "lucide-react";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { InfoTip } from "@/components/ui/info-tip";
import {
  MAX_PRIORITY_BIAS_SECONDS,
  formatBiasDuration,
  parsePriorityBias,
} from "@/utils/priorityBias";

interface RealtimePriorityBiasProps {
  /** The raw input text; empty means no bias. */
  value: string;
  onChange: (next: string) => void;
  /** Whether the viewer may change it (platform managers only). */
  canEdit: boolean;
  id?: string;
}

/**
 * Platform-manager-only control for an account's realtime priority bias: how
 * many seconds earlier this account's realtime requests are placed in the
 * deadline-ordered queue on dynamo endpoints. A per-model overlay value from
 * the organisation catalog replaces it on that model.
 */
export function RealtimePriorityBias({
  value,
  onChange,
  canEdit,
  id = "priority-bias-seconds",
}: RealtimePriorityBiasProps) {
  const parsed = parsePriorityBias(value);
  const hintId = `${id}-hint`;

  return (
    <div className="grid gap-2" data-testid="realtime-priority-bias">
      <div className="flex items-start gap-2">
        <Timer className="mt-0.5 h-4 w-4 text-gray-500" aria-hidden="true" />
        <div className="grid gap-1">
          <div className="flex items-center gap-1">
            <Label htmlFor={id} className="text-sm font-medium text-gray-700">
              Realtime priority head start
            </Label>
            <InfoTip>
              <p className="text-sm">
                Realtime requests to self-hosted (dynamo) endpoints are served
                in order of arrival time, alongside batch work ordered by its
                deadline. A head start treats this account's realtime requests
                as if they arrived this many seconds earlier. A per-model value
                in the organisation catalog replaces it on that model. Has no
                effect unless the deployment enables realtime priority.
              </p>
            </InfoTip>
          </div>
          <p className="text-xs text-gray-500">
            Seconds, 0 to {MAX_PRIORITY_BIAS_SECONDS}. Leave empty for none.
          </p>
        </div>
      </div>
      <div className="flex items-center gap-2">
        <Input
          id={id}
          type="number"
          inputMode="numeric"
          min={0}
          max={MAX_PRIORITY_BIAS_SECONDS}
          step={1}
          placeholder="None"
          value={value}
          onChange={(e) => onChange(e.target.value)}
          disabled={!canEdit}
          aria-invalid={!parsed.ok}
          aria-describedby={hintId}
          className="w-40"
        />
        <span className="text-sm text-gray-500">seconds</span>
      </div>
      {parsed.ok ? (
        <p id={hintId} className="text-xs text-gray-500">
          {parsed.value
            ? `Served ahead of standard realtime requests that arrived up to ${formatBiasDuration(parsed.value)} earlier.`
            : "No head start: ordered by arrival time."}
        </p>
      ) : (
        <p id={hintId} className="text-xs text-red-600">
          {parsed.error}
        </p>
      )}
    </div>
  );
}
