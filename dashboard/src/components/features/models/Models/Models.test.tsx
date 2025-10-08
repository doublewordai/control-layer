import { render, screen, waitFor } from "@testing-library/react";
import { BrowserRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { setupServer } from "msw/node";
import { http, HttpResponse } from "msw";
import { ReactNode } from "react";
import { describe, it, expect, beforeAll, afterEach, afterAll } from "vitest";
import Models from "./Models";
import { handlers } from "../../../../api/clay/mocks/handlers";

// Setup MSW server
const server = setupServer(...handlers);

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

// Test wrapper with QueryClient and Router
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
      <BrowserRouter>{children}</BrowserRouter>
    </QueryClientProvider>
  );
}

describe("Models Component", () => {
  it("renders without crashing", async () => {
    render(<Models />, { wrapper: createWrapper() });

    // Should show loading state initially
    expect(screen.getByText("Loading model usage data...")).toBeInTheDocument();

    // Should render the component after loading
    await waitFor(() => {
      expect(screen.getByText("Models")).toBeInTheDocument();
    });
  });

  it("renders models data when loaded", async () => {
    render(<Models />, { wrapper: createWrapper() });

    await waitFor(() => {
      // Check that the page header is displayed
      expect(screen.getByText("Models")).toBeInTheDocument();
      expect(
        screen.getByText(/View available models by provider/),
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

    render(<Models />, { wrapper: createWrapper() });

    await waitFor(() => {
      expect(screen.getByText(/Error:/)).toBeInTheDocument();
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

    render(<Models />, { wrapper: createWrapper() });

    await waitFor(() => {
      expect(screen.getByText("Models")).toBeInTheDocument();
      // Should still render the page structure even with no models
      expect(
        screen.getByText(/View available models by provider/),
      ).toBeInTheDocument();
    });
  });
});
