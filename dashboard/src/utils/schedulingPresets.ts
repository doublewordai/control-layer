import type { SchedulingTolerationPreset } from "@/api/control-layer/types";

/**
 * The sentinel the account settings select uses for "Default": no preset id
 * is stored, so nothing is pinned and the backend decides placement. Radix
 * Select forbids an empty-string item value, hence a sentinel.
 */
export const DEFAULT_TOLERATION_PRESET = "__default__";

/**
 * The badge label for an account pinned to `preset`, or `null` when it is on
 * Default (nothing to show). When the id is no longer in the deployment's
 * configured presets, the raw id is shown so the operator can see the account
 * references a preset that config no longer defines (which fails closed at
 * runtime).
 */
export function presetBadgeLabel(
  preset: string | null | undefined,
  presets: SchedulingTolerationPreset[] | undefined,
): string | null {
  if (preset == null) return null;
  const match = presets?.find((p) => p.id === preset);
  return match?.label ?? preset;
}
