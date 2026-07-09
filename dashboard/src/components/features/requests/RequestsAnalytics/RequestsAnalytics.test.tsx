import { render, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { setupServer } from "msw/node";
import { http, HttpResponse } from "msw";
import { ReactNode } from "react";
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { RequestsAnalytics } from "./RequestsAnalytics";

const aggregateResponse = {
  total_requests: 12,
  status_codes: [{ status: "200", count: 12, percentage: 100 }],
  models: [{ model: "claude-sonnet-3.5", count: 12 }],
  time_series: [
    {
      timestamp: new Date("2026-07-09T12:00:00.000Z").toISOString(),
      requests: 12,
      input_tokens: 1200,
      output_tokens: 600,
      avg_latency_ms: 120,
      p95_latency_ms: 200,
      p99_latency_ms: 320,
    },
  ],
};

const pendingServiceTierQueries: Array<string | null> = [];

const server = setupServer(
  http.get("/admin/api/v1/requests/aggregate", () => {
    return HttpResponse.json(aggregateResponse);
  }),
  http.get("/admin/api/v1/models", () => {
    return HttpResponse.json({
      data: [
        {
          id: "model-1",
          alias: "claude-sonnet-3.5",
          model_name: "claude-sonnet-3.5",
        },
      ],
      total_count: 1,
      skip: 0,
      limit: 100,
    });
  }),
  http.get(
    "/admin/api/v1/monitoring/pending-request-counts",
    ({ request }) => {
      const url = new URL(request.url);
      pendingServiceTierQueries.push(url.searchParams.get("service_tiers"));

      return HttpResponse.json({
        "claude-sonnet-3.5": { "1h": 2, "24h": 4 },
      });
    },
  ),
);

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => {
  pendingServiceTierQueries.length = 0;
  server.resetHandlers();
});
afterAll(() => server.close());

function createWrapper() {
  const queryClient = new QueryClient({
    defaultOptions: {
      queries: {
        retry: false,
      },
    },
  });

  return ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
  );
}

describe("RequestsAnalytics", () => {
  it("sends selected pending service tiers to the pending count query", async () => {
    const user = userEvent.setup();
    const { container } = render(
      <RequestsAnalytics selectedModel="claude-sonnet-3.5" />,
      { wrapper: createWrapper() },
    );
    const view = within(container);

    await waitFor(() => {
      expect(pendingServiceTierQueries).toContain("batch");
    });

    const batchToggle = await view.findByRole("button", { name: "Batch" });
    const flexToggle = view.getByRole("button", { name: "Flex" });

    expect(batchToggle).toHaveAttribute("aria-pressed", "true");
    expect(flexToggle).toHaveAttribute("aria-pressed", "false");

    await user.click(flexToggle);

    await waitFor(() => {
      expect(pendingServiceTierQueries).toContain("batch,flex");
    });

    await user.click(batchToggle);

    await waitFor(() => {
      expect(pendingServiceTierQueries).toContain("flex");
    });
  });
});
