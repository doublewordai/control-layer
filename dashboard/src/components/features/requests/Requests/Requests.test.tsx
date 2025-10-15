import { render, screen, waitFor } from "@testing-library/react";
import { BrowserRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { setupServer } from "msw/node";
import { http, HttpResponse } from "msw";
import { ReactNode } from "react";
import {
  describe,
  it,
  expect,
  beforeAll,
  afterEach,
  afterAll,
  vi,
} from "vitest";
import { Requests } from "./Requests";
import { handlers } from "../../../../api/waycast/mocks/handlers";
import { SettingsProvider } from "../../../../contexts";

// Mock the authorization hook to control user permissions
vi.mock("../../../../utils/authorization", () => ({
  useAuthorization: vi.fn(() => ({
    userRoles: ["PlatformManager"], // Default to full permissions
    hasPermission: vi.fn(() => true),
    canAccessRoute: vi.fn(() => true),
  })),
}));

// Get the mocked function for test manipulation
import { useAuthorization } from "../../../../utils/authorization";
const _mockUseAuthorization = vi.mocked(useAuthorization);

// Mock ResizeObserver for chart components
class MockResizeObserver {
  observe() {}
  unobserve() {}
  disconnect() {}
}

global.ResizeObserver = MockResizeObserver;

// Add missing MSW handlers for requests analytics
const requestsAnalyticsHandler = http.get(
  "/admin/api/v1/requests/aggregate",
  () => {
    return HttpResponse.json({
      total_requests: 1250,
      status_codes: [
        { status: "200", count: 1150, percentage: 92.0 },
        { status: "400", count: 75, percentage: 6.0 },
        { status: "500", count: 25, percentage: 2.0 },
      ],
      models: [
        { model: "claude-sonnet-3.5", count: 650 },
        { model: "gpt-4", count: 400 },
        { model: "gpt-3.5-turbo", count: 200 },
      ],
      time_series: [
        {
          timestamp: new Date().toISOString(),
          requests: 100,
          input_tokens: 5000,
          output_tokens: 2000,
          avg_latency_ms: 450,
          p95_latency_ms: 800,
          p99_latency_ms: 1200,
        },
      ],
    });
  },
);

// Add missing MSW handler for requests endpoint
const requestsHandler = http.get("/admin/api/v1/requests", ({ request }) => {
  const url = new URL(request.url);
  const _limit = url.searchParams.get("limit");

  return HttpResponse.json({
    requests: [
      {
        id: "req-1",
        timestamp: new Date().toISOString(),
        request_content: "What is AI?",
        response_content: "AI stands for Artificial Intelligence...",
        model: "claude-sonnet-3.5",
        usage: { prompt_tokens: 10, completion_tokens: 50, total_tokens: 60 },
        duration_ms: 450,
        request_type: "chat_completions",
      },
    ],
  });
});

// Add handler for models endpoint used by RequestsAnalytics
const modelsHandler = http.get("/admin/api/v1/models", () => {
  return HttpResponse.json({
    models: [
      { id: "claude-sonnet-3.5", name: "Claude 3.5 Sonnet" },
      { id: "gpt-4", name: "GPT-4" },
      { id: "gpt-3.5-turbo", name: "GPT-3.5 Turbo" },
    ],
  });
});

// Setup MSW server with all handlers
const server = setupServer(
  ...handlers,
  requestsAnalyticsHandler,
  requestsHandler,
  modelsHandler,
);

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => {
  server.resetHandlers();
  // Reset authorization mock to default state
  vi.mocked(useAuthorization).mockReturnValue({
    userRoles: ["PlatformManager"], // Default to full permissions
    hasPermission: vi.fn(() => true),
    canAccessRoute: vi.fn(() => true),
  });
});
afterAll(() => server.close());

// Test wrapper with QueryClient, Router, and Settings
function createWrapper() {
  const queryClient = new QueryClient({
    defaultOptions: {
      queries: {
        retry: false, // Disable retries for tests
      },
    },
  });

  return ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={queryClient}>
      <BrowserRouter>
        <SettingsProvider>{children}</SettingsProvider>
      </BrowserRouter>
    </QueryClientProvider>
  );
}

describe("Requests Component", () => {
  it("renders without crashing", async () => {
    render(<Requests />, { wrapper: createWrapper() });

    // Should render the main component
    await waitFor(() => {
      expect(
        screen.getByRole("heading", { name: "Traffic" }),
      ).toBeInTheDocument();
    });

    // Should show Analytics tab by default (due to user permissions)
    expect(screen.getByRole("tab", { name: /analytics/i })).toHaveAttribute(
      "aria-selected",
      "true",
    );
  });
});
