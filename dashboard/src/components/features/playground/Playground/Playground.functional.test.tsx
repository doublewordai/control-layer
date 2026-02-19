import { render, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { setupServer } from "msw/node";
import type { ReactNode } from "react";
import {
  describe,
  it,
  expect,
  beforeAll,
  afterEach,
  afterAll,
  vi,
} from "vitest";
import Playground from "./Playground";
import { handlers } from "../../../../api/control-layer/mocks/handlers";

const server = setupServer(...handlers);

beforeAll(() => {
  server.listen({ onUnhandledRequest: "error" });
  // Mock scrollIntoView for jsdom
  Element.prototype.scrollIntoView = vi.fn();
});
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

const mockOpenAI = {
  chat: {
    completions: {
      create: vi.fn().mockResolvedValue({
        choices: [
          {
            delta: { content: "Hello! How can I help you today?" },
          },
        ],
        async *[Symbol.asyncIterator]() {
          yield { choices: [{ delta: { content: "Hello! " } }] };
          yield { choices: [{ delta: { content: "How can I " } }] };
          yield { choices: [{ delta: { content: "help you today?" } }] };
        },
      }),
    },
  },
  embeddings: {
    create: vi
      .fn()
      .mockResolvedValueOnce({
        data: [
          {
            embedding: [0.8, 0.6, 0.7, 0.5, 0.9], // Mock embedding vector for text A
          },
        ],
      })
      .mockResolvedValueOnce({
        data: [
          {
            embedding: [0.7, 0.5, 0.8, 0.6, 0.8], // Mock embedding vector for text B (similar to A)
          },
        ],
      }),
  },
};

vi.mock("openai", () => ({
  default: vi.fn(() => mockOpenAI),
}));

let queryClient: QueryClient;

function createWrapper(initialEntries = ["/"]) {
  queryClient = new QueryClient({
    defaultOptions: {
      queries: {
        retry: false,
        gcTime: 0,
      },
    },
  });

  return ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={queryClient}>
      <MemoryRouter initialEntries={initialEntries}>{children}</MemoryRouter>
    </QueryClientProvider>
  );
}

describe("Playground Component - Functional Tests", () => {
  afterEach(() => {
    // Clean up QueryClient to prevent state pollution between tests
    if (queryClient) {
      queryClient.clear();
      queryClient.cancelQueries();
    }
  });
  it("loads playground page and shows welcome state", async () => {
    const { container } = render(<Playground />, { wrapper: createWrapper() });

    await waitFor(() => {
      expect(
        within(container).getByRole("main", { name: /welcome to playground/i }),
      ).toBeInTheDocument();
    });

    expect(
      within(container).getByRole("heading", {
        name: /welcome to the playground/i,
      }),
    ).toBeInTheDocument();
    expect(
      within(container).getByRole("combobox", { name: /select model/i }),
    ).toBeInTheDocument();
    expect(
      within(container).getByRole("button", { name: /back to models/i }),
    ).toBeInTheDocument();
  });

  it("enables model selector when models load", async () => {
    const { container } = render(<Playground />, { wrapper: createWrapper() });

    const modelSelect = within(container).getByRole("combobox", {
      name: /select model/i,
    });

    // Should have correct attributes
    expect(modelSelect).toHaveAttribute("aria-expanded", "false");
    expect(modelSelect).toHaveAttribute("role", "combobox");
    // Placeholder text should indicate models can be searched
    expect(modelSelect.textContent).toMatch(/search models|gpt-4o/i);
  });

  it("shows model selector ready for available models", async () => {
    const { container } = render(<Playground />, { wrapper: createWrapper() });

    const modelSelect = within(container).getByRole("combobox", {
      name: /select model/i,
    });

    // Should have proper ARIA attributes indicating it has options available
    expect(modelSelect).toHaveAttribute("aria-label", "Select model");
    expect(modelSelect).toHaveAttribute("aria-expanded", "false"); // Closed but ready

    // Should show placeholder or model name, not loading or error states
    expect(modelSelect).not.toHaveTextContent(/loading/i);
    expect(modelSelect).not.toHaveTextContent(/no models/i);
    expect(modelSelect.textContent).toMatch(/search models|gpt-4o/i);
  });

  it("shows no error messages when models load successfully", async () => {
    const { container } = render(<Playground />, { wrapper: createWrapper() });

    await waitFor(() => {
      expect(
        within(container).getByRole("main", { name: /welcome to playground/i }),
      ).toBeInTheDocument();
    });

    expect(
      within(container).queryByText(/failed to load models/i),
    ).not.toBeInTheDocument();
  });

  it("displays essential elements on mobile viewport", async () => {
    Object.defineProperty(window, "innerWidth", {
      value: 375,
      configurable: true,
    });

    const { container } = render(<Playground />, { wrapper: createWrapper() });

    await waitFor(() => {
      expect(
        within(container).getByRole("main", { name: /welcome to playground/i }),
      ).toBeInTheDocument();
    });

    expect(
      within(container).getByRole("button", { name: /back to models/i }),
    ).toBeInTheDocument();
    expect(
      within(container).getByRole("combobox", { name: /select model/i }),
    ).toBeInTheDocument();
  });

  it("loads chat playground when model parameter is provided", async () => {
    const { container } = render(<Playground />, {
      wrapper: createWrapper(["/?model=gpt-4o"]),
    });

    // Wait for models to load and model to be selected
    await waitFor(() => {
      const modelSelects = within(container).getAllByRole("combobox", {
        name: /select model/i,
      });
      // First combobox should be the main model selector
      expect(modelSelects[0]).toHaveTextContent("gpt-4o");
    });

    // Should successfully load chat playground (no welcome screen)
    expect(
      within(container).queryByRole("main", { name: /welcome to playground/i }),
    ).not.toBeInTheDocument();

    // Should show chat playground header
    expect(
      within(container).getByRole("heading", { name: /chat playground/i }),
    ).toBeInTheDocument();

    // Should show actual chat interface elements
    expect(
      within(container).getByRole("textbox", { name: /message input/i }),
    ).toBeInTheDocument();
    expect(
      within(container).getByRole("button", { name: /send message/i }),
    ).toBeInTheDocument();
    expect(
      within(container).getByRole("button", { name: /clear conversation/i }),
    ).toBeInTheDocument();

    // Should show empty conversation state
    expect(
      within(container).getByRole("status", { name: /empty conversation/i }),
    ).toBeInTheDocument();
  });

  it("sends message and displays conversation", async () => {
    const user = userEvent.setup();
    const { container } = render(<Playground />, {
      wrapper: createWrapper(["/?model=gpt-4o"]),
    });

    // Wait for chat playground to load
    await waitFor(() => {
      expect(
        within(container).getByRole("textbox", { name: /message input/i }),
      ).toBeInTheDocument();
    });

    // Type and send a message
    const messageInput = within(container).getByRole("textbox", {
      name: /message input/i,
    });
    await user.type(messageInput, "Hello!");
    await user.click(
      within(container).getByRole("button", { name: /send message/i }),
    );

    // Should show the sent message
    expect(within(container).getByText("Hello!")).toBeInTheDocument();

    // Should show the AI response
    await waitFor(() => {
      expect(
        within(container).getByText("Hello! How can I help you today?"),
      ).toBeInTheDocument();
    });

    // Should no longer show empty conversation state
    expect(
      within(container).queryByRole("status", { name: /empty conversation/i }),
    ).not.toBeInTheDocument();

    // Input should be cleared after sending
    expect(messageInput).toHaveValue("");
  });

  it("loads embedding playground and compares text similarity", async () => {
    const user = userEvent.setup();
    const { container } = render(<Playground />, {
      wrapper: createWrapper(["/?model=text-embedding-3-small"]),
    });

    // Wait for embedding playground to load
    await waitFor(() => {
      expect(
        within(container).getByRole("heading", {
          name: /embeddings playground/i,
        }),
      ).toBeInTheDocument();
    });

    // Should show embedding interface elements
    expect(
      within(container).getByRole("textbox", { name: /text a input/i }),
    ).toBeInTheDocument();
    expect(
      within(container).getByRole("textbox", { name: /text b input/i }),
    ).toBeInTheDocument();
    expect(
      within(container).getByRole("button", { name: /compare similarity/i }),
    ).toBeInTheDocument();

    // Type in both text areas
    const firstTextInput = within(container).getByRole("textbox", {
      name: /text a input/i,
    });
    const secondTextInput = within(container).getByRole("textbox", {
      name: /text b input/i,
    });

    await user.type(firstTextInput, "The cat sat on the mat");
    await user.type(secondTextInput, "A feline rested on the rug");

    // Click compare similarity button
    await user.click(
      within(container).getByRole("button", { name: /compare similarity/i }),
    );

    // Should show similarity result
    await waitFor(() => {
      expect(
        within(container).getByRole("heading", { name: /similarity results/i }),
      ).toBeInTheDocument();
      expect(
        within(container).getByRole("status", { name: /similarity category/i }),
      ).toHaveTextContent(/very similar/i);
    });
  });
});
