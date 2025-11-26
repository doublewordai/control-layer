import { render, waitFor, within, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
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
import { ModelCombobox } from "./model-combobox";
import type { Model } from "../../api/control-layer/types";

// Mock models data
const mockModels: Model[] = [
  {
    id: "model-1",
    alias: "gpt-4o",
    model_name: "gpt-4o",
    model_type: "CHAT",
    hosted_on: "endpoint-1",
  },
  {
    id: "model-2",
    alias: "gpt-4o-mini",
    model_name: "gpt-4o-mini",
    model_type: "CHAT",
    hosted_on: "endpoint-1",
  },
  {
    id: "model-3",
    alias: "text-embedding-3-large",
    model_name: "text-embedding-3-large",
    model_type: "EMBEDDINGS",
    hosted_on: "endpoint-2",
  },
  {
    id: "model-4",
    alias: "claude-3-opus",
    model_name: "claude-3-opus-20240229",
    model_type: "CHAT",
    hosted_on: "endpoint-3",
  },
];

// Setup MSW server
const server = setupServer(
  http.get("/admin/api/v1/models", ({ request }) => {
    const url = new URL(request.url);
    const search = url.searchParams.get("search");
    const limit = parseInt(url.searchParams.get("limit") || "50", 10);

    let filtered = [...mockModels];

    // Filter by search query
    if (search) {
      filtered = filtered.filter(
        (model) =>
          model.alias.toLowerCase().includes(search.toLowerCase()) ||
          model.model_name.toLowerCase().includes(search.toLowerCase()),
      );
    }

    // Apply limit
    filtered = filtered.slice(0, limit);

    return HttpResponse.json({
      data: filtered,
      total: filtered.length,
      skip: 0,
      limit,
    });
  }),
);

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => {
  server.resetHandlers();
  vi.clearAllMocks();
});
afterAll(() => server.close());

// Test wrapper with QueryClient
function createWrapper() {
  const queryClient = new QueryClient({
    defaultOptions: {
      queries: {
        retry: false,
        gcTime: 0,
      },
    },
  });

  return ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
  );
}

describe("ModelCombobox", () => {
  it("renders with default placeholder", async () => {
    const { container } = render(<ModelCombobox />, {
      wrapper: createWrapper(),
    });

    const combobox = within(container).getByRole("combobox", {
      name: /select model/i,
    });
    expect(combobox).toBeInTheDocument();
    expect(combobox).toHaveTextContent("Select a model...");
  });

  it("renders with custom placeholder", async () => {
    const { container } = render(
      <ModelCombobox placeholder="Choose your model" />,
      {
        wrapper: createWrapper(),
      },
    );

    const combobox = within(container).getByRole("combobox", {
      name: /select model/i,
    });
    expect(combobox).toHaveTextContent("Choose your model");
  });

  it("renders with React node as placeholder", async () => {
    const { container } = render(
      <ModelCombobox
        placeholder={
          <div className="flex items-center">
            <span>Custom Icon</span> Select
          </div>
        }
      />,
      { wrapper: createWrapper() },
    );

    const combobox = within(container).getByRole("combobox", {
      name: /select model/i,
    });
    expect(combobox).toHaveTextContent("Custom Icon Select");
  });

  it("displays selected model alias when value is provided", async () => {
    const { container } = render(<ModelCombobox value="gpt-4o" />, {
      wrapper: createWrapper(),
    });

    await waitFor(() => {
      const combobox = within(container).getByRole("combobox", {
        name: /select model/i,
      });
      expect(combobox).toHaveTextContent("gpt-4o");
    });
  });

  it("opens popover when clicked", async () => {
    const user = userEvent.setup();
    const { container } = render(<ModelCombobox />, {
      wrapper: createWrapper(),
    });

    const combobox = within(container).getByRole("combobox", {
      name: /select model/i,
    });
    await user.click(combobox);

    // Popover should be open
    expect(combobox).toHaveAttribute("aria-expanded", "true");

    // Search input should be visible
    await waitFor(() => {
      expect(
        // assert screen since popover renders outside of container
        screen.getByPlaceholderText("Search models..."),
      ).toBeInTheDocument();
    });
  });

  it("displays all models when opened", async () => {
    const user = userEvent.setup();
    const { container } = render(<ModelCombobox />, {
      wrapper: createWrapper(),
    });

    const combobox = within(container).getByRole("combobox", {
      name: /select model/i,
    });
    await user.click(combobox);

    // Wait for models to load and display
    await waitFor(() => {
      expect(screen.getByText("gpt-4o")).toBeInTheDocument();
      expect(screen.getByText("gpt-4o-mini")).toBeInTheDocument();
      expect(screen.getByText("text-embedding-3-large")).toBeInTheDocument();
      expect(screen.getByText("claude-3-opus")).toBeInTheDocument();
    });
  });

  it("filters models based on search query", async () => {
    const user = userEvent.setup();
    const { container } = render(<ModelCombobox />, {
      wrapper: createWrapper(),
    });

    const combobox = within(container).getByRole("combobox", {
      name: /select model/i,
    });
    await user.click(combobox);

    // Wait for models to load
    await waitFor(() => {
      expect(screen.getByText("gpt-4o")).toBeInTheDocument();
    });

    // Type search query
    const searchInput = screen.getByPlaceholderText("Search models...");
    await user.type(searchInput, "claude");

    // Should show filtered results (debounced)
    await waitFor(
      () => {
        expect(screen.getByText("claude-3-opus")).toBeInTheDocument();
        expect(screen.queryByText("gpt-4o")).not.toBeInTheDocument();
      },
      { timeout: 1000 },
    );
  });

  it("calls onValueChange when model is selected", async () => {
    const user = userEvent.setup();
    const handleChange = vi.fn();

    const { container } = render(
      <ModelCombobox onValueChange={handleChange} />,
      {
        wrapper: createWrapper(),
      },
    );

    const combobox = within(container).getByRole("combobox", {
      name: /select model/i,
    });
    await user.click(combobox);

    // Wait for models to load
    await waitFor(() => {
      expect(screen.getByText("gpt-4o")).toBeInTheDocument();
    });

    // Click on a model
    await user.click(screen.getByText("gpt-4o"));

    // Should call onValueChange with the model alias
    expect(handleChange).toHaveBeenCalledWith("gpt-4o");
    expect(handleChange).toHaveBeenCalledTimes(1);
  });

  it("closes popover after model selection", async () => {
    const user = userEvent.setup();
    const { container } = render(<ModelCombobox />, {
      wrapper: createWrapper(),
    });

    const combobox = within(container).getByRole("combobox", {
      name: /select model/i,
    });
    await user.click(combobox);

    // Wait for models to load
    await waitFor(() => {
      expect(screen.getByText("gpt-4o")).toBeInTheDocument();
    });

    // Click on a model
    await user.click(screen.getByText("gpt-4o"));

    // Popover should close
    await waitFor(() => {
      expect(combobox).toHaveAttribute("aria-expanded", "false");
    });
  });

  it("applies filter function to models", async () => {
    const user = userEvent.setup();
    const chatOnlyFilter = (model: Model) => model.model_type === "CHAT";

    const { container } = render(<ModelCombobox filterFn={chatOnlyFilter} />, {
      wrapper: createWrapper(),
    });

    const combobox = within(container).getByRole("combobox", {
      name: /select model/i,
    });
    await user.click(combobox);

    // Wait for models to load
    await waitFor(() => {
      expect(screen.getByText("gpt-4o")).toBeInTheDocument();
    });

    // Should show only chat models
    expect(screen.getByText("gpt-4o")).toBeInTheDocument();
    expect(screen.getByText("gpt-4o-mini")).toBeInTheDocument();
    expect(screen.getByText("claude-3-opus")).toBeInTheDocument();

    // Should not show embeddings model
    expect(
      screen.queryByText("text-embedding-3-large"),
    ).not.toBeInTheDocument();
  });

  it("uses custom search placeholder", async () => {
    const user = userEvent.setup();
    const { container } = render(
      <ModelCombobox searchPlaceholder="Find a model..." />,
      {
        wrapper: createWrapper(),
      },
    );

    const combobox = within(container).getByRole("combobox", {
      name: /select model/i,
    });
    await user.click(combobox);

    // Should show custom search placeholder
    await waitFor(() => {
      expect(
        screen.getByPlaceholderText("Find a model..."),
      ).toBeInTheDocument();
    });
  });

  it("shows empty message when no models match search", async () => {
    const user = userEvent.setup();
    const { container } = render(
      <ModelCombobox emptyMessage="No matching models found" />,
      {
        wrapper: createWrapper(),
      },
    );

    const combobox = within(container).getByRole("combobox", {
      name: /select model/i,
    });
    await user.click(combobox);

    // Wait for models to load
    await waitFor(() => {
      expect(screen.getByText("gpt-4o")).toBeInTheDocument();
    });

    // Type search query that matches no models
    const searchInput = screen.getByPlaceholderText("Search models...");
    await user.type(searchInput, "nonexistent-model-xyz");

    // Should show custom empty message (debounced)
    await waitFor(
      () => {
        expect(
          screen.getByText("No matching models found"),
        ).toBeInTheDocument();
      },
      { timeout: 1000 },
    );
  });

  it("applies custom className", async () => {
    const { container } = render(<ModelCombobox className="custom-width" />, {
      wrapper: createWrapper(),
    });

    const combobox = within(container).getByRole("combobox", {
      name: /select model/i,
    });
    expect(combobox).toHaveClass("custom-width");
  });

  it("debounces search input", async () => {
    const user = userEvent.setup();
    const { container } = render(<ModelCombobox />, {
      wrapper: createWrapper(),
    });

    const combobox = within(container).getByRole("combobox", {
      name: /select model/i,
    });
    await user.click(combobox);

    // Wait for initial models to load
    await waitFor(() => {
      expect(screen.getByText("gpt-4o")).toBeInTheDocument();
    });

    const searchInput = screen.getByPlaceholderText("Search models...");

    // Type quickly (should debounce)
    await user.type(searchInput, "gp");

    // Should still show all models immediately (debounced)
    expect(screen.getByText("claude-3-opus")).toBeInTheDocument();

    // After debounce delay, should filter
    await waitFor(
      () => {
        expect(screen.queryByText("claude-3-opus")).not.toBeInTheDocument();
      },
      { timeout: 500 },
    );
  });

  it("passes additional query options to useModels", async () => {
    const user = userEvent.setup();

    // The component should work with query options
    // We can't directly test the query options being passed,
    // but we can verify the component renders correctly with them
    const { container } = render(
      <ModelCombobox queryOptions={{ accessible: true }} />,
      {
        wrapper: createWrapper(),
      },
    );

    const combobox = within(container).getByRole("combobox", {
      name: /select model/i,
    });
    await user.click(combobox);

    // Should still load and display models
    await waitFor(() => {
      expect(screen.getByText("gpt-4o")).toBeInTheDocument();
    });
  });
});
