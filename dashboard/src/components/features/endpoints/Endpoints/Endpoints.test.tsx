import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";
import { setupServer } from "msw/node";
import { http, HttpResponse } from "msw";
import React from "react";
import { describe, it, expect, beforeAll, afterEach, afterAll } from "vitest";
import { handlers } from "../../../../api/waycast/mocks/handlers";
import { Endpoints } from "./Endpoints";

// Setup MSW server
const server = setupServer(...handlers);

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

function createWrapper() {
  const queryClient = new QueryClient({
    defaultOptions: {
      queries: {
        retry: false,
      },
    },
  });

  return ({ children }: { children: React.ReactNode }) => (
    <QueryClientProvider client={queryClient}>
      <MemoryRouter>{children}</MemoryRouter>
    </QueryClientProvider>
  );
}

describe("Endpoints Component", () => {
  it("renders without crashing", async () => {
    render(<Endpoints />, { wrapper: createWrapper() });

    // Check for loading state
    expect(document.querySelector(".animate-pulse")).toBeInTheDocument();

    // Should render after loading
    await waitFor(() => {
      expect(screen.getByText("Endpoints")).toBeInTheDocument();
    });
  });

  it("renders endpoints data when loaded", async () => {
    render(<Endpoints />, { wrapper: createWrapper() });

    await waitFor(() => {
      // Check that endpoint data from mock is displayed
      expect(screen.getByText("Internal")).toBeInTheDocument();
      expect(
        screen.getByText(
          /Manage inference endpoints and their model synchronization/,
        ),
      ).toBeInTheDocument();
    });
  });

  it("renders empty state when no endpoints exist", async () => {
    server.use(
      http.get("/admin/api/v1/endpoints", () => {
        return HttpResponse.json([]);
      }),
    );

    render(<Endpoints />, { wrapper: createWrapper() });

    await waitFor(() => {
      expect(screen.getByText("No endpoints configured")).toBeInTheDocument();
      expect(
        screen.getByText(
          "Add your first inference endpoint to start syncing models",
        ),
      ).toBeInTheDocument();
    });
  });

  it("renders error state when API fails", async () => {
    server.use(
      http.get("/admin/api/v1/endpoints", () => {
        return HttpResponse.json({ error: "Server error" }, { status: 500 });
      }),
    );

    render(<Endpoints />, { wrapper: createWrapper() });

    await waitFor(() => {
      expect(screen.getByText("Error loading endpoints")).toBeInTheDocument();
      expect(
        screen.getByText("Unable to load endpoints. Please try again later."),
      ).toBeInTheDocument();
    });
  });

  it("deletes individual endpoint via dropdown menu", async () => {
    const user = userEvent.setup();
    render(<Endpoints />, { wrapper: createWrapper() });

    // Wait for endpoints to load
    await waitFor(() => {
      expect(
        screen.getByRole("heading", { name: "Endpoints" }),
      ).toBeInTheDocument();
    });

    // Find and click the dropdown menu button for the first endpoint
    const dropdownButtons = screen.getAllByRole("button", {
      name: /open menu/i,
    });
    expect(dropdownButtons.length).toBeGreaterThan(0);

    await user.click(dropdownButtons[0]);

    // Wait for dropdown menu to appear and click delete
    await waitFor(() => {
      expect(screen.getByRole("menu")).toBeInTheDocument();
    });

    const deleteMenuItem = screen.getByRole("menuitem", { name: /delete/i });
    await user.click(deleteMenuItem);

    // DeleteEndpointModal should open
    await waitFor(() => {
      expect(screen.getByRole("dialog")).toBeInTheDocument();
      expect(
        screen.getByRole("heading", { name: "Delete Endpoint" }),
      ).toBeInTheDocument();
    });

    // Should show endpoint details and warning with proper roles
    expect(
      screen.getByRole("group", { name: /endpoint details/i }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("alert", { name: /deletion warning/i }),
    ).toBeInTheDocument();

    // Should have properly labeled cancel and delete buttons
    expect(
      screen.getByRole("button", { name: /cancel deletion/i }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /confirm deletion/i }),
    ).toBeInTheDocument();

    // Cancel should close the modal
    await user.click(screen.getByRole("button", { name: /cancel deletion/i }));

    await waitFor(() => {
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    });
  });
});
