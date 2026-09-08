import { describe, expect, it } from "vitest";

import type { Batch } from "./types";

import { batchModelName } from "./model";

const batch = (overrides: Partial<Batch> = {}): Batch => ({
  id: "batch_1",
  object: "batch",
  endpoint: "/v1/chat/completions",
  input_file_id: "file_1",
  completion_window: "24h",
  status: "completed",
  request_counts: { total: 1, completed: 1, failed: 0 },
  created_at: 0,
  ...overrides,
});

describe("batchModelName", () => {
  it("prefers the typed model field", () => {
    expect(
      batchModelName(
        batch({ model: "deepseek-v4-pro", metadata: { model: "ignored" } }),
      ),
    ).toBe("deepseek-v4-pro");
  });

  it("falls back to metadata.model", () => {
    expect(
      batchModelName(batch({ metadata: { model: "qwen3-235b" } })),
    ).toBe("qwen3-235b");
  });

  it("trims whitespace", () => {
    expect(batchModelName(batch({ model: "  glm-4.5  " }))).toBe("glm-4.5");
    expect(batchModelName(batch({ metadata: { model: " kimi-k2 " } }))).toBe(
      "kimi-k2",
    );
  });

  it("returns null when neither field is set", () => {
    expect(batchModelName(batch())).toBeNull();
    expect(
      batchModelName(batch({ model: "   ", metadata: { model: "" } })),
    ).toBeNull();
  });
});
