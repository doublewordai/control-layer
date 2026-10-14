/** Largest realtime priority bias an account or overlay may carry (one day). */
export const MAX_PRIORITY_BIAS_SECONDS = 86_400;

export type ParsedPriorityBias =
  { ok: true; value: number | null } | { ok: false; error: string };

/**
 * Parse the priority-bias input. Empty means "no bias" (`null`, which clears
 * the setting); otherwise a whole number of seconds from 0 to one day.
 */
export function parsePriorityBias(input: string): ParsedPriorityBias {
  const trimmed = input.trim();
  if (trimmed === "") return { ok: true, value: null };
  if (!/^\d+$/.test(trimmed)) {
    return { ok: false, error: "Enter a whole number of seconds." };
  }
  const value = Number(trimmed);
  if (value > MAX_PRIORITY_BIAS_SECONDS) {
    return {
      ok: false,
      error: `The maximum is ${MAX_PRIORITY_BIAS_SECONDS} seconds (one day).`,
    };
  }
  return { ok: true, value };
}

/** A short human duration for a bias in seconds, e.g. "15 min", "1 h 30 min". */
export function formatBiasDuration(seconds: number): string {
  if (seconds < 60) return `${seconds} s`;
  const hours = Math.floor(seconds / 3600);
  const minutes = Math.floor((seconds % 3600) / 60);
  const rest = seconds % 60;
  const parts: string[] = [];
  if (hours) parts.push(`${hours} h`);
  if (minutes) parts.push(`${minutes} min`);
  if (rest) parts.push(`${rest} s`);
  return parts.join(" ");
}
