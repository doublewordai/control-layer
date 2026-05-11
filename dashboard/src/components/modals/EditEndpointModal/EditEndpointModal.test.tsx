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
import { handlers } from "../../../api/control-layer/mocks/handlers";
import { EditEndpointModal } from "./EditEndpointModal";
import type { Endpoint } from "../../../api/control-layer/types";

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
  auth_header_name: "Authorization",
  auth_header_prefix: "Bearer ",
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
    // assert modal on screen as it renders outside of container
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
    // URL should be visible in Step 1
    expect(screen.getByDisplayValue(mockEndpoint.url)).toBeInTheDocument();
    // Stepper should show we're on Connection step
    expect(screen.getByText("Connection")).toBeInTheDocument();
    expect(screen.getByText("Models")).toBeInTheDocument();
  });

  it("initializes form with endpoint data", async () => {
    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    // Step 1: URL should be populated
    expect(
      screen.getByDisplayValue("https://api.example.com/v1"),
    ).toBeInTheDocument();

    // Discover models to navigate to Step 2
    const discoverButton = screen.getByRole("button", {
      name: /Discover Models/i,
    });
    fireEvent.click(discoverButton);

    await waitFor(() => {
      // Check that form fields are populated in Step 2
      expect(screen.getByDisplayValue("Test Endpoint")).toBeInTheDocument();
      expect(
        screen.getByDisplayValue("Test endpoint description"),
      ).toBeInTheDocument();
    });
  });

  it("shows API key field with hint when endpoint requires API key", () => {
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

  it("shows API key field without hint when endpoint does not require API key", () => {
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

    expect(screen.getByText(/API Key/)).toBeInTheDocument();
    expect(screen.getByPlaceholderText("sk-...")).toBeInTheDocument();
    expect(
      screen.queryByText("Leave empty to keep existing key"),
    ).not.toBeInTheDocument();
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

  it("shows Discover Models button on Step 1", () => {
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
      screen.getByRole("button", { name: /Discover Models/i }),
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

  it("shows Discover Models button when URL changes", async () => {
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
      // Button should still say "Discover Models" when auto-discover is on
      expect(
        screen.getByRole("button", { name: /Discover Models/i }),
      ).toBeInTheDocument();
    });
  });

  it("handles discover models button click", async () => {
    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    const discoverButton = screen.getByRole("button", {
      name: /Discover Models/i,
    });
    fireEvent.click(discoverButton);

    await waitFor(() => {
      // Should show loading state
      expect(screen.getByText(/Testing Connection.../i)).toBeInTheDocument();
    });
  });

  it("navigates to Step 2 after successful discovery", async () => {
    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    const discoverButton = screen.getByRole("button", {
      name: /Discover Models/i,
    });
    fireEvent.click(discoverButton);

    await waitFor(() => {
      // Step 2 surfaces the imported models heading and the Update button
      expect(screen.getByText(/Imported models/i)).toBeInTheDocument();
      expect(
        screen.getByRole("button", { name: /Update Endpoint/i }),
      ).toBeInTheDocument();
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

    const discoverButton = screen.getByRole("button", {
      name: /Discover Models/i,
    });
    fireEvent.click(discoverButton);

    await waitFor(() => {
      // Should stay on Step 1 and show error message
      expect(screen.getAllByText("Connection failed").length).toBeGreaterThan(
        0,
      );
    });
  });

  it("shows the Add model trigger and imported models on Step 2", async () => {
    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    const discoverButton = screen.getByRole("button", {
      name: /Discover Models/i,
    });
    fireEvent.click(discoverButton);

    await waitFor(() => {
      expect(screen.getByText(/Imported models/i)).toBeInTheDocument();
      expect(
        screen.getByRole("button", { name: /Add model/i }),
      ).toBeInTheDocument();
    });
  });

  it("opens the add palette and adds a manually-typed model name", async () => {
    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    fireEvent.click(
      screen.getByRole("button", { name: /Discover Models/i }),
    );
    await waitFor(() =>
      expect(screen.getByText(/Imported models/i)).toBeInTheDocument(),
    );

    fireEvent.click(screen.getByRole("button", { name: /Add model/i }));

    // Palette renders inside a portal — use screen
    const input = await waitFor(() =>
      screen.getByPlaceholderText(/type a (model )?name/i),
    );
    fireEvent.change(input, { target: { value: "my-custom-model" } });

    const manualOption = await waitFor(() =>
      screen.getByText(/Add manually:/i),
    );
    fireEvent.click(manualOption);

    await waitFor(() => {
      // Newly-added row appears with a "new" badge
      expect(screen.getByText("my-custom-model")).toBeInTheDocument();
      expect(screen.getByText(/^new$/i)).toBeInTheDocument();
    });
  });

  it("flags case-only and whitespace-only alias collisions before save (matches backend LOWER(alias))", async () => {
    render(
      <EditEndpointModal
        isOpen={true}
        onClose={mockOnClose}
        onSuccess={mockOnSuccess}
        endpoint={mockEndpoint}
      />,
      { wrapper: createWrapper() },
    );

    fireEvent.click(screen.getByRole("button", { name: /Discover Models/i }));
    await waitFor(() =>
      expect(screen.getByText(/Imported models/i)).toBeInTheDocument(),
    );

    // Add two models manually so we have two rows whose aliases we control.
    const addRow = async (name: string) => {
      fireEvent.click(screen.getByRole("button", { name: /Add model/i }));
      const input = await waitFor(() =>
        screen.getByPlaceholderText(/type a (model )?name/i),
      );
      fireEvent.change(input, { target: { value: name } });
      fireEvent.click(await waitFor(() => screen.getByText(/Add manually:/i)));
      await waitFor(() => expect(screen.getByText(name)).toBeInTheDocument());
    };

    await addRow("model-one");
    await addRow("model-two");

    const aliasOne = screen.getByLabelText("Alias for model-one");
    const aliasTwo = screen.getByLabelText("Alias for model-two");

    // Case-only collision: "Test" and "test" share LOWER(alias) on the backend.
    fireEvent.change(aliasOne, { target: { value: "Test" } });
    fireEvent.change(aliasTwo, { target: { value: "test" } });

    await waitFor(() => {
      expect(
        screen.getByText(/Duplicate aliases detected/i),
      ).toBeInTheDocument();
    });

    // Whitespace-only collision: "  test  " trims to "test", same as the other row.
    fireEvent.change(aliasOne, { target: { value: "  test  " } });

    await waitFor(() => {
      expect(
        screen.getByText(/Duplicate aliases detected/i),
      ).toBeInTheDocument();
    });

    // Disambiguating clears the warning.
    fireEvent.change(aliasOne, { target: { value: "unique-alias" } });

    await waitFor(() => {
      expect(
        screen.queryByText(/Duplicate aliases detected/i),
      ).not.toBeInTheDocument();
    });
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

    // Discover models to navigate to Step 2
    const discoverButton = screen.getByRole("button", {
      name: /Discover Models/i,
    });
    fireEvent.click(discoverButton);

    await waitFor(() => {
      expect(screen.getByDisplayValue("Test Endpoint")).toBeInTheDocument();
    });

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

    // Change URL in Step 1
    const urlInput = screen.getByDisplayValue(mockEndpoint.url);
    fireEvent.change(urlInput, { target: { value: "https://new-url.com/v1" } });

    await waitFor(() => {
      expect(
        screen.getByText("(Changed - requires testing)"),
      ).toBeInTheDocument();
    });

    // Try to navigate to Step 2 without validation - button should be "Discover Models" not "Next"
    // The Discover Models button should work, but we need to test that we can't skip validation
    // For now, let's just verify the warning appears
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

    // Discover models to navigate to Step 2
    const discoverButton = screen.getByRole("button", {
      name: /Discover Models/i,
    });
    fireEvent.click(discoverButton);

    await waitFor(() => {
      expect(screen.getByDisplayValue("Test Endpoint")).toBeInTheDocument();
    });

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

    // Discover models to navigate to Step 2
    const discoverButton = screen.getByRole("button", {
      name: /Discover Models/i,
    });
    fireEvent.click(discoverButton);

    await waitFor(() => {
      expect(screen.getByDisplayValue("Test Endpoint")).toBeInTheDocument();
    });

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

  it("handles discover models flow", async () => {
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

    await waitFor(() => {
      expect(
        screen.getByText("(Changed - requires testing)"),
      ).toBeInTheDocument();
    });

    // Click the "Discover Models" button
    const discoverButton = screen.getByRole("button", {
      name: /Discover Models/i,
    });
    fireEvent.click(discoverButton);

    await waitFor(() => {
      expect(screen.getByText(/Testing Connection.../i)).toBeInTheDocument();
    });
  });
});
