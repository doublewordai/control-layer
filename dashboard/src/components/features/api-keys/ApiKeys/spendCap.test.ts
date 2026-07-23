import { describe, it, expect } from "vitest";
import {
  formatCredits,
  formatResetInstant,
  nextResetBoundary,
  resetPreviewLine,
} from "./spendCap";

describe("spendCap helpers", () => {
  describe("formatCredits", () => {
    it("formats decimal strings as dollars", () => {
      expect(formatCredits("12.5")).toBe("$12.50");
      expect(formatCredits("50")).toBe("$50.00");
      expect(formatCredits("0")).toBe("$0.00");
    });

    it("extends precision for sub-cent amounts so tiny spend is visible", () => {
      expect(formatCredits("0.0001")).toBe("$0.0001");
    });

    it("dashes for missing or invalid values", () => {
      expect(formatCredits(null)).toBe("—");
      expect(formatCredits(undefined)).toBe("—");
      expect(formatCredits("")).toBe("—");
      expect(formatCredits("not-a-number")).toBe("—");
    });
  });

  describe("nextResetBoundary (calendar-aligned UTC — mirrors migration 123)", () => {
    // Wednesday 2026-07-22 15:30 UTC
    const midWeek = new Date(Date.UTC(2026, 6, 22, 15, 30));

    it("daily: next UTC midnight", () => {
      expect(nextResetBoundary("daily", midWeek)?.toISOString()).toBe(
        "2026-07-23T00:00:00.000Z",
      );
    });

    it("weekly: next Monday 00:00 UTC (ISO week)", () => {
      expect(nextResetBoundary("weekly", midWeek)?.toISOString()).toBe(
        "2026-07-27T00:00:00.000Z",
      );
      // Sunday belongs to the current ISO week: it resets the NEXT day.
      const sunday = new Date(Date.UTC(2026, 6, 26, 10, 0));
      expect(nextResetBoundary("weekly", sunday)?.toISOString()).toBe(
        "2026-07-27T00:00:00.000Z",
      );
    });

    it("monthly: first of next month 00:00 UTC, across year end", () => {
      expect(nextResetBoundary("monthly", midWeek)?.toISOString()).toBe(
        "2026-08-01T00:00:00.000Z",
      );
      const december = new Date(Date.UTC(2026, 11, 31, 23, 59));
      expect(nextResetBoundary("monthly", december)?.toISOString()).toBe(
        "2027-01-01T00:00:00.000Z",
      );
    });

    it("one-off caps have no boundary", () => {
      expect(nextResetBoundary(null, midWeek)).toBeNull();
      expect(nextResetBoundary(undefined, midWeek)).toBeNull();
    });
  });

  describe("preview copy", () => {
    it("makes calendar (non-rolling) semantics explicit", () => {
      const midWeek = new Date(Date.UTC(2026, 6, 22, 15, 30));
      expect(resetPreviewLine("daily", midWeek)).toBe(
        "Next resets Jul 23, 2026, 00:00 UTC.",
      );
      expect(resetPreviewLine(null, midWeek)).toMatch(/no automatic reset/i);
    });

    it("renders instants in UTC regardless of local timezone", () => {
      expect(formatResetInstant("2026-08-01T00:00:00Z")).toBe(
        "Aug 1, 2026, 00:00 UTC",
      );
    });
  });
});
