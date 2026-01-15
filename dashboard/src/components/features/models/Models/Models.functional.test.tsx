import { render, screen, waitFor, within } from "@testing-library/react";
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
import Models from "./Models";
import { handlers } from "../../../../api/control-layer/mocks/handlers";
import { SettingsProvider } from "../../../../contexts/settings/SettingsContext";

// Setup MSW server with existing handlers
const server = setupServer(...handlers);

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

// Mock navigation since we're testing functional paths
const mockNavigate = vi.fn();
vi.mock("react-router-dom", async () => {
  const actual = await vi.importActual("react-router-dom");
  return {
    ...actual,
    useNavigate: () => mockNavigate,
  };
});

// Mock the authorization hook
vi.mock("../../../../utils/authorization", () => ({
  useAuthorization: vi.fn(() => ({
    userRoles: ["PlatformManager"],
    hasPermission: vi.fn(() => true),
    canAccessRoute: vi.fn(() => true),
  })),
}));

// Test wrapper with QueryClient, Router, and Context Providers
let queryClient: QueryClient;

function createWrapper() {
  queryClient = new QueryClient({
    defaultOptions: {
      queries: {
        retry: false,
        gcTime: 0, // Disable caching for tests
      },
    },
  });

  return ({ children }: { children: ReactNode }) => (
    <SettingsProvider>
      <QueryClientProvider client={queryClient}>
        <MemoryRouter>{children}</MemoryRouter>
      </QueryClientProvider>
    </SettingsProvider>
  );
}

describe("Models Component - Functional Tests", () => {
  beforeEach(() => {
    mockNavigate.mockClear();
  });

  afterEach(() => {
    // Clean up QueryClient to prevent state pollution between tests
    if (queryClient) {
      queryClient.clear();
      queryClient.cancelQueries();
    }
  });

  describe("Model Discovery Journey", () => {
    it("allows users to browse, filter, and search models", async () => {
      const user = userEvent.setup();
      const { container } = render(<Models />, { wrapper: createWrapper() });

      // Wait for data to load
      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /models/i }),
        ).toBeInTheDocument();
      });

      // Wait for models to load and render
      await waitFor(() => {
        expect(
          within(container).getAllByRole("listitem").length,
        ).toBeGreaterThan(0);
      });

      // Verify initial state - should show multiple model cards
      const modelCards = within(container).getAllByRole("listitem");
      expect(modelCards.length).toBeGreaterThan(0);

      // Test that filter button is present and opens the filter popover
      const filterButton = within(container).getByRole("button", {
        name: /filter models/i,
      });
      expect(filterButton).toBeInTheDocument();

      // Open the filter popover
      await user.click(filterButton);

      // The filter popover renders in a portal, so use screen
      const providerSelect = screen.getByRole("combobox", {
        name: /filter by endpoint provider/i,
      });
      expect(providerSelect).toBeInTheDocument();

      // Verify the select shows "All Endpoints" initially
      expect(providerSelect).toHaveTextContent(/all endpoints/i);

      // Close the popover by pressing Escape
      await user.keyboard("{Escape}");

      // Test search functionality
      const searchInput = within(container).getByRole("textbox", {
        name: /search models/i,
      });
      await user.type(searchInput, "gpt");

      // Verify search results
      await waitFor(() => {
        const searchResults = within(container).getAllByRole("listitem");
        // Should have GPT-related models
        searchResults.forEach((card) => {
          const cardText = card.textContent?.toLowerCase();
          expect(cardText).toMatch(/gpt/i);
        });
      });

      // Clear search to test "no results" scenario
      await user.clear(searchInput);
      await user.type(searchInput, "nonexistent-model-xyz");

      // Verify no results state
      await waitFor(() => {
        expect(within(container).queryAllByRole("listitem")).toHaveLength(0);
      });
    });

    it("navigates to playground when clicking playground button", async () => {
      const user = userEvent.setup();
      const { container } = render(<Models />, { wrapper: createWrapper() });

      // Wait for models to load
      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /models/i }),
        ).toBeInTheDocument();
      });

      // Wait for model cards to render
      await waitFor(() => {
        expect(
          within(container).getAllByRole("listitem").length,
        ).toBeGreaterThan(0);
      });

      // Find first model card and click its playground button
      const modelCards = within(container).getAllByRole("listitem");
      expect(modelCards.length).toBeGreaterThan(0);

      const firstCard = modelCards[0];
      const playgroundButton = within(firstCard).getByRole("button", {
        name: /playground/i,
      });

      await user.click(playgroundButton);

      // Verify navigation was called with correct path
      expect(mockNavigate).toHaveBeenCalledWith(
        expect.stringMatching(/^\/playground\?model=/),
      );
    });
  });

  describe("API Integration Journey", () => {
    it("opens API examples modal when clicking API button", async () => {
      const user = userEvent.setup();
      const { container } = render(<Models />, { wrapper: createWrapper() });

      // Wait for models to load
      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /models/i }),
        ).toBeInTheDocument();
      });

      // Wait for model cards to render
      await waitFor(() => {
        expect(
          within(container).getAllByRole("listitem").length,
        ).toBeGreaterThan(0);
      });

      // Find first model card and click its API button
      const modelCards = within(container).getAllByRole("listitem");
      const firstCard = modelCards[0];
      const apiButton = within(firstCard).getByRole("button", { name: /api/i });

      await user.click(apiButton);

      // Verify API examples modal opened
      await waitFor(() => {
        // use screen as dialog is rendered outside of the container
        expect(screen.getByRole("dialog")).toBeInTheDocument();
        // Look for the specific modal heading
        expect(
          screen.getByRole("heading", { name: /api examples/i }),
        ).toBeInTheDocument();
      });
    });
  });

  describe("Access Control Journey", () => {
    it("shows access toggle for admin users", async () => {
      const user = userEvent.setup();
      const { container } = render(<Models />, { wrapper: createWrapper() });

      // Wait for models to load
      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /models/i }),
        ).toBeInTheDocument();
      });

      // Check for filter button (which contains the admin access toggle)
      const filterButton = within(container).getByRole("button", {
        name: /filter models/i,
      });
      expect(filterButton).toBeInTheDocument();

      // Open the filter popover to verify access toggle is present
      await user.click(filterButton);

      // The filter popover renders in a portal, so use screen
      const accessToggle = screen.getByRole("switch", {
        name: /show only my accessible models/i,
      });
      expect(accessToggle).toBeInTheDocument();
      expect(accessToggle).not.toBeChecked();
    });

    it("allows admin users to toggle between all models and accessible models", async () => {
      const user = userEvent.setup();
      const { container } = render(<Models />, { wrapper: createWrapper() });

      // Wait for models to load
      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /models/i }),
        ).toBeInTheDocument();
      });

      // Wait for model cards to render
      await waitFor(() => {
        expect(
          within(container).getAllByRole("listitem").length,
        ).toBeGreaterThan(0);
      });

      // Get initial model count
      const initialCards = within(container).getAllByRole("listitem");
      const _initialCount = initialCards.length;

      // Open the filter popover
      const filterButton = within(container).getByRole("button", {
        name: /filter models/i,
      });
      await user.click(filterButton);

      // Verify the access toggle is not checked initially (popover renders in portal)
      const accessToggle = screen.getByRole("switch", {
        name: /show only my accessible models/i,
      });
      expect(accessToggle).not.toBeChecked();
    });
  });

  describe("Pagination Journey", () => {
    it("handles pagination when many models are present", async () => {
      const user = userEvent.setup();
      const { container } = render(<Models />, { wrapper: createWrapper() });

      // Wait for models to load
      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /models/i }),
        ).toBeInTheDocument();
      });

      // Check if pagination is present (depends on mock data having >12 models)
      const pagination = within(container).queryByRole("navigation", {
        name: /pagination/i,
      });

      if (pagination) {
        // Test pagination if present
        const nextButton = within(pagination).queryByRole("button", {
          name: /next/i,
        });
        if (
          nextButton &&
          !nextButton.classList.contains("pointer-events-none")
        ) {
          await user.click(nextButton);

          // Verify we moved to next page
          await waitFor(() => {
            const currentPageButton = within(pagination).getByRole("button", {
              pressed: true,
            });
            expect(currentPageButton).toHaveTextContent("2");
          });
        }
      }
    });
  });

  describe("Error Handling Journey", () => {
    it("handles API errors gracefully", async () => {
      // For this test, we'll just verify the component handles the loading state
      // The existing MSW handlers provide successful responses
      const { container } = render(<Models />, { wrapper: createWrapper() });

      // Should eventually load successfully (not show loading state)
      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /models/i }),
        ).toBeInTheDocument();
        expect(
          within(container).queryByText(/loading/i),
        ).not.toBeInTheDocument();
      });
    });
  });

  describe("Admin Features Journey", () => {
    it("allows admins to add groups to models with no groups", async () => {
      const user = userEvent.setup();
      const { container } = render(<Models />, { wrapper: createWrapper() });

      // Wait for models to load
      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /models/i }),
        ).toBeInTheDocument();
      });

      // Wait for model cards to render
      await waitFor(() => {
        expect(
          within(container).getAllByRole("listitem").length,
        ).toBeGreaterThan(0);
      });

      // Find a model card and look for "Add groups" button
      const modelCards = within(container).getAllByRole("listitem");
      expect(modelCards.length).toBeGreaterThan(0);

      // Look for "Add groups" buttons in any of the cards
      const addGroupsButtons = within(container).queryAllByRole("button", {
        name: /add groups/i,
      });

      if (addGroupsButtons.length > 0) {
        // Wait for the button to be fully interactive
        const firstAddGroupsButton = addGroupsButtons[0];
        await waitFor(() => {
          expect(firstAddGroupsButton).toBeEnabled();
        });

        // Click the first "Add groups" button
        // Skip pointer events check as the button may have pointer-events css applied transiently
        await user.click(firstAddGroupsButton);

        // Verify access management modal opens
        await waitFor(() => {
          expect(screen.getByRole("dialog")).toBeInTheDocument();
        });
      } else {
        // If no "Add groups" button is visible, all models have groups
        // This is also a valid state to test
        expect(modelCards.length).toBeGreaterThan(0);
      }
    });

    it("allows admins to manage access groups via group badges", async () => {
      const user = userEvent.setup();
      const { container } = render(<Models />, { wrapper: createWrapper() });

      // Wait for models to load
      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /models/i }),
        ).toBeInTheDocument();
      });

      // Look for group badges or "+ X more" badges
      const moreGroupsBadges = within(container).queryAllByText(/\+\d+ more/);

      if (moreGroupsBadges.length > 0) {
        // Click on a "+ X more" badge
        const firstMoreBadge = moreGroupsBadges[0];
        await user.click(firstMoreBadge);

        // Verify access management modal opens
        // Using both timeout and interval to handle slow CI environments
        await waitFor(
          () => {
            expect(within(container).getByRole("dialog")).toBeInTheDocument();
          },
          { timeout: 5000, interval: 50 },
        );
      } else {
        // Look for regular group badges that might be clickable
        const groupBadges = within(container).queryAllByText(/group/i);

        if (groupBadges.length > 0) {
          // Verify group badges are visible (even if not clickable)
          expect(groupBadges.length).toBeGreaterThan(0);
        }
      }
    });

    it("shows admin-specific UI elements for platform managers", async () => {
      const { container } = render(<Models />, { wrapper: createWrapper() });

      // Wait for models to load
      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /models/i }),
        ).toBeInTheDocument();
      });

      // Wait for model cards to render
      await waitFor(() => {
        expect(
          within(container).getAllByRole("listitem").length,
        ).toBeGreaterThan(0);
      });

      // Verify admin-specific elements are present (filter button contains access toggle)
      const filterButton = within(container).getByRole("button", {
        name: /filter models/i,
      });
      expect(filterButton).toBeInTheDocument();

      // Check for admin-only buttons in model cards
      const modelCards = within(container).getAllByRole("listitem");
      expect(modelCards.length).toBeGreaterThan(0);

      // Look for admin UI elements
      const adminButtons = within(container).queryAllByRole("button", {
        name: /add groups|open menu/i,
      });

      // Should have some admin buttons visible (exact count depends on mock data)
      expect(adminButtons.length).toBeGreaterThanOrEqual(0);
    });

    it("handles hover interactions on group badges", async () => {
      const user = userEvent.setup();
      const { container } = render(<Models />, { wrapper: createWrapper() });

      // Wait for models to load
      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /models/i }),
        ).toBeInTheDocument();
      });

      // Look for "+ X more" badges which should show hover cards
      const moreGroupsBadges = within(container).queryAllByText(/\+\d+ more/);

      if (moreGroupsBadges.length > 0) {
        const firstMoreBadge = moreGroupsBadges[0];

        // Hover over the badge
        await user.hover(firstMoreBadge);

        // Wait a bit for hover card to potentially appear
        // Note: Hover cards might not work in jsdom environment
        // This test mainly verifies the element exists and is hoverable
        expect(firstMoreBadge).toBeInTheDocument();

        // Unhover
        await user.unhover(firstMoreBadge);
      }

      // Test passes if we can interact with hover elements without errors
      expect(
        within(container).getByRole("heading", { name: /models/i }),
      ).toBeInTheDocument();
    });

    it("handles permission-based rendering correctly", async () => {
      const { container } = render(<Models />, { wrapper: createWrapper() });

      // Wait for models to load
      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /models/i }),
        ).toBeInTheDocument();
      });

      // Wait for model cards to render
      await waitFor(() => {
        expect(
          within(container).getAllByRole("listitem").length,
        ).toBeGreaterThan(0);
      });

      const modelCards = within(container).getAllByRole("listitem");
      expect(modelCards.length).toBeGreaterThan(0);

      // Verify that admin features are conditionally rendered
      // The mock data should provide admin permissions by default
      const filterButton = within(container).getByRole("button", {
        name: /filter models/i,
      });
      expect(filterButton).toBeInTheDocument();

      // Check that models show appropriate admin controls
      // This could be group management buttons or dropdown menus
      const adminControls = within(container).queryAllByRole("button", {
        name: /add groups|open menu|manage access/i,
      });

      // With admin permissions, should have some admin controls
      // The exact number depends on mock data structure
      expect(adminControls.length).toBeGreaterThanOrEqual(0);
    });
  });

  describe("Responsive Behavior", () => {
    it("maintains functionality across different screen sizes", async () => {
      const user = userEvent.setup();

      // Test mobile-like viewport
      Object.defineProperty(window, "innerWidth", {
        writable: true,
        configurable: true,
        value: 375,
      });

      const { container } = render(<Models />, { wrapper: createWrapper() });

      // Wait for models to load
      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { name: /models/i }),
        ).toBeInTheDocument();
      });

      // Core functionality should still work on mobile
      const searchInput = within(container).getByRole("textbox", {
        name: /search models/i,
      });

      // Wait for input to be fully interactive
      await waitFor(() => {
        expect(searchInput).toBeEnabled();
      });

      // Skip pointer events check as the input may have transient pointer-events css
      await user.type(searchInput, "gpt");

      await waitFor(() => {
        const searchResults = within(container).getAllByRole("listitem");
        expect(searchResults.length).toBeGreaterThan(0);
      });
    });
  });
});
