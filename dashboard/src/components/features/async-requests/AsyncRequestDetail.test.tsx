import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter, Routes, Route } from "react-router-dom";
import { AsyncRequestDetail } from "./AsyncRequestDetail";
import * as hooks from "../../../api/control-layer/hooks";

vi.mock("../../../api/control-layer/hooks", () => ({
  useAsyncRequest: vi.fn(),
  useRetryBatchRequests: vi.fn(() => ({
    mutateAsync: vi.fn(),
    isPending: false,
  })),
}));

vi.mock("sonner", () => ({
  toast: { success: vi.fn(), error: vi.fn() },
}));

const renderDetail = (requestId: string) => {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <QueryClientProvider client={queryClient}>
      <MemoryRouter initialEntries={[`/responses/${requestId}`]}>
        <Routes>
          <Route path="/responses/:requestId" element={<AsyncRequestDetail />} />
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
};

const failedRequest = () => ({
  id: "req-123",
  status: "failed",
  body: "{}",
  response_body: null,
  error: JSON.stringify({
    type: "NonRetriableHttpStatus",
    details: { status: 503, body: "" },
  }),
  model: "test-model",
  service_tier: "flex",
  created_at: "2026-01-01T00:00:00.000Z",
});

describe("AsyncRequestDetail error card", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("renders a synthesized message for an empty-body 503 FailureReason, not the raw envelope", () => {
    vi.mocked(hooks.useAsyncRequest).mockReturnValue({
      data: failedRequest() as any,
      isLoading: false,
    } as any);

    renderDetail("req-123");

    const card = screen.getByText(/Request failed/);
    expect(card).toBeInTheDocument();
    expect(card).toHaveTextContent("Request failed (503)");

    const errorCard = card.closest(".bg-red-50");
    expect(errorCard).not.toBeNull();
    expect(errorCard!.textContent).not.toContain('"type"');
    expect(errorCard!.textContent).not.toContain("NonRetriableHttpStatus");
    expect(errorCard!.textContent).not.toContain('{"type"');
  });
});
