import { describe, it, expect } from "vitest";
import {
  formatMemory,
  formatNumber,
  formatTokens,
  formatDuration,
  formatLongDuration,
  formatTimestamp,
  formatBytes,
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
  });

  describe("formatNumber", () => {
    it("should format small numbers as-is", () => {
      expect(formatNumber(0)).toBe("0");
      expect(formatNumber(999)).toBe("999");
    });

    it("should format thousands with k suffix", () => {
      expect(formatNumber(1000)).toBe("1k");
      expect(formatNumber(5500)).toBe("6k");
    });

    it("should format millions with M suffix", () => {
      expect(formatNumber(1000000)).toBe("1.0M");
      expect(formatNumber(2500000)).toBe("2.5M");
    });
  });

  describe("formatTokens", () => {
    it("should format small token counts as-is", () => {
      expect(formatTokens(0)).toBe("0");
      expect(formatTokens(999)).toBe("999");
    });

    it("should format thousands with k suffix", () => {
      expect(formatTokens(1000)).toBe("1k");
      expect(formatTokens(5000)).toBe("5k");
    });

    it("should format millions with M suffix", () => {
      expect(formatTokens(1000000)).toBe("1M");
      expect(formatTokens(2500000)).toBe("3M");
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
  });

  describe("formatTimestamp", () => {
    it("should format a timestamp to locale string", () => {
      const timestamp = "2023-01-01T00:00:00Z";
      const result = formatTimestamp(timestamp);
      expect(result).toBeTruthy();
      expect(typeof result).toBe("string");
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
      expect(formatBytes(1024)).toBe("1 KB");
      expect(formatBytes(2048)).toBe("2 KB");
    });

    it("should format megabytes", () => {
      expect(formatBytes(1048576)).toBe("1 MB");
    });

    it("should format gigabytes", () => {
      expect(formatBytes(1073741824)).toBe("1 GB");
    });
  });
});
