import { render, waitFor, within, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { setupServer } from "msw/node";
import { http, HttpResponse } from "msw";
import type { ReactNode } from "react";
import {
  describe,
  it,
  expect,
  beforeAll,
  afterEach,
  afterAll,
  vi,
  type Mock,
} from "vitest";

const mockOrgContext = vi.hoisted(() => ({
  value: {
    activeOrganizationId: null as string | null,
    activeOrganization: null,
    isOrgContext: false,
    setActiveOrganization: async () => {},
  },
}));

const mockStorage = vi.hoisted(() => {
  const store = new Map<string, string>();
  const storage = {
    getItem: (key: string) => store.get(key) ?? null,
    setItem: (key: string, value: string) => {
      store.set(key, value);
    },
    removeItem: (key: string) => {
      store.delete(key);
    },
    clear: () => {
      store.clear();
    },
  };

  Object.defineProperty(globalThis, "localStorage", {
    value: storage,
    configurable: true,
    writable: true,
  });

  return storage;
});

// Mock sonner module - use factory function to avoid hoisting issues
vi.mock("sonner", () => {
  return {
    toast: {
      success: vi.fn(),
      error: vi.fn(),
    },
    Toaster: () => null,
  };
});

// Mock organization context - defaults to personal (non-org) context and can be overridden per test
vi.mock("@/contexts", () => ({
  useOrganizationContext: () => mockOrgContext.value,
}));

import { ApiKeys } from "./ApiKeys";
import { handlers } from "../../../../api/control-layer/mocks/handlers";
import { toast } from "sonner";

// Setup MSW server with existing handlers
const server = setupServer(...handlers);

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => {
  server.resetHandlers();
  vi.clearAllMocks();
  mockStorage.clear();
  mockOrgContext.value = {
    activeOrganizationId: null,
    activeOrganization: null,
    isOrgContext: false,
    setActiveOrganization: async () => {},
  };
});
afterAll(() => server.close());

// Mock clipboard API for copy functionality
const mockWriteText = vi.fn().mockResolvedValue(undefined);
Object.assign(navigator, {
  clipboard: {
    writeText: mockWriteText,
  },
});

// Test wrapper with QueryClient and Router
let queryClient: QueryClient;

function createWrapper() {
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
      <MemoryRouter>{children}</MemoryRouter>
    </QueryClientProvider>
  );
}

describe("API Keys Component - Functional Tests", () => {
  afterEach(() => {
    // Clean up QueryClient to prevent state pollution between tests
    if (queryClient) {
      queryClient.clear();
      queryClient.cancelQueries();
    }
  });
  describe("API Keys List Journey", () => {
    it("displays existing API keys and allows creating new ones", async () => {
      const user = userEvent.setup();
      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      // Wait for component to load
      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /api keys/i }),
        ).toBeInTheDocument();
      });

      // Should show management interface with existing keys
      expect(
        within(container).getByText(
          /manage your api keys for programmatic access/i,
        ),
      ).toBeInTheDocument();

      // Should have create button
      const createButton = within(container).getByRole("button", {
        name: /create new api key/i,
      });
      await user.click(createButton);

      // Should open create dialog (renders in portal)
      await waitFor(() => {
        expect(screen.getByRole("dialog")).toBeInTheDocument();
        expect(
          screen.getByRole("heading", {
            name: /create api key/i,
          }),
        ).toBeInTheDocument();
      });
    });
  });

  describe("API Key Creation Journey", () => {
    it("creates new API key with name and description", async () => {
      const user = userEvent.setup();
      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      // Wait for component to load
      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /api keys/i }),
        ).toBeInTheDocument();
      });

      // Click create API key button
      const createButton = within(container).getByRole("button", {
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
      const submitButton = screen.getByRole("button", {
        name: /create key/i,
      });
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

    it("submits realtime purpose when a platform manager selects Inference in org context", async () => {
      const user = userEvent.setup();
      const orgId = "org-test-123";
      let capturedUserId: string | undefined;
      let capturedBody: Record<string, unknown> | undefined;

      mockOrgContext.value = {
        activeOrganizationId: orgId,
        activeOrganization: null,
        isOrgContext: true,
        setActiveOrganization: async () => {},
      };

      server.use(
        http.post("/admin/api/v1/users/:userId/api-keys", async ({ params, request }) => {
          capturedUserId = params.userId as string;
          capturedBody = (await request.json()) as Record<string, unknown>;

          return HttpResponse.json(
            {
              id: "created-key-id",
              name: capturedBody.name,
              description: capturedBody.description,
              purpose: capturedBody.purpose,
              created_at: new Date().toISOString(),
              key: "sk-test-created-key",
            },
            { status: 201 },
          );
        }),
      );

      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /api keys/i }),
        ).toBeInTheDocument();
      });

      await user.click(
        within(container).getByRole("button", { name: /create new api key/i }),
      );

      await waitFor(() => {
        expect(screen.getByRole("dialog")).toBeInTheDocument();
      });

      await user.type(screen.getByLabelText(/name/i), "Org Inference Key");

      // Key type is chosen via the card radio group (visible to everyone).
      await user.click(screen.getByRole("radio", { name: /platform/i }));
      await user.click(screen.getByRole("radio", { name: /inference/i }));

      await user.click(screen.getByRole("button", { name: /create key/i }));

      await waitFor(() => {
        expect(capturedUserId).toBe(orgId);
        expect(capturedBody).toMatchObject({
          name: "Org Inference Key",
          purpose: "realtime",
        });
      });
    });

    it("validates required name field", async () => {
      const user = userEvent.setup();
      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      // Wait for component to load and click create button
      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /api keys/i }),
        ).toBeInTheDocument();
      });

      const createButton = within(container).getByRole("button", {
        name: /create new api key/i,
      });
      await user.click(createButton);

      // Wait for dialog and try to submit without name
      await waitFor(() => {
        expect(screen.getByRole("dialog")).toBeInTheDocument();
      });

      const submitButton = screen.getByRole("button", {
        name: /create key/i,
      });
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
      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      // Create an API key first
      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /api keys/i }),
        ).toBeInTheDocument();
      });

      const createButton = within(container).getByRole("button", {
        name: /create new api key/i,
      });
      await user.click(createButton);

      await waitFor(() => {
        expect(screen.getByRole("dialog")).toBeInTheDocument();
      });

      const nameInput = screen.getByLabelText(/name/i);
      await user.type(nameInput, "Test Key");

      const submitButton = screen.getByRole("button", {
        name: /create key/i,
      });
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
      const copyButton = screen.getByRole("button", {
        name: /copy api key/i,
      });
      expect(copyButton).toBeInTheDocument();

      // Should show API key in code block
      expect(screen.getByRole("code")).toBeInTheDocument();
    });

    it("shows success toast notification when copying API key", async () => {
      const user = userEvent.setup();

      // Setup fresh clipboard mock for this test
      const testMockWrite = vi.fn().mockResolvedValue(undefined);
      Object.defineProperty(navigator, "clipboard", {
        value: { writeText: testMockWrite },
        writable: true,
        configurable: true,
      });

      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      // Create an API key first
      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /api keys/i }),
        ).toBeInTheDocument();
      });

      const createButton = within(container).getByRole("button", {
        name: /create new api key/i,
      });
      await user.click(createButton);

      await waitFor(() => {
        expect(screen.getByRole("dialog")).toBeInTheDocument();
      });

      const nameInput = screen.getByLabelText(/name/i);
      await user.type(nameInput, "Test Key");

      const submitButton = screen.getByRole("button", {
        name: /create key/i,
      });
      await user.click(submitButton);

      // Wait for success state
      await waitFor(() => {
        expect(
          screen.getByRole("heading", {
            name: /api key created successfully/i,
          }),
        ).toBeInTheDocument();
      });

      // Find and click the copy button
      const copyButton = await screen.findByRole("button", {
        name: /copy api key/i,
      });

      expect(copyButton).toBeInTheDocument();
      await user.click(copyButton);

      // Should call clipboard API and show success toast
      await waitFor(() => {
        expect(testMockWrite).toHaveBeenCalled();
        expect(toast.success as unknown as Mock).toHaveBeenCalledWith(
          "API key copied to clipboard",
        );
      });
    });

    it("shows error toast notification when copying fails", async () => {
      const user = userEvent.setup();

      // Setup fresh clipboard mock that rejects
      const testMockWrite = vi
        .fn()
        .mockRejectedValue(new Error("Clipboard access denied"));
      Object.defineProperty(navigator, "clipboard", {
        value: { writeText: testMockWrite },
        writable: true,
        configurable: true,
      });

      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      // Create an API key first
      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /api keys/i }),
        ).toBeInTheDocument();
      });

      const createButton = within(container).getByRole("button", {
        name: /create new api key/i,
      });
      await user.click(createButton);

      await waitFor(() => {
        expect(screen.getByRole("dialog")).toBeInTheDocument();
      });

      const nameInput = screen.getByLabelText(/name/i);
      await user.type(nameInput, "Test Key");

      const submitButton = screen.getByRole("button", {
        name: /create key/i,
      });
      await user.click(submitButton);

      // Wait for success state
      await waitFor(() => {
        expect(
          screen.getByRole("heading", {
            name: /api key created successfully/i,
          }),
        ).toBeInTheDocument();
      });

      // Find the copy button
      const copyButton = await screen.findByRole("button", {
        name: /copy api key/i,
      });

      expect(copyButton).toBeInTheDocument();
      await user.click(copyButton);

      // Should call clipboard API, fail, and show error toast
      await waitFor(() => {
        expect(testMockWrite).toHaveBeenCalled();
        expect(toast.error as unknown as Mock).toHaveBeenCalledWith(
          "Failed to copy API key",
        );
      });
    });

    it("closes create dialog with cancel or done buttons", async () => {
      const user = userEvent.setup();
      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /api keys/i }),
        ).toBeInTheDocument();
      });

      // Open dialog
      const createButton = within(container).getByRole("button", {
        name: /create new api key/i,
      });
      await user.click(createButton);

      await waitFor(() => {
        expect(screen.getByRole("dialog")).toBeInTheDocument();
      });

      // Cancel should close dialog
      const cancelButton = screen.getByRole("button", {
        name: /cancel/i,
      });
      await user.click(cancelButton);

      await waitFor(() => {
        expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
      });
    });
  });

  describe("API Key Deletion Journey", () => {
    it("deletes individual API key with confirmation", async () => {
      const user = userEvent.setup();
      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      // Wait for component to load - this test assumes there are existing API keys
      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /api keys/i }),
        ).toBeInTheDocument();
      });

      // Look for delete button in table (if API keys exist)
      const deleteButtons = within(container).queryAllByRole("button", {
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
        const cancelButton = screen.getByRole("button", {
          name: /cancel/i,
        });
        await user.click(cancelButton);

        await waitFor(() => {
          expect(
            screen.queryByRole("heading", {
              name: /delete api key/i,
            }),
          ).not.toBeInTheDocument();
        });
      }
    });

    it("removes the deleted API key from the table without a manual refresh", async () => {
      const user = userEvent.setup();
      let apiKeys = [
        {
          id: "key-1",
          name: "CI/CD Pipeline",
          description: "Automated testing and evaluation pipeline",
          created_at: "2025-04-01T10:00:00Z",
        },
        {
          id: "key-2",
          name: "Batch Processing - Production",
          description: "Production batch job submissions",
          created_at: "2025-05-15T09:15:00Z",
        },
      ];

      server.use(
        http.get("/admin/api/v1/users/:userId/api-keys", ({ request }) => {
          const url = new URL(request.url);
          const skip = parseInt(url.searchParams.get("skip") || "0", 10);
          const limit = parseInt(url.searchParams.get("limit") || "10", 10);

          return HttpResponse.json({
            data: apiKeys.slice(skip, skip + limit),
            total_count: apiKeys.length,
            skip,
            limit,
          });
        }),
        http.delete("/admin/api/v1/users/:userId/api-keys/:keyId", ({ params }) => {
          apiKeys = apiKeys.filter((apiKey) => apiKey.id !== params.keyId);
          return HttpResponse.json(null, { status: 204 });
        }),
      );

      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /api keys/i }),
        ).toBeInTheDocument();
      });

      const keyName = screen.getByText("CI/CD Pipeline");
      expect(keyName).toBeInTheDocument();

      const keyRow = keyName.closest("tr");
      expect(keyRow).not.toBeNull();

      // Rows now carry both Edit and Delete actions; target Delete explicitly.
      await user.click(
        within(keyRow!).getByRole("button", { name: /delete/i }),
      );

      await waitFor(() => {
        expect(
          screen.getByRole("heading", { name: /delete api key/i }),
        ).toBeInTheDocument();
      });

      await user.click(
        screen.getByRole("button", {
          name: /delete api key/i,
        }),
      );

      await waitFor(() => {
        expect(screen.queryByText("CI/CD Pipeline")).not.toBeInTheDocument();
      });
    });
  });

  describe("Loading and Error States", () => {
    it("shows loading state initially", async () => {
      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      // Should show loading skeleton initially with animate-pulse
      const loadingContainer = document.querySelector(".animate-pulse");
      expect(loadingContainer).toBeInTheDocument();

      // Wait for actual content to load
      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /api keys/i }),
        ).toBeInTheDocument();
      });
    });

    it("handles form submission and shows success state", async () => {
      const user = userEvent.setup();
      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /api keys/i }),
        ).toBeInTheDocument();
      });

      // Open create dialog
      const createButton = within(container).getByRole("button", {
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
      const submitButton = screen.getByRole("button", {
        name: /create key/i,
      });
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

      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /api keys/i }),
        ).toBeInTheDocument();
      });

      // Core functionality should still work
      const createButton = within(container).getByRole("button", {
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

  describe("Usage Limits", () => {
    it("shows usage against the limit for capped keys and 'No limit' otherwise", async () => {
      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      // key-1 in the mock data is capped at $50 monthly with $12.34 spent.
      await waitFor(() => {
        expect(
          within(container).getByText(/\$12\.34 \/ \$50\.00/),
        ).toBeInTheDocument();
      });
      // Period + calendar-aligned reset instant as subtext, plus the bar.
      expect(
        within(container).getByText(
          /monthly limit · resets aug 1, 2026, 00:00 utc/i,
        ),
      ).toBeInTheDocument();
      const bar = within(container).getByRole("progressbar");
      expect(bar).toHaveAttribute("aria-valuenow", "25"); // 12.34 / 50 ≈ 25%
      // Uncapped keys show the italic placeholder.
      expect(
        within(container).getAllByText("No limit").length,
      ).toBeGreaterThanOrEqual(1);
    });

    it("hides the edit affordance for keys the user cannot manage", async () => {
      // Non-PM user; one foreign-created key, one own key.
      server.use(
        http.get("/admin/api/v1/users/:id", ({ params }) => {
          if (params.id === "current") {
            return HttpResponse.json({
              id: "user-nonpm",
              username: "standard",
              email: "standard@example.com",
              roles: ["StandardUser"],
            });
          }
          return HttpResponse.json({ error: "not found" }, { status: 404 });
        }),
        http.get("/admin/api/v1/users/:userId/api-keys", () => {
          return HttpResponse.json({
            data: [
              {
                id: "own-key",
                name: "My Key",
                purpose: "realtime",
                created_at: "2026-01-01T00:00:00Z",
                created_by: "user-nonpm",
              },
              {
                id: "foreign-key",
                name: "Colleague Key",
                purpose: "realtime",
                created_at: "2026-01-01T00:00:00Z",
                created_by: "someone-else",
              },
            ],
            total_count: 2,
            skip: 0,
            limit: 10,
          });
        }),
      );

      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      await waitFor(() => {
        expect(within(container).getByText("My Key")).toBeInTheDocument();
      });
      expect(
        within(container).getByRole("button", {
          name: /edit usage limit for my key/i,
        }),
      ).toBeInTheDocument();
      expect(
        within(container).queryByRole("button", {
          name: /edit usage limit for colleague key/i,
        }),
      ).not.toBeInTheDocument();
    });

    it("creates a key with a usage limit", async () => {
      const user = userEvent.setup();
      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      await user.click(
        await within(container).findByRole("button", {
          name: /create new api key/i,
        }),
      );
      await waitFor(() => {
        expect(screen.getByRole("dialog")).toBeInTheDocument();
      });

      await user.type(screen.getByLabelText(/name/i), "Budgeted Agent");
      await user.type(screen.getByLabelText(/usage limit amount/i), "25");
      await user.click(
        screen.getByRole("combobox", { name: /usage limit reset period/i }),
      );
      await user.click(screen.getByRole("option", { name: /daily/i }));

      // The helper text now carries the calendar-aligned reset preview.
      expect(screen.getByText(/next resets .*, 00:00 utc\./i)).toBeInTheDocument();

      await user.click(screen.getByRole("button", { name: /create key/i }));

      // Success state shows the one-time key.
      await waitFor(() => {
        expect(
          screen.getByText(/save this key - it won't be shown again/i),
        ).toBeInTheDocument();
      });
    });

    it("edits a limit through the edit dialog and PATCHes tri-state fields", async () => {
      const user = userEvent.setup();
      let patchBody: Record<string, unknown> | null = null;
      server.use(
        http.patch(
          "/admin/api/v1/users/:userId/api-keys/:keyId",
          async ({ request }) => {
            patchBody = (await request.json()) as Record<string, unknown>;
            return HttpResponse.json({
              id: "key-1",
              name: "CI/CD Pipeline",
              created_at: "2025-04-01T10:00:00Z",
              spend_limit: "75",
              spend_limit_interval: "weekly",
              spend: "12.34",
              total_spend: "148.20",
              resets_at: "2026-07-27T00:00:00Z",
            });
          },
        ),
      );

      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      // Open the edit dialog for the capped key.
      await user.click(
        await within(container).findByRole("button", {
          name: /edit usage limit for ci\/cd pipeline/i,
        }),
      );
      await waitFor(() => {
        expect(
          screen.getByRole("heading", { name: /edit usage limit/i }),
        ).toBeInTheDocument();
      });

      // Current usage summary is shown for capped keys, with the full UTC instant.
      expect(
        screen.getByText(
          /spent \$12\.34 of \$50\.00 this window · resets aug 1, 2026, 00:00 utc/i,
        ),
      ).toBeInTheDocument();

      // Fields are prefilled from the key's current limit.
      const amount = screen.getByLabelText(/usage limit amount/i);
      expect(amount).toHaveValue(50);

      // Change the amount and period, then save.
      await user.clear(amount);
      await user.type(amount, "75");
      await user.click(
        screen.getByRole("combobox", { name: /usage limit reset period/i }),
      );
      await user.click(screen.getByRole("option", { name: /weekly/i }));
      await user.click(screen.getByRole("button", { name: /save changes/i }));

      await waitFor(() => {
        expect(patchBody).not.toBeNull();
      });
      expect(patchBody).toMatchObject({
        spend_limit: "75",
        spend_limit_interval: "weekly",
      });
    });

    it("removes a limit by clearing the amount (tri-state null PATCH)", async () => {
      const user = userEvent.setup();
      let patchBody: Record<string, unknown> | null = null;
      server.use(
        http.patch(
          "/admin/api/v1/users/:userId/api-keys/:keyId",
          async ({ request }) => {
            patchBody = (await request.json()) as Record<string, unknown>;
            return HttpResponse.json({
              id: "key-1",
              name: "CI/CD Pipeline",
              created_at: "2025-04-01T10:00:00Z",
              spend_limit: null,
              spend_limit_interval: null,
              spend: null,
              total_spend: null,
              resets_at: null,
            });
          },
        ),
      );

      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      await user.click(
        await within(container).findByRole("button", {
          name: /edit usage limit for ci\/cd pipeline/i,
        }),
      );
      await waitFor(() => {
        expect(
          screen.getByRole("heading", { name: /edit usage limit/i }),
        ).toBeInTheDocument();
      });

      await user.clear(screen.getByLabelText(/usage limit amount/i));
      await user.click(screen.getByRole("button", { name: /save changes/i }));

      await waitFor(() => {
        expect(patchBody).toEqual({
          spend_limit: null,
          spend_limit_interval: null,
        });
      });
    });

    it("resets the spend window from the edit dialog", async () => {
      const user = userEvent.setup();
      let patchBody: Record<string, unknown> | null = null;
      server.use(
        http.patch(
          "/admin/api/v1/users/:userId/api-keys/:keyId",
          async ({ request }) => {
            patchBody = (await request.json()) as Record<string, unknown>;
            return HttpResponse.json({
              id: "key-1",
              name: "CI/CD Pipeline",
              created_at: "2025-04-01T10:00:00Z",
              spend_limit: "50",
              spend_limit_interval: "monthly",
              spend: "0",
              total_spend: "148.20",
              resets_at: "2026-08-01T00:00:00Z",
            });
          },
        ),
      );

      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      await user.click(
        await within(container).findByRole("button", {
          name: /edit usage limit for ci\/cd pipeline/i,
        }),
      );
      await waitFor(() => {
        expect(
          screen.getByRole("heading", { name: /edit usage limit/i }),
        ).toBeInTheDocument();
      });

      await user.click(
        screen.getByRole("button", { name: /reset spend window now/i }),
      );

      await waitFor(() => {
        expect(patchBody).toEqual({ reset_window: true });
      });
    });
  });
});
