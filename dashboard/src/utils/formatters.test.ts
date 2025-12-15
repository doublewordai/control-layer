import { describe, it, expect } from "vitest";
import {
  formatMemory,
  formatNumber,
  formatTokens,
  formatDuration,
  formatLongDuration,
  formatTimestamp,
  formatBytes,
  formatLatency,
  formatRelativeTime,
  formatTariffPrice,
  formatTariffPricing,
} from "./formatters";

describe("formatters", () => {
  describe("formatMemory", () => {
    it("should format memory in GB", () => {
      expect(formatMemory(8)).toBe("8GB");
      expect(formatMemory(100)).toBe("100GB");
    });

    it("should format memory in TB when >= 1000GB", () => {
      expect(formatMemory(1000)).toBe("1.0TB");
      expect(formatMemory(2500)).toBe("2.5TB");
    });

    it("should handle invalid numbers", () => {
      expect(formatMemory(NaN)).toBe("N/A");
      expect(formatMemory(Infinity)).toBe("N/A");
      expect(formatMemory(-100)).toBe("0GB");
    });
  });

  describe("formatNumber", () => {
    it("should format small numbers as-is", () => {
      expect(formatNumber(0)).toBe("0");
      expect(formatNumber(999)).toBe("999");
    });

    it("should format thousands with K suffix", () => {
      expect(formatNumber(1000)).toBe("1.0K");
      expect(formatNumber(5500)).toBe("5.5K");
    });

    it("should format millions with M suffix", () => {
      expect(formatNumber(1000000)).toBe("1.0M");
      expect(formatNumber(2500000)).toBe("2.5M");
    });

    it("should format billions with B suffix", () => {
      expect(formatNumber(1000000000)).toBe("1.0B");
      expect(formatNumber(2500000000)).toBe("2.5B");
    });

    it("should handle invalid numbers", () => {
      expect(formatNumber(NaN)).toBe("N/A");
      expect(formatNumber(Infinity)).toBe("N/A");
      expect(formatNumber(-Infinity)).toBe("N/A");
      expect(formatNumber(-100)).toBe("0");
    });
  });

  describe("formatTokens", () => {
    it("should format small token counts as-is", () => {
      expect(formatTokens(0)).toBe("0");
      expect(formatTokens(999)).toBe("999");
    });

    it("should format thousands with K suffix", () => {
      expect(formatTokens(1000)).toBe("1.0K");
      expect(formatTokens(5000)).toBe("5.0K");
    });

    it("should format millions with M suffix", () => {
      expect(formatTokens(1000000)).toBe("1.0M");
      expect(formatTokens(2500000)).toBe("2.5M");
    });

    it("should handle invalid numbers", () => {
      expect(formatTokens(NaN)).toBe("N/A");
      expect(formatTokens(Infinity)).toBe("N/A");
      expect(formatTokens(-100)).toBe("0");
    });
  });

  describe("formatDuration", () => {
    it("should format milliseconds", () => {
      expect(formatDuration(500)).toBe("500ms");
    });

    it("should format seconds", () => {
      expect(formatDuration(1500)).toBe("1.5s");
      expect(formatDuration(30000)).toBe("30.0s");
    });

    it("should format minutes and seconds", () => {
      expect(formatDuration(90000)).toBe("1m 30s");
      expect(formatDuration(125000)).toBe("2m 5s");
    });

    it("should handle invalid numbers", () => {
      expect(formatDuration(NaN)).toBe("N/A");
      expect(formatDuration(Infinity)).toBe("N/A");
      expect(formatDuration(-100)).toBe("0ms");
    });
  });

  describe("formatLongDuration", () => {
    it("should format very short durations", () => {
      expect(formatLongDuration(500)).toBe("< 1s");
    });

    it("should format seconds", () => {
      expect(formatLongDuration(5000)).toBe("5s");
    });

    it("should format minutes and seconds", () => {
      expect(formatLongDuration(90000)).toBe("1m 30s");
    });

    it("should format hours and minutes", () => {
      expect(formatLongDuration(3900000)).toBe("1h 5m");
    });

    it("should format days and hours", () => {
      expect(formatLongDuration(90000000)).toBe("1d 1h");
    });

    it("should format exact days", () => {
      expect(formatLongDuration(86400000)).toBe("1d");
    });

    it("should handle invalid numbers", () => {
      expect(formatLongDuration(NaN)).toBe("N/A");
      expect(formatLongDuration(Infinity)).toBe("N/A");
      expect(formatLongDuration(-100)).toBe("0s");
    });
  });

  describe("formatTimestamp", () => {
    it("should format a timestamp to locale string", () => {
      const timestamp = "2023-01-01T00:00:00Z";
      const result = formatTimestamp(timestamp);
      expect(result).toBeTruthy();
      expect(typeof result).toBe("string");
    });

    it("should handle invalid dates", () => {
      expect(formatTimestamp("garbage")).toBe("Invalid date");
      expect(formatTimestamp("not-a-date")).toBe("Invalid date");
    });
  });

  describe("formatBytes", () => {
    it("should format zero bytes", () => {
      expect(formatBytes(0)).toBe("0 Bytes");
    });

    it("should format bytes", () => {
      expect(formatBytes(500)).toBe("500 Bytes");
    });

    it("should format kilobytes", () => {
      expect(formatBytes(1000)).toBe("1 KB");
      expect(formatBytes(2000)).toBe("2 KB");
    });

    it("should format megabytes", () => {
      expect(formatBytes(1000000)).toBe("1 MB");
    });

    it("should format gigabytes", () => {
      expect(formatBytes(1000000000)).toBe("1 GB");
    });

    it("should handle invalid numbers", () => {
      expect(formatBytes(NaN)).toBe("N/A");
      expect(formatBytes(Infinity)).toBe("N/A");
      expect(formatBytes(-100)).toBe("0 Bytes");
    });
  });

  describe("formatLatency", () => {
    it("should return N/A for undefined or null", () => {
      expect(formatLatency(undefined)).toBe("N/A");
      expect(formatLatency(0)).toBe("N/A");
    });

    it("should format milliseconds", () => {
      expect(formatLatency(50)).toBe("50ms");
      expect(formatLatency(999)).toBe("999ms");
    });

    it("should format seconds", () => {
      expect(formatLatency(1000)).toBe("1.0s");
      expect(formatLatency(1500)).toBe("1.5s");
      expect(formatLatency(2345)).toBe("2.3s");
    });
  });

  describe("formatRelativeTime", () => {
    it("should return Never for undefined", () => {
      expect(formatRelativeTime(undefined)).toBe("Never");
    });

    it("should format just now", () => {
      const now = new Date().toISOString();
      expect(formatRelativeTime(now)).toBe("Just now");
    });

    it("should format minutes ago", () => {
      const minutesAgo = new Date(Date.now() - 5 * 60 * 1000).toISOString();
      expect(formatRelativeTime(minutesAgo)).toBe("5m ago");
    });

    it("should format hours ago", () => {
      const hoursAgo = new Date(Date.now() - 3 * 60 * 60 * 1000).toISOString();
      expect(formatRelativeTime(hoursAgo)).toBe("3h ago");
    });

    it("should format days ago", () => {
      const daysAgo = new Date(Date.now() - 2 * 24 * 60 * 60 * 1000).toISOString();
      expect(formatRelativeTime(daysAgo)).toBe("2d ago");
    });

    it("should format older dates as locale string", () => {
      const longAgo = new Date(Date.now() - 10 * 24 * 60 * 60 * 1000).toISOString();
      const result = formatRelativeTime(longAgo);
      expect(result).toBeTruthy();
      expect(result).not.toContain("ago");
    });
  });

  describe("formatTariffPrice", () => {
    it("should return $0 for null/undefined/zero", () => {
      expect(formatTariffPrice(null)).toBe("$0");
      expect(formatTariffPrice(undefined)).toBe("$0");
      expect(formatTariffPrice(0)).toBe("$0");
      expect(formatTariffPrice("0")).toBe("$0");
    });

    it("should convert per-token price to per-million and format", () => {
      // $0.000001 per token = $1.00 per million tokens
      expect(formatTariffPrice("0.000001")).toBe("$1.00");
      expect(formatTariffPrice(0.000001)).toBe("$1.00");

      // $0.000002 per token = $2.00 per million tokens
      expect(formatTariffPrice("0.000002")).toBe("$2.00");

      // $0.0000015 per token = $1.50 per million tokens
      expect(formatTariffPrice("0.0000015")).toBe("$1.50");
    });

    it("should handle typical pricing values", () => {
      // Typical GPT-4 input pricing: $0.00001 per token = $10 per million
      expect(formatTariffPrice("0.00001")).toBe("$10.00");

      // Typical GPT-3.5 input pricing: $0.0000005 per token = $0.50 per million
      expect(formatTariffPrice("0.0000005")).toBe("$0.50");
    });
  });

  describe("formatTariffPricing", () => {
    it("should format both input and output prices", () => {
      expect(formatTariffPricing("0.000001", "0.000002")).toBe("$1.00 / $2.00");
    });

    it("should handle null prices", () => {
      expect(formatTariffPricing(null, null)).toBe("$0 / $0");
      expect(formatTariffPricing("0.000001", null)).toBe("$1.00 / $0");
      expect(formatTariffPricing(null, "0.000002")).toBe("$0 / $2.00");
    });

    it("should handle typical model pricing", () => {
      // GPT-4: $10 input / $30 output per million tokens
      expect(formatTariffPricing("0.00001", "0.00003")).toBe("$10.00 / $30.00");

      // GPT-3.5: $0.50 input / $1.50 output per million tokens
      expect(formatTariffPricing("0.0000005", "0.0000015")).toBe("$0.50 / $1.50");
    });
  });
});
