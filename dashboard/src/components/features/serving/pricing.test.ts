import { describe, expect, it } from "vitest";
import type {
  CachePricing,
  ModelTariff,
  OrganizationCacheTariff,
} from "@/api/control-layer/types";
import { resolveCachePrice, resolveTokenPrice } from "./pricing";

const now = Date.parse("2026-06-01T00:00:00Z");
function tariff(id: string, extra: Partial<ModelTariff> = {}): ModelTariff {
  return {
    id,
    deployed_model_id: "model",
    name: id,
    input_price_per_token: "0.000001",
    output_price_per_token: "0.000002",
    valid_from: "2026-01-01T00:00:00Z",
    valid_until: null,
    is_active: true,
    api_key_purpose: "realtime",
    ...extra,
  };
}
describe("operator effective pricing", () => {
  it("keeps zero, honours scope precedence and isolates organisations", () => {
    const rows = [
      tariff("general"),
      tariff("other", { organization_id: "b", serving_class: "interactive" }),
      tariff("all", { organization_id: "a" }),
      tariff("class", {
        organization_id: "a",
        serving_class: "interactive",
        input_price_per_token: "0",
        output_price_per_token: "0",
      }),
    ];
    expect(
      resolveTokenPrice(rows, "a", "interactive", "realtime", null, now)?.id,
    ).toBe("class");
    expect(
      resolveTokenPrice(rows, "a", "standard", "realtime", null, now)?.id,
    ).toBe("all");
    expect(
      resolveTokenPrice(rows, "c", "interactive", "realtime", null, now)?.id,
    ).toBe("general");
    expect(
      resolveTokenPrice(rows, undefined, "standard", "realtime", null, now)?.id,
    ).toBe("general");
  });
  it("exhausts exact batch windows before standard-class realtime fallback", () => {
    const rows = [
      tariff("realtime", {
        organization_id: "a",
        input_price_per_token: "0",
        output_price_per_token: "0",
      }),
      tariff("batch", { api_key_purpose: "batch", completion_window: "24h" }),
      tariff("standard", {
        organization_id: "a",
        serving_class: "standard",
        api_key_purpose: "batch",
        completion_window: "1h",
      }),
    ];
    expect(
      resolveTokenPrice(rows, "a", "interactive", "batch", "24h", now)?.id,
    ).toBe("batch");
    expect(
      resolveTokenPrice(rows, "a", "interactive", "batch", "1h", now)?.id,
    ).toBe("standard");
    expect(
      resolveTokenPrice(rows, "a", "standard", "batch", "12h", now)?.id,
    ).toBe("realtime");
  });
  it("exhausts playground fallback within the class scope before general playground", () => {
    const rows = [
      tariff("general-playground", { api_key_purpose: "playground" }),
      tariff("org-playground", {
        organization_id: "a",
        api_key_purpose: "playground",
      }),
      tariff("class-realtime", {
        organization_id: "a",
        serving_class: "interactive",
      }),
    ];
    expect(
      resolveTokenPrice(rows, "a", "interactive", "playground", null, now)?.id,
    ).toBe("class-realtime");
  });
  it("ignores expired/future prices and breaks same-time ties by id", () => {
    const rows = [
      tariff("b"),
      tariff("a"),
      tariff("expired", {
        organization_id: "a",
        valid_until: "2026-06-01T00:00:00Z",
      }),
      tariff("future", {
        organization_id: "a",
        valid_from: "2026-06-02T00:00:00Z",
      }),
    ];
    expect(
      resolveTokenPrice(rows, "a", "standard", "realtime", null, now)?.id,
    ).toBe("a");
  });
  it("gates cache on the general setting and inherits class/all-class/general multipliers", () => {
    const general: CachePricing = {
      enabled: true,
      read_multiplier: "0.1",
      write_multiplier_5m: "1",
      write_multiplier_1h: "2",
      write_multiplier_24h: "3",
      min_prefix_tokens: 1024,
      valid_from: null,
      valid_until: null,
    };
    const all: OrganizationCacheTariff = {
      deployed_model_id: "model",
      alias: "example",
      read_multiplier: "0.2",
      write_multiplier_5m: "1",
      write_multiplier_1h: "2",
      write_multiplier_24h: "3",
      valid_from: "2026-01-01T00:00:00Z",
    };
    const special = {
      ...all,
      serving_class: "interactive" as const,
      read_multiplier: "0",
    };
    expect(
      resolveCachePrice(general, [all, special], "interactive", now)?.values
        .read_multiplier,
    ).toBe("0");
    expect(
      resolveCachePrice(general, [all, special], "standard", now)?.values
        .read_multiplier,
    ).toBe("0.2");
    expect(
      resolveCachePrice(general, [], "standard", now)?.values.read_multiplier,
    ).toBe("0.1");
    expect(
      resolveCachePrice(
        { ...general, enabled: false },
        [special],
        "interactive",
        now,
      ),
    ).toBeUndefined();
  });
});
