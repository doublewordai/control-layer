import { describe, it, expect } from "vitest";
import {
  buildReferenceIndex,
  computeReferencesForDeployment,
  hasUserConfiguredReferences,
  lookupReferences,
  totalReferenceCount,
} from "./references";
import type { Model } from "../../../api/control-layer/types";

const ENDPOINT_ID = "endpoint-1";

function standardModel(overrides: Partial<Model>): Model {
  return {
    id: overrides.id || "m-std",
    alias: overrides.alias || "std",
    model_name: overrides.model_name || "model-x",
    hosted_on: ENDPOINT_ID,
    is_composite: false,
    ...overrides,
  } as Model;
}

function virtualModel(overrides: Partial<Model>): Model {
  return {
    id: overrides.id || "m-virt",
    alias: overrides.alias || "virt",
    model_name: overrides.model_name || "virt-x",
    is_composite: true,
    ...overrides,
  } as Model;
}

describe("computeReferencesForDeployment", () => {
  it("finds the standard model wrapping a deployment", () => {
    const models: Model[] = [
      standardModel({ id: "wrapper-1", alias: "fast-llama", model_name: "llama-70b" }),
      standardModel({ id: "wrapper-2", alias: "qwen", model_name: "qwen-32b" }),
    ];

    const refs = computeReferencesForDeployment(ENDPOINT_ID, "llama-70b", models);

    expect(refs.directHosted).toEqual([
      { modelId: "wrapper-1", modelAlias: "fast-llama" },
    ]);
    expect(refs.virtualModels).toEqual([]);
    expect(refs.trafficRules).toEqual([]);
  });

  it("ignores standard models hosted on a different endpoint", () => {
    const models: Model[] = [
      standardModel({ id: "wrapper-1", model_name: "llama-70b", hosted_on: "other-endpoint" }),
    ];

    const refs = computeReferencesForDeployment(ENDPOINT_ID, "llama-70b", models);

    expect(refs.directHosted).toEqual([]);
  });

  it("ignores standard models with a different provider model name", () => {
    const models: Model[] = [
      standardModel({ id: "wrapper-1", model_name: "llama-8b" }),
    ];

    const refs = computeReferencesForDeployment(ENDPOINT_ID, "llama-70b", models);

    expect(refs.directHosted).toEqual([]);
  });

  it("finds virtual models that include the deployment as a component", () => {
    const wrapper = standardModel({ id: "wrapper-1", alias: "fast-llama", model_name: "llama-70b" });
    const virtual = virtualModel({
      id: "virt-1",
      alias: "smart-virtual",
      components: [
        {
          weight: 1,
          enabled: true,
          sort_order: 0,
          created_at: "2024-01-01",
          model: { id: "wrapper-1", alias: "fast-llama", model_name: "llama-70b" },
        },
      ],
    });

    const refs = computeReferencesForDeployment(ENDPOINT_ID, "llama-70b", [wrapper, virtual]);

    expect(refs.virtualModels).toEqual([
      { modelId: "virt-1", modelAlias: "smart-virtual" },
    ]);
  });

  it("does not list virtual models whose components reference unrelated standard models", () => {
    const wrapper = standardModel({ id: "wrapper-1", alias: "fast-llama", model_name: "llama-70b" });
    const otherWrapper = standardModel({ id: "wrapper-2", alias: "qwen", model_name: "qwen-32b" });
    const virtual = virtualModel({
      id: "virt-1",
      components: [
        {
          weight: 1,
          enabled: true,
          sort_order: 0,
          created_at: "2024-01-01",
          model: { id: "wrapper-2", alias: "qwen", model_name: "qwen-32b" },
        },
      ],
    });

    const refs = computeReferencesForDeployment(ENDPOINT_ID, "llama-70b", [wrapper, otherWrapper, virtual]);

    expect(refs.virtualModels).toEqual([]);
  });

  it("finds traffic rules that redirect to one of the wrapper aliases", () => {
    const wrapper = standardModel({ id: "wrapper-1", alias: "fast-llama", model_name: "llama-70b" });
    const ruleOwner: Model = {
      ...virtualModel({ id: "owner", alias: "owner-alias" }),
      traffic_routing_rules: [
        { api_key_purpose: "batch", action: { type: "redirect", target: "fast-llama" } },
        { api_key_purpose: "realtime", action: { type: "deny" } },
      ],
    };

    const refs = computeReferencesForDeployment(ENDPOINT_ID, "llama-70b", [wrapper, ruleOwner]);

    expect(refs.trafficRules).toHaveLength(1);
    expect(refs.trafficRules[0]).toMatchObject({
      modelId: "owner",
      modelAlias: "owner-alias",
      rule: { api_key_purpose: "batch", action: { type: "redirect", target: "fast-llama" } },
    });
  });

  it("ignores deny rules and redirects to other targets", () => {
    const wrapper = standardModel({ id: "wrapper-1", alias: "fast-llama", model_name: "llama-70b" });
    const ruleOwner: Model = {
      ...virtualModel({ id: "owner", alias: "owner-alias" }),
      traffic_routing_rules: [
        { api_key_purpose: "batch", action: { type: "deny" } },
        { api_key_purpose: "realtime", action: { type: "redirect", target: "some-other-model" } },
      ],
    };

    const refs = computeReferencesForDeployment(ENDPOINT_ID, "llama-70b", [wrapper, ruleOwner]);

    expect(refs.trafficRules).toEqual([]);
  });

  it("returns empty references when there is no wrapping standard model", () => {
    const refs = computeReferencesForDeployment(ENDPOINT_ID, "ghost-model", []);

    expect(refs.directHosted).toEqual([]);
    expect(refs.virtualModels).toEqual([]);
    expect(refs.trafficRules).toEqual([]);
    expect(totalReferenceCount(refs)).toBe(0);
  });

  it("totalReferenceCount sums direct, virtual, and rules", () => {
    const refs = {
      directHosted: [{ modelId: "a", modelAlias: "a" }],
      virtualModels: [
        { modelId: "b", modelAlias: "b" },
        { modelId: "c", modelAlias: "c" },
      ],
      trafficRules: [
        {
          modelId: "d",
          modelAlias: "d",
          rule: { api_key_purpose: "batch" as const, action: { type: "deny" as const } },
        },
      ],
    };

    expect(totalReferenceCount(refs)).toBe(4);
  });
});

describe("hasUserConfiguredReferences", () => {
  it("ignores the implicit single Standard Model wrapper", () => {
    expect(
      hasUserConfiguredReferences({
        directHosted: [{ modelId: "wrapper", modelAlias: "alias" }],
        virtualModels: [],
        trafficRules: [],
      }),
    ).toBe(false);
  });

  it("flags additional wrappers beyond the implicit one", () => {
    expect(
      hasUserConfiguredReferences({
        directHosted: [
          { modelId: "wrapper-1", modelAlias: "alias-1" },
          { modelId: "wrapper-2", modelAlias: "alias-2" },
        ],
        virtualModels: [],
        trafficRules: [],
      }),
    ).toBe(true);
  });

  it("flags virtual model dependencies", () => {
    expect(
      hasUserConfiguredReferences({
        directHosted: [{ modelId: "wrapper", modelAlias: "alias" }],
        virtualModels: [{ modelId: "v", modelAlias: "v" }],
        trafficRules: [],
      }),
    ).toBe(true);
  });

  it("flags traffic rule dependencies", () => {
    expect(
      hasUserConfiguredReferences({
        directHosted: [{ modelId: "wrapper", modelAlias: "alias" }],
        virtualModels: [],
        trafficRules: [
          {
            modelId: "owner",
            modelAlias: "owner",
            rule: { api_key_purpose: "batch", action: { type: "deny" } },
          },
        ],
      }),
    ).toBe(true);
  });
});

describe("buildReferenceIndex + lookupReferences", () => {
  it("returns the same data as computeReferencesForDeployment for a typical case", () => {
    const wrapper = standardModel({
      id: "wrapper-1",
      alias: "fast-llama",
      model_name: "llama-70b",
    });
    const virtual = virtualModel({
      id: "virt-1",
      alias: "smart-virtual",
      components: [
        {
          weight: 1,
          enabled: true,
          sort_order: 0,
          created_at: "2024-01-01",
          model: { id: "wrapper-1", alias: "fast-llama", model_name: "llama-70b" },
        },
      ],
    });
    const ruleOwner: Model = {
      ...virtualModel({ id: "owner", alias: "owner-alias" }),
      traffic_routing_rules: [
        { api_key_purpose: "batch", action: { type: "redirect", target: "fast-llama" } },
      ],
    };

    const all = [wrapper, virtual, ruleOwner];
    const direct = computeReferencesForDeployment(ENDPOINT_ID, "llama-70b", all);
    const indexed = lookupReferences(
      buildReferenceIndex(all),
      ENDPOINT_ID,
      "llama-70b",
    );

    expect(indexed).toEqual(direct);
  });

  it("returns empty results for a deployment with no references in the index", () => {
    const index = buildReferenceIndex([]);
    const refs = lookupReferences(index, ENDPOINT_ID, "ghost-model");

    expect(refs.directHosted).toEqual([]);
    expect(refs.virtualModels).toEqual([]);
    expect(refs.trafficRules).toEqual([]);
  });

  it("does not double-count a virtual model that includes two wrappers of the same deployment", () => {
    const wrapperA = standardModel({
      id: "wrapper-a",
      alias: "alias-a",
      model_name: "shared-model",
    });
    const wrapperB = standardModel({
      id: "wrapper-b",
      alias: "alias-b",
      model_name: "shared-model",
    });
    const virtual = virtualModel({
      id: "virt-1",
      alias: "v",
      components: [
        {
          weight: 1,
          enabled: true,
          sort_order: 0,
          created_at: "2024-01-01",
          model: { id: "wrapper-a", alias: "alias-a", model_name: "shared-model" },
        },
        {
          weight: 1,
          enabled: true,
          sort_order: 1,
          created_at: "2024-01-01",
          model: { id: "wrapper-b", alias: "alias-b", model_name: "shared-model" },
        },
      ],
    });

    const refs = lookupReferences(
      buildReferenceIndex([wrapperA, wrapperB, virtual]),
      ENDPOINT_ID,
      "shared-model",
    );

    expect(refs.virtualModels).toHaveLength(1);
    expect(refs.virtualModels[0].modelId).toBe("virt-1");
  });
});
