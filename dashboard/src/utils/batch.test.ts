import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { getBatchDownloadFilename, downloadFile } from "./batch";

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

describe("downloadFile", () => {
  const mockBlob = new Blob(["test content"], { type: "application/json" });
  const mockUrl = "blob:test-url";

  beforeEach(() => {
    vi.stubGlobal("fetch", vi.fn());
    vi.stubGlobal(
      "URL",
      Object.assign({}, window.URL, {
        createObjectURL: vi.fn(() => mockUrl),
        revokeObjectURL: vi.fn(),
      }),
    );
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("downloads file successfully and triggers download", async () => {
    const mockResponse = {
      ok: true,
      blob: vi.fn().mockResolvedValue(mockBlob),
    };
    vi.mocked(fetch).mockResolvedValue(mockResponse as unknown as Response);

    const appendChildSpy = vi.spyOn(document.body, "appendChild");
    const removeChildSpy = vi.spyOn(document.body, "removeChild");

    await downloadFile("/api/test", "test.jsonl");

    expect(fetch).toHaveBeenCalledWith("/api/test", expect.any(Object));
    expect(appendChildSpy).toHaveBeenCalled();
    expect(removeChildSpy).toHaveBeenCalled();
  });

  it("throws error when download fails", async () => {
    const mockResponse = {
      ok: false,
      status: 404,
    };
    vi.mocked(fetch).mockResolvedValue(mockResponse as unknown as Response);

    await expect(downloadFile("/api/test", "test.jsonl")).rejects.toThrow(
      "Download failed: 404",
    );
  });
});
