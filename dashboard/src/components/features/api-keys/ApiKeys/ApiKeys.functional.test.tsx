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
    activeOrganization: null as {
      id: string;
      name: string;
      role: string;
      zero_data_retention: boolean;
      can_manage_keys: boolean;
    } | null,
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

      // Should show the key name and API key (scoped to the dialog: the
      // created key is now also persisted into the mock list behind it)
      const dialog = screen.getByRole("dialog");
      expect(within(dialog).getByText("Test API Key")).toBeInTheDocument();
      expect(within(dialog).getByText(/save this key/i)).toBeInTheDocument();
    });

    it("submits realtime purpose when a platform manager selects Inference in org context", async () => {
      const user = userEvent.setup();
      const orgId = "org-test-123";
      let capturedUserId: string | undefined;
      let capturedBody: Record<string, unknown> | undefined;

      mockOrgContext.value = {
        activeOrganizationId: orgId,
        activeOrganization: {
          id: orgId,
          name: "Test Org",
          role: "owner",
          zero_data_retention: false,
          can_manage_keys: true,
        },
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

  describe("Rotation", () => {
    it("rotate dialog warns about in-flight batches and shows the new secret on confirm", async () => {
      const user = userEvent.setup();
      let rotatedKeyId: string | undefined;
      server.use(
        http.post(
          "/admin/api/v1/users/:userId/api-keys/:keyId/rotate",
          ({ params }) => {
            rotatedKeyId = params.keyId as string;
            return HttpResponse.json({ key: "sk-rotated-new-secret" });
          },
        ),
      );

      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      await user.click(
        await within(container).findByRole("button", {
          name: /rotate ci\/cd pipeline/i,
        }),
      );

      // Confirm dialog carries the in-flight-batch warning.
      await waitFor(() => {
        expect(
          screen.getByRole("heading", { name: /rotate api key/i }),
        ).toBeInTheDocument();
      });
      expect(
        screen.getByText(/batches already submitted with this key/i),
      ).toBeInTheDocument();
      expect(
        screen.getByText(/cancel any in-flight batches\s+separately/i),
      ).toBeInTheDocument();

      await user.click(screen.getByRole("button", { name: /^rotate key$/i }));

      // One-time secret display, same pattern as creation.
      await waitFor(() => {
        expect(
          screen.getByRole("heading", { name: /api key rotated/i }),
        ).toBeInTheDocument();
      });
      expect(rotatedKeyId).toBe("key-1");
      expect(
        screen.getByText(/save this key now - it won't be shown again/i),
      ).toBeInTheDocument();
      expect(screen.getByText("sk-rotated-new-secret")).toBeInTheDocument();
      expect(
        screen.getByRole("button", { name: /copy api key/i }),
      ).toBeInTheDocument();
    });

    it("never shows a secret in the list — no masked column or reveal/copy affordances", async () => {
      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      const keyName = await within(container).findByText("CI/CD Pipeline");
      const keyRow = keyName.closest("tr");
      expect(keyRow).not.toBeNull();

      expect(
        within(container).queryByRole("columnheader", { name: /secret/i }),
      ).not.toBeInTheDocument();
      expect(within(keyRow!).queryByText("sk-••••••••")).not.toBeInTheDocument();
      expect(
        within(keyRow!).queryByRole("button", {
          name: /reveal secret for ci\/cd pipeline/i,
        }),
      ).not.toBeInTheDocument();
      expect(
        within(keyRow!).queryByRole("button", {
          name: /copy secret for ci\/cd pipeline/i,
        }),
      ).not.toBeInTheDocument();
    });
  });

  describe("Org Key Management", () => {
    const orgId = "org-550e8400-0001";
    // Sarah Chen — usersData[0], the demo "current" user and org owner.
    const managerId = "550e8400-e29b-41d4-a716-446655440001";
    // James Wilson — usersData[1], plain member of the org.
    const memberId = "550e8400-e29b-41d4-a716-446655440002";

    // can_manage_keys mirrors the server's effective flag: owners/admins are
    // always true; plain members only when granted the additive role.
    function enterOrgContext(role: string, canManageKeys = role !== "member") {
      mockOrgContext.value = {
        activeOrganizationId: orgId,
        activeOrganization: {
          id: orgId,
          name: "Acme Corporation",
          role,
          zero_data_retention: false,
          can_manage_keys: canManageKeys,
        },
        isOrgContext: true,
        setActiveOrganization: async () => {},
      };
    }

    it("shows scoping controls to a PlatformManager who is only a plain org member", async () => {
      // The default mock current user carries the PlatformManager role. A PM
      // whose org membership is 'member' still receives an UNscoped key list
      // server-side (ReadAll bypasses created_by scoping), so the UI must
      // offer the same tabs/filter as managers — otherwise they get
      // everyone's keys with no way to narrow the view.
      enterOrgContext("member", false);

      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      expect(
        await within(container).findByRole("tab", { name: /all keys/i }),
      ).toBeInTheDocument();
      expect(
        within(container).getByRole("tab", { name: /my keys/i }),
      ).toBeInTheDocument();
      expect(
        within(container).getByRole("combobox", { name: /filter by member/i }),
      ).toBeInTheDocument();
    });

    it("shows scope tabs, member filter, and assignee column to an org manager", async () => {
      enterOrgContext("owner");

      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      // Manager default is "All keys".
      const allKeysTab = await within(container).findByRole("tab", {
        name: /all keys/i,
      });
      expect(allKeysTab).toHaveAttribute("aria-selected", "true");
      expect(
        within(container).getByRole("tab", { name: /my keys/i }),
      ).toBeInTheDocument();

      // Member filter dropdown, built from the org members list.
      expect(
        within(container).getByRole("combobox", { name: /filter by member/i }),
      ).toBeInTheDocument();

      // Assignee column resolves created_by via the members list.
      expect(
        within(container).getByRole("columnheader", { name: /assignee/i }),
      ).toBeInTheDocument();
      await waitFor(() => {
        expect(
          within(container).getAllByText("Sarah Chen").length,
        ).toBeGreaterThanOrEqual(1);
      });
    });

    it("filters the list down to a single member's keys", async () => {
      const user = userEvent.setup();
      enterOrgContext("owner");

      server.use(
        http.get("/admin/api/v1/users/:userId/api-keys", () => {
          return HttpResponse.json({
            data: [
              {
                id: "mgr-key",
                name: "Manager Key",
                created_at: "2026-01-01T00:00:00Z",
                created_by: managerId,
              },
              {
                id: "mem-key",
                name: "Member Key",
                created_at: "2026-01-02T00:00:00Z",
                created_by: memberId,
              },
            ],
            total_count: 2,
            skip: 0,
            limit: 10,
          });
        }),
      );

      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      // Managers see everyone's keys by default.
      await within(container).findByText("Manager Key");
      expect(within(container).getByText("Member Key")).toBeInTheDocument();

      // Filter by James → only his key remains.
      await user.click(
        within(container).getByRole("combobox", { name: /filter by member/i }),
      );
      await user.click(screen.getByRole("option", { name: "James Wilson" }));
      await waitFor(() => {
        expect(
          within(container).queryByText("Manager Key"),
        ).not.toBeInTheDocument();
      });
      expect(within(container).getByText("Member Key")).toBeInTheDocument();

      // "My keys" tab filters on created_by === me, overriding the member
      // filter.
      await user.click(
        within(container).getByRole("tab", { name: /my keys/i }),
      );
      await waitFor(() => {
        expect(within(container).getByText("Manager Key")).toBeInTheDocument();
      });
      expect(
        within(container).queryByText("Member Key"),
      ).not.toBeInTheDocument();
    });

    it("lets a manager issue a key to another member", async () => {
      const user = userEvent.setup();
      enterOrgContext("admin");

      let capturedBody: Record<string, unknown> | undefined;
      server.use(
        http.post(
          "/admin/api/v1/users/:userId/api-keys",
          async ({ request }) => {
            capturedBody = (await request.json()) as Record<string, unknown>;
            return HttpResponse.json(
              {
                id: "issued-key",
                name: capturedBody.name,
                created_at: new Date().toISOString(),
                created_by: memberId,
                key: "sk-issued-to-member",
              },
              { status: 201 },
            );
          },
        ),
      );

      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      await user.click(
        await within(container).findByRole("button", {
          name: /create new api key/i,
        }),
      );
      await waitFor(() => {
        expect(screen.getByRole("dialog")).toBeInTheDocument();
      });

      await user.type(screen.getByLabelText(/name/i), "James's Key");

      // Assign to another member; helper text explains where the key lands.
      const assignSelect = screen.getByRole("combobox", {
        name: /assign to member/i,
      });
      expect(
        screen.getByText(/rotate it from their api keys page/i),
      ).toBeInTheDocument();
      await user.click(assignSelect);
      // The assign dropdown shows EMAILS — admins know their members' email
      // addresses, not their generated usernames or display names.
      await user.click(
        screen.getByRole("option", { name: "james.wilson@acme.com" }),
      );

      await user.click(screen.getByRole("button", { name: /create key/i }));

      await waitFor(() => {
        expect(capturedBody).toMatchObject({
          name: "James's Key",
          member_id: memberId,
        });
      });
    });

    it("member without the key grant: banner, no create, rotate-only rows", async () => {
      const user = userEvent.setup();
      enterOrgContext("member", false);

      server.use(
        // Current user is James, a plain StandardUser member.
        http.get("/admin/api/v1/users/:id", ({ params }) => {
          if (params.id === "current") {
            return HttpResponse.json({
              id: memberId,
              username: "github|87234156",
              email: "james.wilson@acme.com",
              roles: ["StandardUser"],
            });
          }
          return HttpResponse.json({ error: "not found" }, { status: 404 });
        }),
        // The member holds one org key (issued by an admin).
        http.get("/admin/api/v1/users/:userId/api-keys", () => {
          return HttpResponse.json({
            data: [
              {
                id: "held-key",
                name: "Issued Key",
                created_at: "2026-01-01T00:00:00Z",
                created_by: memberId,
                spend_limit: "10",
                spend_limit_interval: "monthly",
                spend: "1",
                resets_at: "2026-08-01T00:00:00Z",
              },
            ],
            total_count: 1,
            skip: 0,
            limit: 10,
          });
        }),
        http.post(
          "/admin/api/v1/users/:userId/api-keys/:keyId/rotate",
          () => {
            return HttpResponse.json({ key: "sk-member-rotated-secret" });
          },
        ),
      );

      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      await within(container).findByText("Issued Key");

      // Info banner (rotation is the secret-recovery path), and no create
      // affordance anywhere.
      await waitFor(() => {
        expect(
          within(container).getByText(
            /api keys in this organization are issued by its admins\. you can rotate a key you hold to get a fresh secret/i,
          ),
        ).toBeInTheDocument();
      });
      expect(
        within(container).queryByRole("button", { name: /create new api key/i }),
      ).not.toBeInTheDocument();
      expect(
        within(container).queryByRole("button", { name: /create first api key/i }),
      ).not.toBeInTheDocument();

      // No edit/delete or bulk selection — but rotate stays available on a
      // key the member holds, since it's their route to a fresh secret.
      expect(
        within(container).queryByRole("button", {
          name: /edit usage limit for issued key/i,
        }),
      ).not.toBeInTheDocument();
      expect(
        within(container).queryByRole("button", { name: /delete issued key/i }),
      ).not.toBeInTheDocument();
      expect(within(container).queryByRole("checkbox")).not.toBeInTheDocument();
      // No scoping tabs for non-managers.
      expect(within(container).queryByRole("tab")).not.toBeInTheDocument();

      // Rotate flow: confirm dialog with the in-flight-batch warning, then
      // the one-time display of the new secret.
      await user.click(
        within(container).getByRole("button", { name: /rotate issued key/i }),
      );
      await waitFor(() => {
        expect(
          screen.getByRole("heading", { name: /rotate api key/i }),
        ).toBeInTheDocument();
      });
      expect(
        screen.getByText(/batches already submitted with this key/i),
      ).toBeInTheDocument();

      await user.click(screen.getByRole("button", { name: /^rotate key$/i }));

      await waitFor(() => {
        expect(
          screen.getByRole("heading", { name: /api key rotated/i }),
        ).toBeInTheDocument();
      });
      expect(
        screen.getByText("sk-member-rotated-secret"),
      ).toBeInTheDocument();
    });

    it("org managers keep full management of all org keys", async () => {
      enterOrgContext("owner");

      server.use(
        http.get("/admin/api/v1/users/:userId/api-keys", () => {
          return HttpResponse.json({
            data: [
              {
                id: "mem-key",
                name: "Member Key",
                created_at: "2026-01-02T00:00:00Z",
                created_by: memberId,
              },
            ],
            total_count: 1,
            skip: 0,
            limit: 10,
          });
        }),
      );

      const { container } = render(<ApiKeys />, { wrapper: createWrapper() });

      await within(container).findByText("Member Key");

      // Managers keep create + full row actions, even on a key held by
      // another member.
      expect(
        within(container).getByRole("button", { name: /create new api key/i }),
      ).toBeInTheDocument();
      expect(
        within(container).queryByText(/issued by its admins/i),
      ).not.toBeInTheDocument();
      expect(
        within(container).getByRole("button", {
          name: /edit usage limit for member key/i,
        }),
      ).toBeInTheDocument();
      expect(
        within(container).getByRole("button", { name: /rotate member key/i }),
      ).toBeInTheDocument();
      expect(
        within(container).getByRole("button", { name: /delete member key/i }),
      ).toBeInTheDocument();
    });
  });
});
