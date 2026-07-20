import { describe, expect, it } from "vitest";
import {
  DEFAULT_FALLBACK_STATUS_CODES,
  buildFallbackStatusCodes,
  buildFallbackUpdatePayload,
} from "./fallbackStatusCodes";

describe("virtual-model fallback status codes", () => {
  it("includes 499 in the default status list", () => {
    expect(DEFAULT_FALLBACK_STATUS_CODES).toEqual([
      429, 499, 500, 502, 503, 504,
    ]);
  });

  it("preserves an unchanged sparse status list exactly", () => {
    expect(
      buildFallbackStatusCodes({
        originalStatuses: [429, 503],
        on429: true,
        on499: false,
        on404: false,
        on5xx: true,
      }),
    ).toEqual([429, 503]);
  });

  it("preserves custom statuses, wildcard 5, and custom 5xx values when switches are unchanged", () => {
    expect(
      buildFallbackStatusCodes({
        originalStatuses: [5, 408, 429, 501, 503],
        on429: true,
        on499: false,
        on404: false,
        on5xx: true,
      }),
    ).toEqual([5, 408, 429, 501, 503]);
  });

  it("preserves an empty status list when every switch remains off", () => {
    expect(
      buildFallbackStatusCodes({
        originalStatuses: [],
        on429: false,
        on499: false,
        on404: false,
        on5xx: false,
      }),
    ).toEqual([]);
  });

  it("adds or removes only 499", () => {
    expect(
      buildFallbackStatusCodes({
        originalStatuses: [5, 408, 429, 499, 503],
        on429: true,
        on499: false,
        on404: false,
        on5xx: true,
      }),
    ).toEqual([5, 408, 429, 503]);

    expect(
      buildFallbackStatusCodes({
        originalStatuses: [5, 408, 429, 503],
        on429: true,
        on499: true,
        on404: false,
        on5xx: true,
      }),
    ).toEqual([5, 408, 429, 503, 499]);
  });

  it("adds or removes only the individual 429 and 404 codes", () => {
    expect(
      buildFallbackStatusCodes({
        originalStatuses: [5, 404, 429, 501],
        on429: false,
        on499: false,
        on404: false,
        on5xx: true,
      }),
    ).toEqual([5, 501]);

    expect(
      buildFallbackStatusCodes({
        originalStatuses: [5, 408, 501],
        on429: true,
        on499: false,
        on404: true,
        on5xx: true,
      }),
    ).toEqual([5, 408, 501, 429, 404]);
  });

  it("normalizes grouped 5xx statuses only when the group is enabled", () => {
    expect(
      buildFallbackStatusCodes({
        originalStatuses: [5, 408],
        on429: false,
        on499: false,
        on404: false,
        on5xx: true,
      }),
    ).toEqual([5, 408, 500, 502, 503, 504]);
  });

  it("removes grouped 5xx statuses only when the group is disabled", () => {
    expect(
      buildFallbackStatusCodes({
        originalStatuses: [5, 408, 501, 503],
        on429: false,
        on499: false,
        on404: false,
        on5xx: false,
      }),
    ).toEqual([5, 408]);
  });

  it("retains the computed status policy in disabled and re-enabled payloads", () => {
    const base = {
      originalStatuses: [5, 408, 429, 503],
      on429: true,
      on499: true,
      on404: false,
      on5xx: true,
      fallbackOnRateLimit: true,
      withReplacement: true,
      maxAttempts: 3,
    };

    const disabled = buildFallbackUpdatePayload({
      ...base,
      fallbackEnabled: false,
    });
    const reEnabled = buildFallbackUpdatePayload({
      ...base,
      fallbackEnabled: true,
    });

    expect(disabled).toMatchObject({
      fallback_enabled: false,
      fallback_on_rate_limit: false,
      fallback_on_status: [5, 408, 429, 503, 499],
      fallback_with_replacement: false,
      fallback_max_attempts: null,
    });
    expect(reEnabled).toMatchObject({
      fallback_enabled: true,
      fallback_on_rate_limit: true,
      fallback_on_status: [5, 408, 429, 503, 499],
      fallback_with_replacement: true,
      fallback_max_attempts: 3,
    });
  });
});
