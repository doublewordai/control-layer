import { render, waitFor, within } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
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
import Models from "./Models";
import { handlers } from "../../../../api/control-layer/mocks/handlers";
import { SettingsProvider } from "../../../../contexts/settings/SettingsContext";

// Mock the authorization hook
vi.mock("../../../../utils/authorization", () => ({
  useAuthorization: vi.fn(() => ({
    userRoles: ["PlatformManager"],
    hasPermission: vi.fn(() => true),
    canAccessRoute: vi.fn(() => true),
  })),
}));

// Setup MSW server
const server = setupServer(...handlers);

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

// Test wrapper with QueryClient, Router, and Context Providers
function createWrapper() {
  const queryClient = new QueryClient({
    defaultOptions: {
      queries: {
        retry: false, // Disable retries for tests
      },
    },
  });

  return ({ children }: { children: ReactNode }) => (
    <SettingsProvider>
      <QueryClientProvider client={queryClient}>
        <MemoryRouter>{children}</MemoryRouter>
      </QueryClientProvider>
    </SettingsProvider>
  );
}

describe("Models Component", () => {
  it("renders without crashing", async () => {
    const { container } = render(<Models />, { wrapper: createWrapper() });

    // Should show loading state initially
    expect(
      within(container).getByText("Loading model usage data..."),
    ).toBeInTheDocument();

    // Should render the component after loading
    await waitFor(() => {
      expect(
        within(container).getByRole("heading", { name: /models/i }),
      ).toBeInTheDocument();
    });
  });

  it("renders models data when loaded", async () => {
    const { container } = render(<Models />, { wrapper: createWrapper() });

    await waitFor(() => {
      // Check that the page header is displayed
      expect(
        within(container).getByRole("heading", { name: /models/i }),
      ).toBeInTheDocument();
      expect(
        within(container).getByText(/View and monitor your deployed models/),
      ).toBeInTheDocument();
    });
  });

  it("renders error state when models API fails", async () => {
    server.use(
      http.get("/admin/api/v1/models", () => {
        return HttpResponse.json(
          { error: "Failed to fetch models" },
          { status: 500 },
        );
      }),
    );

    const { container } = render(<Models />, { wrapper: createWrapper() });

    await waitFor(() => {
      expect(within(container).getByText(/Error:/)).toBeInTheDocument();
    });
  });

  it("renders empty state when no models exist", async () => {
    server.use(
      http.get("/admin/api/v1/models", () => {
        return HttpResponse.json([]);
      }),
      http.get("/admin/api/v1/endpoints", () => {
        return HttpResponse.json([]);
      }),
    );

    const { container } = render(<Models />, { wrapper: createWrapper() });

    await waitFor(() => {
      expect(
        within(container).getByRole("heading", { name: /models/i }),
      ).toBeInTheDocument();
      // Should still render the page structure even with no models
      expect(
        within(container).getByText(/View and monitor your deployed models/),
      ).toBeInTheDocument();
    });
  });
});
