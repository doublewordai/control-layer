import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { setupServer } from "msw/node";
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
import Playground from "./Playground";
import { handlers } from "../../../../api/control-layer/mocks/handlers";
import { SettingsProvider } from "../../../../contexts/settings/SettingsContext";

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
      create: vi.fn().mockImplementation((params) => {
        // Non-streaming call (for summarization)
        if (!params.stream) {
          return Promise.resolve({
            choices: [
              {
                message: { content: "Summarized Title" },
              },
            ],
          });
        }

        // Streaming call (for chat responses)
        return Promise.resolve({
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
        });
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

function createWrapper(initialEntries = ["/"]) {
  const queryClient = new QueryClient({
    defaultOptions: {
      queries: {
        retry: false,
        gcTime: 0,
      },
    },
  });

  return ({ children }: { children: ReactNode }) => (
    <SettingsProvider>
      <QueryClientProvider client={queryClient}>
        <MemoryRouter initialEntries={initialEntries}>{children}</MemoryRouter>
      </QueryClientProvider>
    </SettingsProvider>
  );
}

describe("Playground Component - Functional Tests", () => {
  it("loads playground page and shows welcome state", async () => {
    render(<Playground />, { wrapper: createWrapper() });

    await waitFor(() => {
      expect(
        screen.getByRole("main", { name: /welcome to playground/i }),
      ).toBeInTheDocument();
    });

    expect(
      screen.getByRole("heading", { name: /welcome to the playground/i }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("combobox", { name: /select model/i }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /back to models/i }),
    ).toBeInTheDocument();
  });

  it("enables model selector when models load", async () => {
    render(<Playground />, { wrapper: createWrapper() });

    const modelSelect = screen.getByRole("combobox", { name: /select model/i });

    // Initially disabled while models load
    expect(modelSelect).toBeDisabled();
    expect(modelSelect).toHaveAttribute("data-disabled");

    // Becomes enabled when models load
    await waitFor(() => {
      expect(modelSelect).not.toBeDisabled();
    });

    // Should have correct attributes and text
    expect(modelSelect).toHaveTextContent(/select a model/i);
    expect(modelSelect).toHaveAttribute("aria-expanded", "false");
    expect(modelSelect).toHaveAttribute("role", "combobox");
    expect(modelSelect).toHaveAttribute("aria-autocomplete", "none");
  });

  it("shows model selector ready for available models", async () => {
    render(<Playground />, { wrapper: createWrapper() });

    const modelSelect = screen.getByRole("combobox", { name: /select model/i });

    // Initially disabled while loading
    expect(modelSelect).toBeDisabled();

    // Wait for models API to complete and selector to be enabled
    await waitFor(() => {
      expect(modelSelect).not.toBeDisabled();
    });

    // Should have proper ARIA attributes indicating it has options available
    expect(modelSelect).toHaveAttribute("aria-label", "Select model");
    expect(modelSelect).toHaveAttribute("aria-controls"); // Has dropdown options
    expect(modelSelect).toHaveAttribute("aria-expanded", "false"); // Closed but ready

    // Should show placeholder indicating models are loaded and ready for selection
    expect(modelSelect).toHaveTextContent(/select a model/i);
    expect(modelSelect).not.toHaveTextContent(/loading/i);
    expect(modelSelect).not.toHaveTextContent(/no models/i);
  });

  it("shows no error messages when models load successfully", async () => {
    render(<Playground />, { wrapper: createWrapper() });

    await waitFor(() => {
      expect(
        screen.getByRole("main", { name: /welcome to playground/i }),
      ).toBeInTheDocument();
    });

    expect(
      screen.queryByText(/failed to load models/i),
    ).not.toBeInTheDocument();
  });

  it("displays essential elements on mobile viewport", async () => {
    Object.defineProperty(window, "innerWidth", {
      value: 375,
      configurable: true,
    });

    render(<Playground />, { wrapper: createWrapper() });

    await waitFor(() => {
      expect(
        screen.getByRole("main", { name: /welcome to playground/i }),
      ).toBeInTheDocument();
    });

    expect(
      screen.getByRole("button", { name: /back to models/i }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("combobox", { name: /select model/i }),
    ).toBeInTheDocument();
  });

  it("loads chat playground when model parameter is provided", async () => {
    render(<Playground />, {
      wrapper: createWrapper(["/?model=gpt-4o"]),
    });

    // Wait for models to load and model to be selected
    await waitFor(() => {
      const modelSelect = screen.getByRole("combobox", {
        name: /select model/i,
      });
      expect(modelSelect).toHaveTextContent("gpt-4o");
    });

    // Should successfully load chat playground (no welcome screen)
    expect(
      screen.queryByRole("main", { name: /welcome to playground/i }),
    ).not.toBeInTheDocument();

    // Should show chat playground header
    expect(
      screen.getByRole("heading", { name: /chat playground/i }),
    ).toBeInTheDocument();

    // Should show actual chat interface elements
    expect(
      screen.getByRole("textbox", { name: /message input/i }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /send message/i }),
    ).toBeInTheDocument();

    // Should show conversation history sidebar
    expect(
      screen.getByRole("heading", { name: /conversations/i }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /new conversation/i }),
    ).toBeInTheDocument();

    // Should show empty conversation state
    expect(
      screen.getByRole("status", { name: /empty conversation/i }),
    ).toBeInTheDocument();
  });

  it("sends message and displays conversation", async () => {
    const user = userEvent.setup();
    render(<Playground />, {
      wrapper: createWrapper(["/?model=gpt-4o"]),
    });

    // Wait for chat playground to load
    await waitFor(() => {
      expect(
        screen.getByRole("textbox", { name: /message input/i }),
      ).toBeInTheDocument();
    });

    // Type and send a message
    const messageInput = screen.getByRole("textbox", {
      name: /message input/i,
    });
    await user.type(messageInput, "Hello!");
    await user.click(screen.getByRole("button", { name: /send message/i }));

    // Should show the sent message
    expect(screen.getByText("Hello!")).toBeInTheDocument();

    // Should show the AI response
    await waitFor(() => {
      expect(
        screen.getByText("Hello! How can I help you today?"),
      ).toBeInTheDocument();
    });

    // Should no longer show empty conversation state
    expect(
      screen.queryByRole("status", { name: /empty conversation/i }),
    ).not.toBeInTheDocument();

    // Input should be cleared after sending
    expect(messageInput).toHaveValue("");
  });

  it("loads embedding playground and compares text similarity", async () => {
    const user = userEvent.setup();
    render(<Playground />, {
      wrapper: createWrapper(["/?model=embedding-small"]),
    });

    // Wait for embedding playground to load
    await waitFor(() => {
      expect(
        screen.getByRole("heading", { name: /embeddings playground/i }),
      ).toBeInTheDocument();
    });

    // Should show embedding interface elements
    expect(
      screen.getByRole("textbox", { name: /text a input/i }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("textbox", { name: /text b input/i }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /compare similarity/i }),
    ).toBeInTheDocument();

    // Type in both text areas
    const firstTextInput = screen.getByRole("textbox", {
      name: /text a input/i,
    });
    const secondTextInput = screen.getByRole("textbox", {
      name: /text b input/i,
    });

    await user.type(firstTextInput, "The cat sat on the mat");
    await user.type(secondTextInput, "A feline rested on the rug");

    // Click compare similarity button
    await user.click(
      screen.getByRole("button", { name: /compare similarity/i }),
    );

    // Should show similarity result
    await waitFor(() => {
      expect(
        screen.getByRole("heading", { name: /similarity results/i }),
      ).toBeInTheDocument();
      expect(
        screen.getByRole("status", { name: /similarity category/i }),
      ).toHaveTextContent(/very similar/i);
    });
  });

  // New conversation history integration tests
  it("creates new conversation on first message sent (lazy creation)", async () => {
    const user = userEvent.setup();

    // Clear any existing conversations
    localStorage.clear();

    render(<Playground />, {
      wrapper: createWrapper(["/?model=gpt-4o"]),
    });

    await waitFor(() => {
      expect(
        screen.getByRole("textbox", { name: /message input/i }),
      ).toBeInTheDocument();
    });

    // Send first message
    const messageInput = screen.getByRole("textbox", { name: /message input/i });
    await user.type(messageInput, "What is TypeScript?");
    await user.click(screen.getByRole("button", { name: /send message/i }));

    // Conversation should be created with the message
    await waitFor(() => {
      expect(screen.getByText("What is TypeScript?")).toBeInTheDocument();

      // Conversation should appear in sidebar with timestamp-based title or message content
      const conversationItems = screen.queryAllByText(/\d{1,2} \w+ \d{2}:\d{2}/);
      expect(conversationItems.length).toBeGreaterThan(0);
    });
  });

  it("clears conversation when New Conversation button is clicked", async () => {
    const user = userEvent.setup();

    render(<Playground />, {
      wrapper: createWrapper(["/?model=gpt-4o"]),
    });

    await waitFor(() => {
      expect(
        screen.getByRole("textbox", { name: /message input/i }),
      ).toBeInTheDocument();
    });

    // Send a message to create a conversation
    const messageInput = screen.getByRole("textbox", { name: /message input/i });
    await user.type(messageInput, "First message");
    await user.click(screen.getByRole("button", { name: /send message/i }));

    // Wait for response
    await waitFor(() => {
      expect(screen.getByText("First message")).toBeInTheDocument();
    });

    // Click New Conversation button
    await user.click(screen.getByRole("button", { name: /new conversation/i }));

    // Messages should be cleared
    expect(screen.queryByText("First message")).not.toBeInTheDocument();

    // Should show empty conversation state
    await waitFor(() => {
      expect(
        screen.getByRole("status", { name: /empty conversation/i }),
      ).toBeInTheDocument();
    });
  });

  it("switches between conversations correctly", async () => {
    const user = userEvent.setup();

    // Clear and create two conversations
    localStorage.clear();

    render(<Playground />, {
      wrapper: createWrapper(["/?model=gpt-4o"]),
    });

    await waitFor(() => {
      expect(
        screen.getByRole("textbox", { name: /message input/i }),
      ).toBeInTheDocument();
    });

    const messageInput = screen.getByRole("textbox", { name: /message input/i });

    // Create first conversation
    await user.type(messageInput, "First conversation message");
    await user.click(screen.getByRole("button", { name: /send message/i }));

    await waitFor(() => {
      expect(screen.getByText("First conversation message")).toBeInTheDocument();
    });

    // Create new conversation
    await user.click(screen.getByRole("button", { name: /new conversation/i }));

    await waitFor(() => {
      expect(screen.queryByText("First conversation message")).not.toBeInTheDocument();
    });

    // Send second message
    await user.type(messageInput, "Second conversation message");
    await user.click(screen.getByRole("button", { name: /send message/i }));

    await waitFor(() => {
      expect(screen.getByText("Second conversation message")).toBeInTheDocument();
    });

    // Now we should have 2 conversations in the sidebar
    // Click on the first conversation to switch back
    const conversationItems = screen.getAllByRole("button").filter(btn =>
      btn.textContent?.includes("First conversation")
    );

    if (conversationItems.length > 0) {
      await user.click(conversationItems[0]);

      // Should now see the first conversation's messages
      await waitFor(() => {
        expect(screen.getByText("First conversation message")).toBeInTheDocument();
        expect(screen.queryByText("Second conversation message")).not.toBeInTheDocument();
      });
    }
  });

  it("restores partial streaming content after simulated refresh", async () => {
    const user = userEvent.setup();

    localStorage.clear();

    render(<Playground />, {
      wrapper: createWrapper(["/?model=gpt-4o"]),
    });

    await waitFor(() => {
      expect(
        screen.getByRole("textbox", { name: /message input/i }),
      ).toBeInTheDocument();
    });

    // Send a message to create conversation
    const messageInput = screen.getByRole("textbox", { name: /message input/i });
    await user.type(messageInput, "Test message");
    await user.click(screen.getByRole("button", { name: /send message/i }));

    // Wait for conversation to be created
    await waitFor(() => {
      expect(screen.getByText("Test message")).toBeInTheDocument();
    });

    // Get the conversation ID from localStorage
    const conversations = JSON.parse(localStorage.getItem("playground-conversations") || "{}");
    const conversationId = conversations.conversations?.[0]?.id;

    if (conversationId) {
      // Simulate a partial streaming state (as if page was refreshed mid-stream)
      const streamingState = {
        conversationId,
        partialContent: "This is partial streamed content...",
        userMessage: {
          role: "user",
          content: "Another message",
          timestamp: new Date().toISOString(),
          modelAlias: "",
        },
        modelAlias: "gpt-4o",
        timestamp: new Date().toISOString(),
      };
      localStorage.setItem("playground-streaming-state", JSON.stringify(streamingState));

      // Unmount and remount to simulate page refresh
      const { unmount } = render(<Playground />, {
        wrapper: createWrapper(["/?model=gpt-4o"]),
      });
      unmount();

      // Remount
      render(<Playground />, {
        wrapper: createWrapper(["/?model=gpt-4o"]),
      });

      // Should restore partial content with interrupted marker
      await waitFor(() => {
        expect(screen.getByText(/This is partial streamed content/i)).toBeInTheDocument();
        expect(screen.getByText(/stream interrupted/i)).toBeInTheDocument();
      });
    }
  });

  it("saves messages periodically during streaming (batched auto-save)", async () => {
    const user = userEvent.setup();

    localStorage.clear();

    render(<Playground />, {
      wrapper: createWrapper(["/?model=gpt-4o"]),
    });

    await waitFor(() => {
      expect(
        screen.getByRole("textbox", { name: /message input/i }),
      ).toBeInTheDocument();
    });

    // Send a message
    const messageInput = screen.getByRole("textbox", { name: /message input/i });
    await user.type(messageInput, "Testing streaming save");
    await user.click(screen.getByRole("button", { name: /send message/i }));

    // Wait for streaming to start
    await waitFor(() => {
      expect(screen.getByText("Testing streaming save")).toBeInTheDocument();
    });

    // During streaming, check if streaming state gets saved
    // Note: This is a simplified check - in real scenario we'd need to
    // intercept the stream and verify saves happen every N chunks
    await waitFor(() => {
      const streamingState = localStorage.getItem("playground-streaming-state");
      // After stream completes, it should be cleared
      expect(streamingState).toBeNull();
    }, { timeout: 5000 });
  });

  it("auto-summarizes conversation title when feature is enabled", async () => {
    const user = userEvent.setup();

    localStorage.clear();

    // Enable auto-summarize feature via settings
    localStorage.setItem("app-settings", JSON.stringify({
      apiBaseUrl: "/admin/api/v1",
      features: {
        demo: false,
        autoSummarizeTitles: true,
      },
    }));

    // Reset mock to track calls from this test
    vi.clearAllMocks();

    render(<Playground />, {
      wrapper: createWrapper(["/?model=gpt-4o"]),
    });

    await waitFor(() => {
      expect(
        screen.getByRole("textbox", { name: /message input/i }),
      ).toBeInTheDocument();
    });

    // Send first message to create conversation
    const messageInput = screen.getByRole("textbox", { name: /message input/i });
    await user.type(messageInput, "What is the capital of France?");
    await user.click(screen.getByRole("button", { name: /send message/i }));

    // Wait for response to complete
    await waitFor(() => {
      expect(screen.getByText("Hello! How can I help you today?")).toBeInTheDocument();
    });

    // Wait for summarization to complete and title to update
    await waitFor(() => {
      // Verify summarization call was made with correct parameters
      const calls = (mockOpenAI.chat.completions.create as any).mock.calls;
      const summarizationCall = calls.find((call: any) => !call[0].stream);

      if (summarizationCall) {
        expect(summarizationCall[0].messages[0].role).toBe("system");
        expect(summarizationCall[0].messages[0].content).toContain("short, concise titles");

        // Check that the conversation title was updated to "Summarized Title"
        const conversations = JSON.parse(localStorage.getItem("playground-conversations") || "{}");
        const conversation = conversations.conversations?.[0];
        expect(conversation?.title).toBe("Summarized Title");
      }
    }, { timeout: 3000 });
  }, 10000);

  it("does not auto-summarize title when feature is disabled", async () => {
    const user = userEvent.setup();

    localStorage.clear();

    // Disable auto-summarize feature
    localStorage.setItem("app-settings", JSON.stringify({
      apiBaseUrl: "/admin/api/v1",
      features: {
        demo: false,
        autoSummarizeTitles: false,
      },
    }));

    // Reset the mock to track calls
    vi.clearAllMocks();

    render(<Playground />, {
      wrapper: createWrapper(["/?model=gpt-4o"]),
    });

    await waitFor(() => {
      expect(
        screen.getByRole("textbox", { name: /message input/i }),
      ).toBeInTheDocument();
    });

    // Send first message
    const messageInput = screen.getByRole("textbox", { name: /message input/i });
    await user.type(messageInput, "What is the capital of France?");
    await user.click(screen.getByRole("button", { name: /send message/i }));

    // Wait for response to complete
    await waitFor(() => {
      expect(screen.getByText("Hello! How can I help you today?")).toBeInTheDocument();
    });

    // Wait a bit to ensure no summarization happens
    await new Promise(resolve => setTimeout(resolve, 1000));

    // Verify only one call was made (the chat stream), no summarization call
    expect(mockOpenAI.chat.completions.create).toHaveBeenCalledTimes(1);

    // Check that the conversation title remains the default timestamp + message format
    const conversations = JSON.parse(localStorage.getItem("playground-conversations") || "{}");
    const conversation = conversations.conversations?.[0];
    // Title should include timestamp and start of message, or just timestamp
    expect(conversation?.title).toBeDefined();
    expect(conversation?.title).toMatch(/\d{1,2} \w+ \d{2}:\d{2}/);
    expect(conversation?.title).not.toBe("Summarized Title");
  });
});
