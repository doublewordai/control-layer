import { describe, it, expect } from "vitest";
import { isPlaygroundDenied, isBatchDenied, isRealtimeDenied } from "./modelAccess";
import type { Model } from "../api/control-layer/types";

const baseModel: Model = {
  id: "test-id",
  alias: "test-model",
  model_name: "test",
  has_payment_provider_id: false,
} as Model;

describe("isPlaygroundDenied", () => {
  it("returns false when traffic_routing_rules is undefined", () => {
    expect(isPlaygroundDenied(baseModel)).toBe(false);
  });

  it("returns false when traffic_routing_rules is null", () => {
    expect(
      isPlaygroundDenied({ ...baseModel, traffic_routing_rules: null }),
    ).toBe(false);
  });

  it("returns false when traffic_routing_rules is empty", () => {
    expect(
      isPlaygroundDenied({ ...baseModel, traffic_routing_rules: [] }),
    ).toBe(false);
  });

  it("returns true when playground purpose is denied", () => {
    expect(
      isPlaygroundDenied({
        ...baseModel,
        traffic_routing_rules: [
          { api_key_purpose: "playground", action: { type: "deny" } },
        ],
      }),
    ).toBe(true);
  });

  it("returns false when only realtime purpose is denied (separate from playground)", () => {
    expect(
      isPlaygroundDenied({
        ...baseModel,
        traffic_routing_rules: [
          { api_key_purpose: "realtime", action: { type: "deny" } },
        ],
      }),
    ).toBe(false);
  });

  it("returns false when batch purpose is denied (not playground-related)", () => {
    expect(
      isPlaygroundDenied({
        ...baseModel,
        traffic_routing_rules: [
          { api_key_purpose: "batch", action: { type: "deny" } },
        ],
      }),
    ).toBe(false);
  });

  it("returns false when playground purpose is redirected (not denied)", () => {
    expect(
      isPlaygroundDenied({
        ...baseModel,
        traffic_routing_rules: [
          {
            api_key_purpose: "playground",
            action: { type: "redirect", target: "other-model" },
          },
        ],
      }),
    ).toBe(false);
  });

  it("returns true when one of multiple rules denies playground", () => {
    expect(
      isPlaygroundDenied({
        ...baseModel,
        traffic_routing_rules: [
          { api_key_purpose: "batch", action: { type: "deny" } },
          { api_key_purpose: "playground", action: { type: "deny" } },
        ],
      }),
    ).toBe(true);
  });
});

describe("isBatchDenied", () => {
  it("returns false when traffic_routing_rules is undefined", () => {
    expect(isBatchDenied(baseModel)).toBe(false);
  });

  it("returns true when batch purpose is denied", () => {
    expect(
      isBatchDenied({
        ...baseModel,
        traffic_routing_rules: [
          { api_key_purpose: "batch", action: { type: "deny" } },
        ],
      }),
    ).toBe(true);
  });

  it("returns false when batch purpose is redirected", () => {
    expect(
      isBatchDenied({
        ...baseModel,
        traffic_routing_rules: [
          {
            api_key_purpose: "batch",
            action: { type: "redirect", target: "other-model" },
          },
        ],
      }),
    ).toBe(false);
  });

  it("returns false when only playground is denied", () => {
    expect(
      isBatchDenied({
        ...baseModel,
        traffic_routing_rules: [
          { api_key_purpose: "playground", action: { type: "deny" } },
        ],
      }),
    ).toBe(false);
  });
});

describe("isRealtimeDenied", () => {
  it("returns false when traffic_routing_rules is undefined", () => {
    expect(isRealtimeDenied(baseModel)).toBe(false);
  });

  it("returns true when realtime purpose is denied", () => {
    expect(
      isRealtimeDenied({
        ...baseModel,
        traffic_routing_rules: [
          { api_key_purpose: "realtime", action: { type: "deny" } },
        ],
      }),
    ).toBe(true);
  });

  it("returns false when realtime purpose is redirected", () => {
    expect(
      isRealtimeDenied({
        ...baseModel,
        traffic_routing_rules: [
          {
            api_key_purpose: "realtime",
            action: { type: "redirect", target: "other-model" },
          },
        ],
      }),
    ).toBe(false);
  });

  it("returns false when only playground is denied", () => {
    expect(
      isRealtimeDenied({
        ...baseModel,
        traffic_routing_rules: [
          { api_key_purpose: "playground", action: { type: "deny" } },
        ],
      }),
    ).toBe(false);
  });
});
