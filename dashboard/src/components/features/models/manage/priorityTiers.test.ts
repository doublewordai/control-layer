import { describe, expect, it } from "vitest";
import type { ModelComponent } from "../../../../api/control-layer";
import {
  groupPriorityTiers,
  moveProviderToTier,
  parseRoutingInteger,
} from "./priorityTiers";

function component(id: string, sortOrder: number, weight = 1): ModelComponent {
  return {
    weight,
    enabled: true,
    sort_order: sortOrder,
    created_at: "2026-08-07T00:00:00Z",
    model: { id, alias: id, model_name: id },
  };
}

describe("priority tiers", () => {
  it("groups equal priorities and orders tiers numerically", () => {
    const tiers = groupPriorityTiers([
      component("backup", 2),
      component("primary-b", 0, 3),
      component("primary-a", 0, 1),
    ]);

    expect(tiers.map((tier) => tier.priority)).toEqual([0, 2]);
    expect(tiers[0].providers.map((provider) => provider.model.id)).toEqual([
      "primary-b",
      "primary-a",
    ]);
  });

  it("moves only the selected provider into an existing tier", () => {
    const components = [component("a", 0), component("b", 1)];

    expect(moveProviderToTier(components, "b", 0).map((item) => item.sort_order)).toEqual([
      0, 0,
    ]);
    expect(components.map((item) => item.sort_order)).toEqual([0, 1]);
  });
});

describe("routing number parsing", () => {
  it("accepts only bounded integers", () => {
    expect(parseRoutingInteger("25", 1, 100)).toBe(25);
    expect(parseRoutingInteger("", 1, 100)).toBeNull();
    expect(parseRoutingInteger("   ", 1, 100)).toBeNull();
    expect(parseRoutingInteger("1.5", 1, 100)).toBeNull();
    expect(parseRoutingInteger("not-a-number", 1, 100)).toBeNull();
    expect(parseRoutingInteger("0", 1, 100)).toBeNull();
    expect(parseRoutingInteger("101", 1, 100)).toBeNull();
  });
});
