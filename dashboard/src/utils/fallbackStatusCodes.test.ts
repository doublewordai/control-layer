import { describe, expect, it } from "vitest";
import {
  DEFAULT_FALLBACK_STATUS_CODES,
  buildFallbackStatusCodes,
} from "./fallbackStatusCodes";

describe("virtual-model fallback status codes", () => {
  it("includes 499 in the default status list", () => {
    expect(DEFAULT_FALLBACK_STATUS_CODES).toEqual([
      429, 499, 500, 502, 503, 504,
    ]);
  });

  it("toggles 499 independently", () => {
    const base = {
      on429: true,
      on404: true,
      on5xx: true,
    };

    expect(buildFallbackStatusCodes({ ...base, on499: true })).toContain(499);
    expect(buildFallbackStatusCodes({ ...base, on499: false })).not.toContain(
      499,
    );
    expect(buildFallbackStatusCodes({ ...base, on499: false })).toEqual([
      429, 404, 500, 502, 503, 504,
    ]);
  });
});
