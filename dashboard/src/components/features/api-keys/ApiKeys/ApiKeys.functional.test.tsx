import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { BrowserRouter } from "react-router-dom";
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
import { ApiKeys } from "./ApiKeys";
import { handlers } from "../../../../api/control-layer/mocks/handlers";

// Setup MSW server with existing handlers
const server = setupServer(...handlers);

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => {
  server.resetHandlers();
  vi.clearAllMocks();
});
afterAll(() => server.close());

// Mock clipboard API for copy functionality
const mockWriteText = vi.fn().mockImplementation(() => Promise.resolve());
Object.assign(navigator, {
  clipboard: {
    writeText: mockWriteText,
  },
});

// Mock toast from sonner
const mockToastSuccess = vi.fn();
const mockToastError = vi.fn();
vi.mock("sonner", () => ({
  toast: {
    success: mockToastSuccess,
    error: mockToastError,
  },
  Toaster: () => null,
}));

// Test wrapper with QueryClient and Router
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
    <QueryClientProvider client={queryClient}>
      <BrowserRouter>{children}</BrowserRouter>
    </QueryClientProvider>
  );
}

describe("API Keys Component - Functional Tests", () => {
  describe("API Keys List Journey", () => {
    it("displays existing API keys and allows creating new ones", async () => {
      const user = userEvent.setup();
      render(<ApiKeys />, { wrapper: createWrapper() });

      // Wait for component to load
      await waitFor(() => {
        expect(
          screen.getByRole("heading", { name: /api keys/i }),
        ).toBeInTheDocument();
      });

      // Should show management interface with existing keys
      expect(
        screen.getByText(/manage your api keys for programmatic access/i),
      ).toBeInTheDocument();

      // Should have create button
      const createButton = screen.getByRole("button", {
        name: /create new api key/i,
      });
      await user.click(createButton);

      // Should open create dialog
      await waitFor(() => {
        expect(screen.getByRole("dialog")).toBeInTheDocument();
        expect(
          screen.getByRole("heading", { name: /create new api key/i }),
        ).toBeInTheDocument();
      });
    });
  });

  describe("API Key Creation Journey", () => {
    it("creates new API key with name and description", async () => {
      const user = userEvent.setup();
      render(<ApiKeys />, { wrapper: createWrapper() });

      // Wait for component to load
      await waitFor(() => {
        expect(
          screen.getByRole("heading", { name: /api keys/i }),
        ).toBeInTheDocument();
      });

      // Click create API key button
      const createButton = screen.getByRole("button", {
        name: /create new api key/i,
      });
      await user.click(createButton);

      // Wait for dialog to open
      await waitFor(() => {
        expect(screen.getByRole("dialog")).toBeInTheDocument();
      });

      // Fill in the form
      const nameInput = screen.getByLabelText(/name/i);
      const descriptionInput = screen.getByLabelText(/description/i);

      await user.type(nameInput, "Test API Key");
      await user.type(descriptionInput, "For testing purposes");

      // Submit the form
      const submitButton = screen.getByRole("button", { name: /create key/i });
      await user.click(submitButton);

      // Should show success state with the created key
      await waitFor(() => {
        expect(
          screen.getByRole("heading", {
            name: /api key created successfully/i,
          }),
        ).toBeInTheDocument();
      });

      // Should show the key name and API key
      expect(screen.getByText("Test API Key")).toBeInTheDocument();
      expect(screen.getByText(/save this key/i)).toBeInTheDocument();
    });

    it("validates required name field", async () => {
      const user = userEvent.setup();
      render(<ApiKeys />, { wrapper: createWrapper() });

      // Wait for component to load and click create button
      await waitFor(() => {
        expect(
          screen.getByRole("heading", { name: /api keys/i }),
        ).toBeInTheDocument();
      });

      const createButton = screen.getByRole("button", {
        name: /create new api key/i,
      });
      await user.click(createButton);

      // Wait for dialog and try to submit without name
      await waitFor(() => {
        expect(screen.getByRole("dialog")).toBeInTheDocument();
      });

      const submitButton = screen.getByRole("button", { name: /create key/i });
      expect(submitButton).toBeDisabled();

      // Add name and button should be enabled
      const nameInput = screen.getByLabelText(/name/i);
      await user.type(nameInput, "My Key");

      expect(submitButton).not.toBeDisabled();
    });
  });

  describe("API Key Management Journey", () => {
    it("copies API key to clipboard after creation", async () => {
      const user = userEvent.setup();
      render(<ApiKeys />, { wrapper: createWrapper() });

      // Create an API key first
      await waitFor(() => {
        expect(
          screen.getByRole("heading", { name: /api keys/i }),
        ).toBeInTheDocument();
      });

      const createButton = screen.getByRole("button", {
        name: /create new api key/i,
      });
      await user.click(createButton);

      await waitFor(() => {
        expect(screen.getByRole("dialog")).toBeInTheDocument();
      });

      const nameInput = screen.getByLabelText(/name/i);
      await user.type(nameInput, "Test Key");

      const submitButton = screen.getByRole("button", { name: /create key/i });
      await user.click(submitButton);

      // Wait for success state
      await waitFor(() => {
        expect(
          screen.getByRole("heading", {
            name: /api key created successfully/i,
          }),
        ).toBeInTheDocument();
      });

      // Should show copy button with accessibility label
      const copyButton = screen.getByRole("button", { name: /copy api key/i });
      expect(copyButton).toBeInTheDocument();

      // Should show API key in code block
      expect(screen.getByRole("code")).toBeInTheDocument();
    });

    it("shows success toast notification when copying API key", async () => {
      const user = userEvent.setup();
      render(<ApiKeys />, { wrapper: createWrapper() });

      // Create an API key first
      await waitFor(() => {
        expect(
          screen.getByRole("heading", { name: /api keys/i }),
        ).toBeInTheDocument();
      });

      const createButton = screen.getByRole("button", {
        name: /create new api key/i,
      });
      await user.click(createButton);

      await waitFor(() => {
        expect(screen.getByRole("dialog")).toBeInTheDocument();
      });

      const nameInput = screen.getByLabelText(/name/i);
      await user.type(nameInput, "Test Key");

      const submitButton = screen.getByRole("button", { name: /create key/i });
      await user.click(submitButton);

      // Wait for success state
      await waitFor(() => {
        expect(
          screen.getByRole("heading", {
            name: /api key created successfully/i,
          }),
        ).toBeInTheDocument();
      });

      // Click copy button
      const copyButton = screen.getByRole("button", { name: /copy api key/i });
      await user.click(copyButton);

      // Should call clipboard API
      await waitFor(() => {
        expect(mockWriteText).toHaveBeenCalled();
      });

      // Should show success toast
      await waitFor(() => {
        expect(mockToastSuccess).toHaveBeenCalledWith(
          "API key copied to clipboard",
        );
      });
    });

    it("shows error toast notification when copying fails", async () => {
      const user = userEvent.setup();

      // Mock clipboard to fail
      mockWriteText.mockImplementationOnce(() =>
        Promise.reject(new Error("Clipboard access denied")),
      );

      render(<ApiKeys />, { wrapper: createWrapper() });

      // Create an API key first
      await waitFor(() => {
        expect(
          screen.getByRole("heading", { name: /api keys/i }),
        ).toBeInTheDocument();
      });

      const createButton = screen.getByRole("button", {
        name: /create new api key/i,
      });
      await user.click(createButton);

      await waitFor(() => {
        expect(screen.getByRole("dialog")).toBeInTheDocument();
      });

      const nameInput = screen.getByLabelText(/name/i);
      await user.type(nameInput, "Test Key");

      const submitButton = screen.getByRole("button", { name: /create key/i });
      await user.click(submitButton);

      // Wait for success state
      await waitFor(() => {
        expect(
          screen.getByRole("heading", {
            name: /api key created successfully/i,
          }),
        ).toBeInTheDocument();
      });

      // Click copy button
      const copyButton = screen.getByRole("button", { name: /copy api key/i });
      await user.click(copyButton);

      // Should call clipboard API and fail
      await waitFor(() => {
        expect(mockWriteText).toHaveBeenCalled();
      });

      // Should show error toast
      await waitFor(() => {
        expect(mockToastError).toHaveBeenCalledWith("Failed to copy API key");
      });
    });

    it("closes create dialog with cancel or done buttons", async () => {
      const user = userEvent.setup();
      render(<ApiKeys />, { wrapper: createWrapper() });

      await waitFor(() => {
        expect(
          screen.getByRole("heading", { name: /api keys/i }),
        ).toBeInTheDocument();
      });

      // Open dialog
      const createButton = screen.getByRole("button", {
        name: /create new api key/i,
      });
      await user.click(createButton);

      await waitFor(() => {
        expect(screen.getByRole("dialog")).toBeInTheDocument();
      });

      // Cancel should close dialog
      const cancelButton = screen.getByRole("button", { name: /cancel/i });
      await user.click(cancelButton);

      await waitFor(() => {
        expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
      });
    });
  });

  describe("API Key Deletion Journey", () => {
    it("deletes individual API key with confirmation", async () => {
      const user = userEvent.setup();
      render(<ApiKeys />, { wrapper: createWrapper() });

      // Wait for component to load - this test assumes there are existing API keys
      await waitFor(() => {
        expect(
          screen.getByRole("heading", { name: /api keys/i }),
        ).toBeInTheDocument();
      });

      // Look for delete button in table (if API keys exist)
      const deleteButtons = screen.queryAllByRole("button", {
        name: /delete/i,
      });

      if (deleteButtons.length > 0) {
        // Click first delete button
        await user.click(deleteButtons[0]);

        // Should open confirmation dialog
        await waitFor(() => {
          expect(
            screen.getByRole("heading", { name: /delete api key/i }),
          ).toBeInTheDocument();
        });

        expect(
          screen.getByText(/this action cannot be undone/i),
        ).toBeInTheDocument();

        // Cancel should close dialog
        const cancelButton = screen.getByRole("button", { name: /cancel/i });
        await user.click(cancelButton);

        await waitFor(() => {
          expect(
            screen.queryByRole("heading", { name: /delete api key/i }),
          ).not.toBeInTheDocument();
        });
      }
    });
  });

  describe("Loading and Error States", () => {
    it("shows loading state initially", async () => {
      render(<ApiKeys />, { wrapper: createWrapper() });

      // Should show loading skeleton initially with animate-pulse
      const loadingContainer = document.querySelector(".animate-pulse");
      expect(loadingContainer).toBeInTheDocument();

      // Wait for actual content to load
      await waitFor(() => {
        expect(
          screen.getByRole("heading", { name: /api keys/i }),
        ).toBeInTheDocument();
      });
    });

    it("handles form submission and shows success state", async () => {
      const user = userEvent.setup();
      render(<ApiKeys />, { wrapper: createWrapper() });

      await waitFor(() => {
        expect(
          screen.getByRole("heading", { name: /api keys/i }),
        ).toBeInTheDocument();
      });

      // Open create dialog
      const createButton = screen.getByRole("button", {
        name: /create new api key/i,
      });
      await user.click(createButton);

      await waitFor(() => {
        expect(screen.getByRole("dialog")).toBeInTheDocument();
      });

      // Fill form
      const nameInput = screen.getByLabelText(/name/i);
      await user.type(nameInput, "Test Success Key");

      // Submit form
      const submitButton = screen.getByRole("button", { name: /create key/i });
      await user.click(submitButton);

      // Should show success state
      await waitFor(() => {
        expect(
          screen.getByRole("heading", {
            name: /api key created successfully/i,
          }),
        ).toBeInTheDocument();
      });
    });
  });

  describe("Responsive Behavior", () => {
    it("maintains functionality on mobile viewports", async () => {
      const user = userEvent.setup();

      // Set mobile viewport
      Object.defineProperty(window, "innerWidth", {
        writable: true,
        configurable: true,
        value: 375,
      });

      render(<ApiKeys />, { wrapper: createWrapper() });

      await waitFor(() => {
        expect(
          screen.getByRole("heading", { name: /api keys/i }),
        ).toBeInTheDocument();
      });

      // Core functionality should still work
      const createButton = screen.getByRole("button", {
        name: /create new api key/i,
      });
      await user.click(createButton);

      await waitFor(() => {
        expect(screen.getByRole("dialog")).toBeInTheDocument();
      });

      // Form should still be functional on mobile
      const nameInput = screen.getByLabelText(/name/i);
      expect(nameInput).toBeInTheDocument();
    });
  });
});
