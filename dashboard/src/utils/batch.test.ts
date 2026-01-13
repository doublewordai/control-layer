import { describe, it, expect } from "vitest";
import { getBatchDownloadFilename } from "./batch";

describe("getBatchDownloadFilename", () => {
  const testBatchId = "batch_abc123def456";

  it("generates correct filename for input files", () => {
    expect(getBatchDownloadFilename(testBatchId, "input")).toBe(
      "batch_batch_abc123def456_input.jsonl",
    );
  });

  it("generates correct filename for output files", () => {
    expect(getBatchDownloadFilename(testBatchId, "output")).toBe(
      "batch_batch_abc123def456_output.jsonl",
    );
  });

  it("generates correct filename for error files", () => {
    expect(getBatchDownloadFilename(testBatchId, "error")).toBe(
      "batch_batch_abc123def456_error.jsonl",
    );
  });

  it("handles different batch ID formats", () => {
    expect(getBatchDownloadFilename("12345", "output")).toBe(
      "batch_12345_output.jsonl",
    );
    expect(
      getBatchDownloadFilename("550e8400-e29b-41d4-a716-446655440000", "input"),
    ).toBe("batch_550e8400-e29b-41d4-a716-446655440000_input.jsonl");
  });
});
