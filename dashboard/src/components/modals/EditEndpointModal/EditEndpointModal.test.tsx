import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { setupServer } from "msw/node";
import { http, HttpResponse } from "msw";
import {
  vi,
  describe,
  it,
  expect,
  beforeAll,
  afterEach,
  afterAll,
  beforeEach,
} from "vitest";
import React from "react";
import { handlers } from "../../../api/clay/mocks/handlers";
import { EditEndpointModal } from "./EditEndpointModal";
import type { Endpoint } from "../../../api/clay/types";

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
    <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
  );
}

const mockEndpoint: Endpoint = {
  id: "a1b2c3d4-e5f6-7890-1234-567890abcdef",
  name: "Test Endpoint",
  description: "Test endpoint description",
  url: "https://api.example.com/v1",
  created_by: "test-user",
  created_at: "2024-01-01T00:00:00Z",
  updated_at: "2024-01-01T00:00:00Z",
  requires_api_key: true,
  model_filter: ["model1", "model2"],
};

describe("EditEndpointModal", () => {
  const mockOnClose = vi.fn();
  const mockOnSuccess = vi.fn();

  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("does not render when isOpen is false", () => {
    render(
      <EditEndpointModal
        isOpen={false}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    expect(screen.queryByText("Edit Endpoint")).not.toBeInTheDocument();
  });

  it("renders modal when isOpen is true", () => {
    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    expect(screen.getByText("Edit Endpoint")).toBeInTheDocument();
    expect(screen.getByDisplayValue(mockEndpoint.name)).toBeInTheDocument();
    expect(screen.getByDisplayValue(mockEndpoint.url)).toBeInTheDocument();
    expect(
      screen.getByDisplayValue(mockEndpoint.description!),
    ).toBeInTheDocument();
  });

  it("initializes form with endpoint data", () => {
    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    // Check that form fields are populated
    expect(screen.getByDisplayValue("Test Endpoint")).toBeInTheDocument();
    expect(
      screen.getByDisplayValue("https://api.example.com/v1"),
    ).toBeInTheDocument();
    expect(
      screen.getByDisplayValue("Test endpoint description"),
    ).toBeInTheDocument();
  });

  it("shows API key field when endpoint requires API key", () => {
    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    expect(screen.getByText(/API Key/)).toBeInTheDocument();
    expect(
      screen.getByText("Leave empty to keep existing key"),
    ).toBeInTheDocument();
    expect(screen.getByPlaceholderText("sk-...")).toBeInTheDocument();
  });

  it("does not show API key field when endpoint does not require API key", () => {
    const endpointWithoutApiKey = { ...mockEndpoint, requires_api_key: false };

    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={endpointWithoutApiKey}
      />,
      { wrapper: createWrapper() },
    );

    expect(screen.queryByText(/API Key/)).not.toBeInTheDocument();
    expect(screen.queryByPlaceholderText("sk-...")).not.toBeInTheDocument();
  });

  it("closes modal when close button is clicked", () => {
    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    fireEvent.click(screen.getByRole("button", { name: /close/i }));
    expect(mockOnClose).toHaveBeenCalledOnce();
  });

  it("closes modal when cancel is clicked", () => {
    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(mockOnClose).toHaveBeenCalledOnce();
  });

  it("shows Configure Models button initially", () => {
    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    expect(
      screen.getByRole("button", { name: /Configure Models/i }),
    ).toBeInTheDocument();
    expect(
      screen.getByText("Configure which models to sync from this endpoint"),
    ).toBeInTheDocument();
  });

  it("validates URL changes and shows warning", async () => {
    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    const urlInput = screen.getByDisplayValue(mockEndpoint.url);
    fireEvent.change(urlInput, { target: { value: "https://new-url.com/v1" } });

    await waitFor(() => {
      expect(
        screen.getByText("(Changed - requires testing)"),
      ).toBeInTheDocument();
    });
  });

  it("shows Test Connection button when URL changes", async () => {
    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    const urlInput = screen.getByDisplayValue(mockEndpoint.url);
    fireEvent.change(urlInput, { target: { value: "https://new-url.com/v1" } });

    await waitFor(() => {
      expect(
        screen.getByRole("button", { name: /Test Connection/i }),
      ).toBeInTheDocument();
    });
  });

  it("handles refresh models button click", async () => {
    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    const configureButton = screen.getByRole("button", {
      name: /Configure Models/i,
    });
    fireEvent.click(configureButton);

    await waitFor(() => {
      expect(screen.getByText("Loading...")).toBeInTheDocument();
    });
  });

  it("shows validation success state after successful model fetch", async () => {
    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    const configureButton = screen.getByRole("button", {
      name: /Configure Models/i,
    });
    fireEvent.click(configureButton);

    await waitFor(() => {
      // Look for the success message that includes "Models refreshed"
      expect(screen.getByText(/Models refreshed/i)).toBeInTheDocument();
    });
  });

  it("shows validation error state on failed model fetch", async () => {
    // Mock validation error
    server.use(
      http.post("/admin/api/v1/endpoints/validate", () => {
        return HttpResponse.json({
          status: "error",
          error: "Connection failed",
        });
      }),
    );

    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    const configureButton = screen.getByRole("button", {
      name: /Configure Models/i,
    });
    fireEvent.click(configureButton);

    await waitFor(() => {
      expect(screen.getByText("Connection Failed")).toBeInTheDocument();
      expect(screen.getByText("Connection failed")).toBeInTheDocument();
    });
  });

  it("shows model selection after successful validation", async () => {
    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    const configureButton = screen.getByRole("button", {
      name: /Configure Models/i,
    });
    fireEvent.click(configureButton);

    await waitFor(() => {
      expect(screen.getByText("Model Settings")).toBeInTheDocument();
      expect(
        screen.getByText(/Select which models to sync/),
      ).toBeInTheDocument();
      expect(
        screen.getByRole("button", { name: /Select All|Deselect All/i }),
      ).toBeInTheDocument();
    });
  });

  it("handles model selection/deselection", async () => {
    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    const configureButton = screen.getByRole("button", {
      name: /Configure Models/i,
    });
    fireEvent.click(configureButton);

    await waitFor(() => {
      expect(screen.getByText("Model Settings")).toBeInTheDocument();
    });

    // Find model checkboxes and click one
    const checkboxes = screen.getAllByRole("checkbox");
    expect(checkboxes.length).toBeGreaterThan(0);

    fireEvent.click(checkboxes[0]);

    // The checkbox state should have changed
    // This is a basic test - in a real scenario we'd verify the selection count changes
  });

  it("handles select all/deselect all functionality", async () => {
    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    const configureButton = screen.getByRole("button", {
      name: /Configure Models/i,
    });
    fireEvent.click(configureButton);

    await waitFor(() => {
      expect(screen.getByText("Model Settings")).toBeInTheDocument();
    });

    const selectAllButton = screen.getByRole("button", {
      name: /Select All|Deselect All/i,
    });
    fireEvent.click(selectAllButton);

    // This would change the selection state
  });

  it("requires name field for update", async () => {
    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    // Clear the name field
    const nameInput = screen.getByDisplayValue("Test Endpoint");
    fireEvent.change(nameInput, { target: { value: "" } });

    const updateButton = screen.getByRole("button", {
      name: /Update Endpoint/i,
    });

    // Button should be disabled when name is empty
    expect(updateButton).toBeDisabled();

    // We can't test the error message without clicking because the button is disabled
    // But we can verify the disabled state is working correctly
    expect(mockOnSuccess).not.toHaveBeenCalled();
  });

  it("requires validation after URL change", async () => {
    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    // Change URL
    const urlInput = screen.getByDisplayValue(mockEndpoint.url);
    fireEvent.change(urlInput, { target: { value: "https://new-url.com/v1" } });

    await waitFor(() => {
      expect(
        screen.getByText("(Changed - requires testing)"),
      ).toBeInTheDocument();
    });

    const updateButton = screen.getByRole("button", {
      name: /Update Endpoint/i,
    });

    // Button should be disabled when URL changed but not validated
    expect(updateButton).toBeDisabled();

    expect(mockOnSuccess).not.toHaveBeenCalled();
  });

  it("successfully updates endpoint", async () => {
    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    // Change name
    const nameInput = screen.getByDisplayValue("Test Endpoint");
    fireEvent.change(nameInput, { target: { value: "Updated Endpoint Name" } });

    const updateButton = screen.getByRole("button", {
      name: /Update Endpoint/i,
    });
    fireEvent.click(updateButton);

    await waitFor(() => {
      expect(mockOnSuccess).toHaveBeenCalledOnce();
      expect(mockOnClose).toHaveBeenCalledOnce();
    });
  });

  it("handles update errors", async () => {
    // Mock update error
    server.use(
      http.patch("/admin/api/v1/endpoints/*", () => {
        return HttpResponse.json({ error: "Update failed" }, { status: 500 });
      }),
    );

    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    const updateButton = screen.getByRole("button", {
      name: /Update Endpoint/i,
    });
    fireEvent.click(updateButton);

    await waitFor(() => {
      expect(
        screen.getByText(/Failed to update endpoint/i),
      ).toBeInTheDocument();
    });

    expect(mockOnSuccess).not.toHaveBeenCalled();
  });

  it("handles enter key for validation", async () => {
    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    // Change URL to trigger validation requirement
    const urlInput = screen.getByDisplayValue(mockEndpoint.url);
    fireEvent.change(urlInput, { target: { value: "https://new-url.com/v1" } });

    // Press Enter to trigger validation
    fireEvent.keyDown(urlInput, { key: "Enter", code: "Enter" });

    await waitFor(() => {
      expect(screen.getByText("Testing...")).toBeInTheDocument();
    });
  });
});
