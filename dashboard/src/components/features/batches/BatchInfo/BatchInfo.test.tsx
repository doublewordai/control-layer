import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, within } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import BatchInfo from "./BatchInfo";
import * as hooks from "../../../../api/control-layer/hooks";

vi.mock("../../../../api/control-layer/hooks", () => ({
  useBatch: vi.fn(),
  useBatchAnalytics: vi.fn(),
  useRetryBatch: vi.fn(),
}));

vi.mock("../../../../utils/authorization", () => ({
  useAuthorization: vi.fn(() => ({
    userRoles: ["PlatformManager"],
    isLoading: false,
    hasPermission: () => true,
    canAccessRoute: () => true,
    getFirstAccessibleRoute: () => "/batches",
  })),
}));

vi.mock("../../../../utils/batch", () => ({
  getBatchDownloadFilename: vi.fn(() => "batch.jsonl"),
  downloadFile: vi.fn(),
}));

vi.mock("./BatchResults", () => ({
  default: () => <div data-testid="batch-results" />,
}));

const mockBatch = {
  id: "batch-1",
  object: "batch",
  endpoint: "/v1/chat/completions",
  errors: null,
  input_file_id: "file-1",
  completion_window: "24h",
  status: "completed" as const,
  output_file_id: "file-output-1",
  error_file_id: null,
  created_at: 1730822400,
  in_progress_at: 1730824200,
  expires_at: 1731427200,
  finalizing_at: 1730865600,
  completed_at: 1730869200,
  failed_at: null,
  expired_at: null,
  cancelling_at: null,
  cancelled_at: null,
  request_counts: {
    total: 250,
    completed: 248,
    failed: 2,
  },
  metadata: {},
};

function renderBatchInfo() {
  const queryClient = new QueryClient({
    defaultOptions: {
      queries: { retry: false },
      mutations: { retry: false },
    },
  });

  return render(
    <QueryClientProvider client={queryClient}>
      <MemoryRouter initialEntries={["/batches/batch-1"]}>
        <Routes>
          <Route path="/batches/:batchId" element={<BatchInfo />} />
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("BatchInfo", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    vi.mocked(hooks.useBatch).mockReturnValue({
      data: mockBatch,
      isLoading: false,
      error: null,
    } as never);
    vi.mocked(hooks.useRetryBatch).mockReturnValue({
      mutate: vi.fn(),
      isPending: false,
    } as never);
  });

  it("hides the reasoning token card when the total is zero", () => {
    vi.mocked(hooks.useBatchAnalytics).mockReturnValue({
      data: {
        total_requests: 250,
        total_prompt_tokens: 1000,
        total_completion_tokens: 500,
        total_reasoning_tokens: 0,
        total_tokens: 1500,
      },
      isLoading: false,
    } as never);

    renderBatchInfo();

    expect(screen.queryByText("Reasoning")).not.toBeInTheDocument();
  });

  it("hides the reasoning token card when the total is undefined", () => {
    vi.mocked(hooks.useBatchAnalytics).mockReturnValue({
      data: {
        total_requests: 250,
        total_prompt_tokens: 1000,
        total_completion_tokens: 500,
        total_tokens: 1500,
      },
      isLoading: false,
    } as never);

    renderBatchInfo();

    expect(screen.queryByText("Reasoning")).not.toBeInTheDocument();
  });

  it("shows the reasoning token card when the total is positive", () => {
    vi.mocked(hooks.useBatchAnalytics).mockReturnValue({
      data: {
        total_requests: 250,
        total_prompt_tokens: 1000,
        total_completion_tokens: 500,
        total_reasoning_tokens: 42,
        total_tokens: 1542,
      },
      isLoading: false,
    } as never);

    renderBatchInfo();

    expect(screen.getByText("Reasoning")).toBeInTheDocument();
    expect(screen.getByText("42")).toBeInTheDocument();
  });

  describe("model row in the Metrics card", () => {
    const analyticsWithRequests = {
      data: {
        total_requests: 250,
        total_prompt_tokens: 1000,
        total_completion_tokens: 500,
        total_tokens: 1500,
      },
      isLoading: false,
    } as never;

    // The Model label's row: the value sits beside the label in the same flex row.
    const modelRow = (container: HTMLElement) =>
      within(container).getByText("Model").parentElement!;

    it("shows the batch's model above the token metrics", () => {
      vi.mocked(hooks.useBatch).mockReturnValue({
        data: { ...mockBatch, model: "deepseek-v4-pro" },
        isLoading: false,
        error: null,
      } as never);
      vi.mocked(hooks.useBatchAnalytics).mockReturnValue(analyticsWithRequests);

      const { container } = renderBatchInfo();

      const row = modelRow(container);
      expect(row).toHaveTextContent("deepseek-v4-pro");
      // Model is the first row of the card, above Prompt.
      const prompt = within(container).getByText("Prompt").parentElement!;
      expect(row.compareDocumentPosition(prompt)).toBe(
        Node.DOCUMENT_POSITION_FOLLOWING,
      );
    });

    it("falls back to metadata.model for batches without the typed field", () => {
      vi.mocked(hooks.useBatch).mockReturnValue({
        data: { ...mockBatch, metadata: { model: "qwen3-235b" } },
        isLoading: false,
        error: null,
      } as never);
      vi.mocked(hooks.useBatchAnalytics).mockReturnValue(analyticsWithRequests);

      const { container } = renderBatchInfo();

      expect(modelRow(container)).toHaveTextContent("qwen3-235b");
    });

    it("shows a dash when the batch has no model label", () => {
      vi.mocked(hooks.useBatchAnalytics).mockReturnValue(analyticsWithRequests);

      const { container } = renderBatchInfo();

      const row = modelRow(container);
      expect(row).toHaveTextContent("Model-");
      expect(row.querySelector("[title]")).toBeNull();
    });
  });
});
