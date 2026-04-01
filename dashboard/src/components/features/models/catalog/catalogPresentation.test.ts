import { describe, expect, it } from "vitest";
import type { Model } from "../../../../api/control-layer/types";
import {
  getModelIcon,
  getModelIconLabel,
} from "./catalogPresentation";

function buildModel(overrides: Partial<Model> = {}): Model {
  return {
    id: "model-1",
    alias: "test/model",
    model_name: "test/model",
    model_type: "CHAT",
    metadata: null,
    ...overrides,
  };
}

describe("catalogPresentation", () => {
  it("falls back to provider icons for known providers", () => {
    const model = buildModel({
      metadata: {
        provider: "OpenAI",
      },
    });

    expect(getModelIcon(model)).toBe("openai");
    expect(getModelIconLabel(model)).toBe("OpenAI");
  });

  it("prefers explicit entry icons over provider icons", () => {
    const model = buildModel({
      metadata: {
        provider: "OpenAI",
      },
    });

    expect(
      getModelIcon(model, {
        deployment_id: model.id,
        sort_order: 0,
        icon: "https://cdn.example.com/jv-mark.svg",
        created_at: "2026-03-31T00:00:00Z",
      }),
    ).toBe("https://cdn.example.com/jv-mark.svg");
  });

  it("returns undefined when there is no known icon source", () => {
    const model = buildModel();

    expect(getModelIcon(model)).toBeUndefined();
  });
});
