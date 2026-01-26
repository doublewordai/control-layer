import { describe, it, expect } from "vitest";
import { formatDollars } from "./money";

describe("formatDollars", () => {
  it("formats whole dollars with 2 decimal places", () => {
    expect(formatDollars(100)).toBe("$100.00");
    expect(formatDollars(0)).toBe("$0.00");
    expect(formatDollars(1)).toBe("$1.00");
  });

  it("formats cents correctly", () => {
    expect(formatDollars(99.99)).toBe("$99.99");
    expect(formatDollars(0.01)).toBe("$0.01");
    expect(formatDollars(123.456)).toBe("$123.46"); // Rounds to 2 decimal places by default
  });

  it("uses custom max decimal places when specified", () => {
    expect(formatDollars(123.456789, 4)).toBe("$123.4568");
    expect(formatDollars(1.1, 6)).toBe("$1.10"); // Min is still 2
  });

  it("formats large numbers with commas", () => {
    expect(formatDollars(1000)).toBe("$1,000.00");
    expect(formatDollars(1000000)).toBe("$1,000,000.00");
  });

  it("handles negative amounts", () => {
    expect(formatDollars(-50)).toBe("-$50.00");
  });
});
