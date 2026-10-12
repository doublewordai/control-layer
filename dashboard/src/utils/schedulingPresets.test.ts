import { describe, it, expect } from "vitest";
import {
  DEFAULT_TOLERATION_PRESET,
  presetBadgeLabel,
} from "./schedulingPresets";

const PRESETS = [
  { id: "no-overflow", label: "No overflow capacity", description: "d1" },
  { id: "gpu", label: "GPU only", description: "d2" },
];

describe("presetBadgeLabel", () => {
  it("is null on Default (nothing pinned)", () => {
    expect(presetBadgeLabel(null, PRESETS)).toBeNull();
    expect(presetBadgeLabel(undefined, PRESETS)).toBeNull();
  });

  it("uses the configured preset's label", () => {
    expect(presetBadgeLabel("gpu", PRESETS)).toBe("GPU only");
  });

  it("falls back to the raw id when the preset is not configured", () => {
    expect(presetBadgeLabel("retired", PRESETS)).toBe("retired");
    expect(presetBadgeLabel("gpu", undefined)).toBe("gpu");
  });
});

describe("DEFAULT_TOLERATION_PRESET", () => {
  it("is a non-empty sentinel Radix Select accepts", () => {
    expect(DEFAULT_TOLERATION_PRESET).not.toBe("");
  });
});
