import { describe, expect, it } from "vitest";
import {
  MAX_PRIORITY_BIAS_SECONDS,
  formatBiasDuration,
  parsePriorityBias,
} from "./priorityBias";

describe("parsePriorityBias", () => {
  it("reads empty input as no bias", () => {
    expect(parsePriorityBias("")).toEqual({ ok: true, value: null });
    expect(parsePriorityBias("  ")).toEqual({ ok: true, value: null });
  });

  it("accepts whole seconds from 0 to one day", () => {
    expect(parsePriorityBias("0")).toEqual({ ok: true, value: 0 });
    expect(parsePriorityBias("900")).toEqual({ ok: true, value: 900 });
    expect(parsePriorityBias(String(MAX_PRIORITY_BIAS_SECONDS))).toEqual({
      ok: true,
      value: MAX_PRIORITY_BIAS_SECONDS,
    });
  });

  it.each(["-1", "1.5", "abc", "1e3", String(MAX_PRIORITY_BIAS_SECONDS + 1)])(
    "rejects %s",
    (input) => {
      expect(parsePriorityBias(input).ok).toBe(false);
    },
  );
});

describe("formatBiasDuration", () => {
  it.each([
    [45, "45 s"],
    [900, "15 min"],
    [5400, "1 h 30 min"],
    [86400, "24 h"],
    [3661, "1 h 1 min 1 s"],
  ])("%i seconds is %s", (seconds, expected) => {
    expect(formatBiasDuration(seconds)).toBe(expected);
  });
});
