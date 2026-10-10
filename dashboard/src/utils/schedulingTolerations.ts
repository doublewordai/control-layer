import type {
  PinnedTolerations,
  SchedulingToleration,
} from "@/api/control-layer/types";

/**
 * Helpers for the "Scheduling restrictions" controls. The pinned toleration
 * list is opaque JSON to most of the dashboard, so the JSON editor needs a
 * parse + validation path that mirrors the server's, and the toggle needs a
 * way to tell "not pinned" (null/undefined) apart from "pinned to the empty
 * list" (dedicated capacity).
 */

/** The three states a pin can be in, as the controls model them. */
export type PinState =
  /** No pin: `null`/absent. The account is unchanged. */
  | { kind: "unpinned" }
  /** Pinned to `[]`: keep the account's work off tainted capacity. */
  | { kind: "dedicated" }
  /** Pinned to a non-empty list: the advanced editor. */
  | { kind: "custom"; tolerations: SchedulingToleration[] };

/** Classify a stored pin into the state the controls render. */
export function pinState(pinned: PinnedTolerations | null | undefined): PinState {
  if (pinned == null) return { kind: "unpinned" };
  if (pinned.length === 0) return { kind: "dedicated" };
  return { kind: "custom", tolerations: pinned };
}

/** Whether the account is pinned at all (empty list counts). */
export function isPinned(pinned: PinnedTolerations | null | undefined): boolean {
  return pinned != null;
}

/** A human label for the badge: `null` when the account is not pinned. */
export function pinBadgeLabel(
  pinned: PinnedTolerations | null | undefined,
): string | null {
  if (pinned == null) return null;
  if (pinned.length === 0) return "Dedicated capacity";
  return "Custom scheduling";
}

/**
 * Parse the JSON editor's text into a validated list, or return the error to
 * show inline. The rules mirror `dwctl::scheduling::PinnedTolerations`: a
 * top-level array; each entry an object with a `key`, an optional `operator`
 * (`Equal`/`Exists`), an optional `value`, and an optional `effect`
 * (`NoSchedule`/`PreferNoSchedule`); `Equal` (the default) needs a value and
 * `Exists` must not carry one; unknown fields and spellings are refused.
 */
export type ParseResult =
  | { ok: true; tolerations: PinnedTolerations }
  | { ok: false; error: string };

const OPERATORS = new Set(["Equal", "Exists"]);
const EFFECTS = new Set(["NoSchedule", "PreferNoSchedule"]);

export function parseTolerations(text: string): ParseResult {
  const trimmed = text.trim();
  if (trimmed === "") {
    return { ok: false, error: "Enter a JSON array of tolerations, or []." };
  }

  let parsed: unknown;
  try {
    parsed = JSON.parse(trimmed);
  } catch {
    return { ok: false, error: "That is not valid JSON." };
  }

  if (!Array.isArray(parsed)) {
    return { ok: false, error: "The list must be a JSON array." };
  }

  const tolerations: SchedulingToleration[] = [];
  for (let i = 0; i < parsed.length; i++) {
    const entry = parsed[i];
    if (entry === null || typeof entry !== "object" || Array.isArray(entry)) {
      return { ok: false, error: `Entry ${i} must be an object.` };
    }
    const record = entry as Record<string, unknown>;
    const allowed = new Set(["key", "operator", "value", "effect"]);
    for (const field of Object.keys(record)) {
      if (!allowed.has(field)) {
        return { ok: false, error: `Entry ${i}: unknown field "${field}".` };
      }
    }

    const key = record.key;
    if (typeof key !== "string" || key.length === 0) {
      return { ok: false, error: `Entry ${i}: "key" must be a non-empty string.` };
    }
    const operator = record.operator;
    if (operator !== undefined && (typeof operator !== "string" || !OPERATORS.has(operator))) {
      return { ok: false, error: `Entry ${i}: "operator" must be Equal or Exists.` };
    }
    const value = record.value;
    if (value !== undefined && typeof value !== "string") {
      return { ok: false, error: `Entry ${i}: "value" must be a string.` };
    }
    const effect = record.effect;
    if (effect !== undefined && (typeof effect !== "string" || !EFFECTS.has(effect))) {
      return {
        ok: false,
        error: `Entry ${i}: "effect" must be NoSchedule or PreferNoSchedule.`,
      };
    }

    const resolvedOperator = (operator as string | undefined) ?? "Equal";
    if (resolvedOperator === "Equal" && value === undefined) {
      return {
        ok: false,
        error: `Entry ${i}: operator Equal needs a value (or set operator: Exists).`,
      };
    }
    if (resolvedOperator === "Exists" && value !== undefined) {
      return { ok: false, error: `Entry ${i}: operator Exists must not carry a value.` };
    }

    tolerations.push({
      key,
      ...(operator !== undefined
        ? { operator: operator as SchedulingToleration["operator"] }
        : {}),
      ...(value !== undefined ? { value: value as string } : {}),
      ...(effect !== undefined
        ? { effect: effect as SchedulingToleration["effect"] }
        : {}),
    });
  }

  return { ok: true, tolerations };
}

/** Pretty-print a pin for the JSON editor. */
export function formatTolerations(tolerations: PinnedTolerations): string {
  return JSON.stringify(tolerations, null, 2);
}
