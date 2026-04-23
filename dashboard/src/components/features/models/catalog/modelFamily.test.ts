import { describe, expect, it } from "vitest";
import type { Model, ModelTariff } from "../../../../api/control-layer/types";
import {
  aggregateFamilies,
  getFamilyInfo,
  getFormatBadges,
  pickTariffForContext,
} from "./modelFamily";

function buildModel(overrides: Partial<Model> = {}): Model {
  return {
    id: "model-1",
    alias: "Qwen/Qwen3.5-397B-A17B-FP8",
    model_name: "Qwen/Qwen3.5-397B-A17B-FP8",
    model_type: "CHAT",
    metadata: {
      provider: "Alibaba",
    },
    ...overrides,
  };
}

function buildTariff(overrides: Partial<ModelTariff> = {}): ModelTariff {
  return {
    id: "t1",
    deployed_model_id: "m1",
    name: "default",
    input_price_per_token: "0.000001",
    output_price_per_token: "0.000002",
    valid_from: "2026-01-01T00:00:00Z",
    valid_until: null,
    is_active: true,
    api_key_purpose: "realtime",
    completion_window: null,
    ...overrides,
  };
}

describe("getFamilyInfo", () => {
  it("splits Qwen3.5 variants into family + variant", () => {
    const info = getFamilyInfo(
      buildModel({ alias: "Qwen/Qwen3.5-397B-A17B-FP8" }),
    );
    expect(info.label).toBe("Qwen 3.5");
    expect(info.variantLabel).toBe("397 B A 17 B FP 8");
    expect(info.key).toBe("alibaba::qwen3.5");
  });

  it("groups Qwen3.5 sizes together regardless of quantization", () => {
    const a = getFamilyInfo(
      buildModel({ alias: "Qwen/Qwen3.5-397B-A17B-FP8" }),
    );
    const b = getFamilyInfo(
      buildModel({ alias: "Qwen/Qwen3.5-35B-A3B-FP8" }),
    );
    expect(a.key).toBe(b.key);
  });

  it("keeps Qwen3 and Qwen3.5 families distinct", () => {
    const a = getFamilyInfo(buildModel({ alias: "Qwen/Qwen3-14B-FP8" }));
    const b = getFamilyInfo(buildModel({ alias: "Qwen/Qwen3.5-35B-A3B-FP8" }));
    expect(a.key).not.toBe(b.key);
  });

  it("keeps Qwen3-VL separate from Qwen3 since VL is not a size token", () => {
    const a = getFamilyInfo(buildModel({ alias: "Qwen/Qwen3-14B-FP8" }));
    const b = getFamilyInfo(
      buildModel({ alias: "Qwen/Qwen3-VL-235B-A22B-Instruct-FP8" }),
    );
    expect(a.key).not.toBe(b.key);
  });

  it("handles aliases without a slash", () => {
    const info = getFamilyInfo(
      buildModel({
        alias: "gpt-oss-20b",
        metadata: { provider: "OpenAI" },
      }),
    );
    expect(info.label).toBe("gpt oss");
    expect(info.variantLabel).toBe("20 b");
  });

  it("uses provider from metadata to disambiguate identical stems", () => {
    const a = getFamilyInfo(
      buildModel({
        alias: "X/Foo-7B",
        metadata: { provider: "Alice" },
      }),
    );
    const b = getFamilyInfo(
      buildModel({
        alias: "X/Foo-7B",
        metadata: { provider: "Bob" },
      }),
    );
    expect(a.key).not.toBe(b.key);
  });
});

describe("getFormatBadges", () => {
  it("returns the explicit quantization when present", () => {
    const badges = getFormatBadges(
      buildModel({
        alias: "Qwen/Qwen3.5-9B",
        metadata: { provider: "Alibaba", quantization: "FP8" },
      }),
    );
    expect(badges).toEqual(["FP8"]);
  });

  it("infers quantization from alias tokens as a fallback", () => {
    const badges = getFormatBadges(
      buildModel({
        alias: "Qwen/Qwen3.5-35B-A3B-INT4",
        metadata: { provider: "Alibaba" },
      }),
    );
    expect(badges).toEqual(["INT4"]);
  });

  it("returns an empty list when no format is present", () => {
    const badges = getFormatBadges(
      buildModel({ alias: "Qwen/Qwen3.5-9B", metadata: null }),
    );
    expect(badges).toEqual([]);
  });
});

describe("pickTariffForContext", () => {
  const realtime = buildTariff({
    id: "rt",
    api_key_purpose: "realtime",
    input_price_per_token: "0.000001",
    output_price_per_token: "0.000002",
  });
  const async1h = buildTariff({
    id: "a1",
    api_key_purpose: "batch",
    completion_window: "1h",
    input_price_per_token: "0.0000008",
    output_price_per_token: "0.0000016",
  });
  const batch24h = buildTariff({
    id: "b24",
    api_key_purpose: "batch",
    completion_window: "24h",
    input_price_per_token: "0.0000004",
    output_price_per_token: "0.0000008",
  });
  const playground = buildTariff({
    id: "pg",
    api_key_purpose: "playground",
  });

  it("picks the realtime tariff for async context", () => {
    expect(
      pickTariffForContext([realtime, async1h, batch24h, playground], "async")
        ?.id,
    ).toBe("rt");
  });

  it("picks the 1h batch tariff when no realtime tariff exists", () => {
    expect(pickTariffForContext([async1h, batch24h], "async")?.id).toBe("a1");
  });

  it("picks the 24h batch tariff for batch context", () => {
    expect(
      pickTariffForContext([realtime, async1h, batch24h], "batch")?.id,
    ).toBe("b24");
  });

  it("ignores playground tariffs", () => {
    expect(pickTariffForContext([playground], "async")).toBeNull();
  });

  it("returns null for models without tariffs", () => {
    expect(pickTariffForContext(null, "async")).toBeNull();
    expect(pickTariffForContext([], "async")).toBeNull();
  });
});

describe("aggregateFamilies", () => {
  const providerLabelOf = (p: string) =>
    p === "alibaba" ? "Alibaba" : p === "openai" ? "OpenAI" : p;
  const providerIconOf = () => null;
  const displayCapabilitiesOf = (m: Model) =>
    m.metadata?.display_category === "embedding" ? ["embeddings"] : ["text"];

  it("groups Qwen 3.5 variants into a single family and aggregates stats", () => {
    const models: Model[] = [
      buildModel({
        id: "big",
        alias: "Qwen/Qwen3.5-397B-A17B-FP8",
        metadata: {
          provider: "Alibaba",
          intelligence_index: 45,
          context_window: 262144,
          released_at: "2026-02-16",
        },
        tariffs: [
          buildTariff({
            id: "big-rt",
            input_price_per_token: "0.0000015",
            output_price_per_token: "0.0000030",
          }),
        ],
      }),
      buildModel({
        id: "mid",
        alias: "Qwen/Qwen3.5-35B-A3B-FP8",
        metadata: {
          provider: "Alibaba",
          intelligence_index: 37,
          context_window: 262144,
          released_at: "2026-02-24",
        },
        tariffs: [
          buildTariff({
            id: "mid-rt",
            input_price_per_token: "0.0000003",
            output_price_per_token: "0.0000009",
          }),
        ],
      }),
    ];

    const families = aggregateFamilies(models, {
      newCutoff: "2026-02-01",
      context: "async",
      providerLabelOf,
      providerIconOf,
      displayCapabilitiesOf,
    });

    expect(families).toHaveLength(1);
    const fam = families[0];
    expect(fam.label).toBe("Qwen 3.5");
    expect(fam.variants).toHaveLength(2);
    expect(fam.intelligenceMin).toBe(37);
    expect(fam.intelligenceMax).toBe(45);
    expect(fam.contextMax).toBe(262144);
    expect(fam.priceFrom).toBeCloseTo(0.0000003);
    expect(fam.releasedAt).toBe("2026-02-24");
    expect(fam.hasNewVariant).toBe(true);
  });

  it("sorts variants by intelligence descending within a family", () => {
    const models: Model[] = [
      buildModel({
        id: "small",
        alias: "Qwen/Qwen3.5-9B",
        metadata: { provider: "Alibaba", intelligence_index: 20 },
      }),
      buildModel({
        id: "big",
        alias: "Qwen/Qwen3.5-397B-A17B-FP8",
        metadata: { provider: "Alibaba", intelligence_index: 45 },
      }),
    ];

    const families = aggregateFamilies(models, {
      newCutoff: "2099-01-01",
      context: "async",
      providerLabelOf,
      providerIconOf,
      displayCapabilitiesOf,
    });

    expect(families[0].variants.map((v) => v.id)).toEqual(["big", "small"]);
  });

  it("orders families by most recent release first", () => {
    const models: Model[] = [
      buildModel({
        id: "older",
        alias: "Old/Old-9B",
        metadata: { provider: "Old", released_at: "2024-01-01" },
      }),
      buildModel({
        id: "newer",
        alias: "New/New-9B",
        metadata: { provider: "New", released_at: "2026-04-01" },
      }),
    ];

    const families = aggregateFamilies(models, {
      newCutoff: "2026-01-01",
      context: "async",
      providerLabelOf: (p) => p,
      providerIconOf,
      displayCapabilitiesOf,
    });

    expect(families.map((f) => f.label)).toEqual(["New", "Old"]);
  });
});
