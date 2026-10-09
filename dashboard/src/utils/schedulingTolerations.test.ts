import { describe, it, expect } from "vitest";
import {
  formatTolerations,
  isPinned,
  parseTolerations,
  pinBadgeLabel,
  pinState,
} from "./schedulingTolerations";

describe("pinState", () => {
  it("classifies the three pin states", () => {
    expect(pinState(null).kind).toBe("unpinned");
    expect(pinState(undefined).kind).toBe("unpinned");
    expect(pinState([]).kind).toBe("dedicated");
    expect(pinState([{ key: "k", value: "v" }])).toEqual({
      kind: "custom",
      tolerations: [{ key: "k", value: "v" }],
    });
  });
});

describe("badge", () => {
  it("is null only when unpinned", () => {
    expect(isPinned(null)).toBe(false);
    expect(isPinned([])).toBe(true);
    expect(pinBadgeLabel(null)).toBeNull();
    expect(pinBadgeLabel([])).toBe("Dedicated capacity");
    expect(pinBadgeLabel([{ key: "k", value: "v" }])).toBe("Custom scheduling");
  });
});

describe("parseTolerations", () => {
  it("accepts the empty list as a real pin", () => {
    const result = parseTolerations("[]");
    expect(result).toEqual({ ok: true, tolerations: [] });
  });

  it("keeps only the fields the client sent", () => {
    const result = parseTolerations('[{ "key": "dedicated", "value": "only", "effect": "NoSchedule" }]');
    expect(result).toEqual({
      ok: true,
      tolerations: [{ key: "dedicated", value: "only", effect: "NoSchedule" }],
    });
  });

  it("refuses an Equal toleration without a value", () => {
    const result = parseTolerations('[{"key": "dedicated"}]');
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.error).toMatch(/Equal needs a value/);
  });

  it("refuses an Exists toleration carrying a value", () => {
    const result = parseTolerations('[{"key": "k", "operator": "Exists", "value": "nope"}]');
    expect(result.ok).toBe(false);
    if (!result.ok) expect(result.error).toMatch(/must not carry a value/);
  });

  it("refuses unknown fields and spellings", () => {
    expect(parseTolerations('[{"key": "k", "value": "v", "priority": 1}]').ok).toBe(false);
    expect(parseTolerations('[{"key": "k", "value": "v", "effect": "Bogus"}]').ok).toBe(false);
    expect(parseTolerations('[{"key": "k", "value": "v", "operator": "Sometimes"}]').ok).toBe(false);
  });

  it("refuses non-JSON and non-arrays", () => {
    expect(parseTolerations("not json").ok).toBe(false);
    expect(parseTolerations('{"key": "k"}').ok).toBe(false);
    expect(parseTolerations("").ok).toBe(false);
  });
});

describe("formatTolerations", () => {
  it("pretty-prints a pin for the editor", () => {
    expect(formatTolerations([{ key: "pool", value: "gpu" }])).toBe(
      JSON.stringify([{ key: "pool", value: "gpu" }], null, 2),
    );
  });
});
