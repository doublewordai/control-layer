import { describe, expect, it } from "vitest";
import type { Model } from "../../../../api/control-layer/types";
import {
  getCatalogIconInitials,
  getModelOrder,
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
  it("returns the explicit model order when present", () => {
    const model = buildModel({
      metadata: {
        extra: {
          model_order: 7,
        },
      },
    });

    expect(getModelOrder(model)).toBe(7);
  });

  it("returns undefined when no model order is present", () => {
    const model = buildModel();

    expect(getModelOrder(model)).toBeUndefined();
  });

  it("uses the first letters of the first two words for initials", () => {
    expect(getCatalogIconInitials("Open AI")).toBe("OA");
  });

  it("falls back to the first two characters when there is a single word", () => {
    expect(getCatalogIconInitials("OpenAI")).toBe("OP");
  });
});
