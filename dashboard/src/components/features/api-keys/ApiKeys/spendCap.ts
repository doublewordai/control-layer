import type { SpendLimitInterval } from "../../../../api/control-layer/types";
import { formatDollars } from "../../../../utils/money";

// Spending-cap display helpers.
//
// Cap windows are CALENDAR-ALIGNED (UTC), never rolling: daily resets at UTC
// midnight, weekly at the ISO week boundary (Monday 00:00 UTC), monthly on the
// 1st at 00:00 UTC. The preview math below MUST mirror the backend's
// date_trunc-based helpers (migration 123) so the UI promises the same reset
// moment the server enforces.

/** Format a decimal-string credit amount for display, e.g. "12.5" -> "$12.50". */
export function formatCredits(value: string | null | undefined): string {
  if (value === null || value === undefined || value === "") return "—";
  const n = Number(value);
  if (!Number.isFinite(n)) return "—";
  // Shared money formatter (locale/grouping-consistent); extend precision for
  // sub-cent amounts so tiny spend is visible rather than rounding to $0.00.
  const maxDecimalPlaces = n !== 0 && Math.abs(n) < 0.01 ? 4 : 2;
  return formatDollars(n, maxDecimalPlaces);
}

/**
 * The next calendar reset boundary (UTC) for a windowed cap, computed from
 * `now`. Returns null for one-off caps (no automatic reset).
 */
export function nextResetBoundary(
  interval: SpendLimitInterval | null | undefined,
  now: Date = new Date(),
): Date | null {
  if (!interval) return null;
  const y = now.getUTCFullYear();
  const m = now.getUTCMonth();
  const d = now.getUTCDate();
  switch (interval) {
    case "daily":
      return new Date(Date.UTC(y, m, d + 1));
    case "weekly": {
      // ISO week starts Monday. getUTCDay(): Sun=0..Sat=6.
      const dow = now.getUTCDay();
      const daysUntilMonday = dow === 0 ? 1 : 8 - dow;
      return new Date(Date.UTC(y, m, d + daysUntilMonday));
    }
    case "monthly":
      return new Date(Date.UTC(y, m + 1, 1));
  }
}

/** Display a reset instant, e.g. "Aug 1, 2026, 00:00 UTC". */
export function formatResetInstant(date: Date | string): string {
  const d = typeof date === "string" ? new Date(date) : date;
  const day = d.toLocaleDateString("en-US", {
    year: "numeric",
    month: "short",
    day: "numeric",
    timeZone: "UTC",
  });
  const time = d.toLocaleTimeString("en-US", {
    hour: "2-digit",
    minute: "2-digit",
    // h23 pins midnight to "00:00" — bare hour12:false can render "24:00"
    // in some Intl implementations.
    hourCycle: "h23",
    timeZone: "UTC",
  });
  return `${day}, ${time} UTC`;
}

/**
 * The preview line under the interval picker: makes it unmistakable that
 * windows are static calendar periods, not rolling ones.
 */
export function resetPreviewLine(
  interval: SpendLimitInterval | null | undefined,
  now: Date = new Date(),
): string {
  const boundary = nextResetBoundary(interval, now);
  if (!boundary) {
    return "No automatic reset — the cap counts all spend from when it is set.";
  }
  return `Next resets ${formatResetInstant(boundary)}.`;
}

/** Table subtext for a capped key's reset period, e.g. "Daily limit". */
export function limitPeriodLabel(
  interval: SpendLimitInterval | null | undefined,
): string {
  switch (interval) {
    case "daily":
      return "Daily limit";
    case "weekly":
      return "Weekly limit";
    case "monthly":
      return "Monthly limit";
    default:
      return "One-off limit";
  }
}
