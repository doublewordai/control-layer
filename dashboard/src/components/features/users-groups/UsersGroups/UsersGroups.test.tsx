import { render, within, waitFor, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { setupServer } from "msw/node";
import { http, HttpResponse } from "msw";
import React, { ReactNode } from "react";
import { describe, it, expect, beforeAll, afterEach, afterAll } from "vitest";
import userEvent from "@testing-library/user-event";
import UsersGroups from "./UsersGroups";
import { handlers } from "../../../../api/control-layer/mocks/handlers";
import { SettingsProvider } from "../../../../contexts";

// Setup MSW server
const server = setupServer(...handlers);

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

// Test wrapper with QueryClient, Router, and SettingsProvider
function createWrapper() {
  const queryClient = new QueryClient({
    defaultOptions: {
      queries: {
        retry: false, // Disable retries for tests
        staleTime: 0, // Always refetch
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

describe("UsersGroups Component", () => {
  it("renders without crashing", async () => {
    const { container } = render(<UsersGroups />, { wrapper: createWrapper() });

    // Should show loading state initially
    expect(
      within(container).getByText("Loading users and groups..."),
    ).toBeInTheDocument();

    // Should render the component after loading
    await waitFor(() => {
      expect(within(container).getByText("User Access")).toBeInTheDocument();
    });
  });

  it("renders users data when loaded", async () => {
    const { container } = render(<UsersGroups />, { wrapper: createWrapper() });

    await waitFor(() => {
      // Check that user data from mock is displayed
      expect(within(container).getByText("Sarah Chen")).toBeInTheDocument();
      expect(
        within(container).getByText("sarah.chen@acme.com"),
      ).toBeInTheDocument();
    });
  });

  it("renders error state when API fails", async () => {
    server.use(
      http.get("/admin/api/v1/users", () => {
        return HttpResponse.json(
          { error: "Failed to fetch users" },
          { status: 500 },
        );
      }),
    );

    const { container } = render(<UsersGroups />, { wrapper: createWrapper() });

    await waitFor(() => {
      expect(within(container).getByText(/Error:/)).toBeInTheDocument();
    });
  });

  it("renders empty state when no data exists", async () => {
    server.use(
      http.get("/admin/api/v1/users", () => {
        return HttpResponse.json({
          data: [],
          total_count: 0,
          skip: 0,
          limit: 10,
        });
      }),
      http.get("/admin/api/v1/groups", () => {
        return HttpResponse.json({
          data: [],
          total_count: 0,
          skip: 0,
          limit: 10,
        });
      }),
    );

    const { container } = render(<UsersGroups />, { wrapper: createWrapper() });

    await waitFor(() => {
      // Should still render the component structure
      expect(
        within(container).getByRole("heading", { level: 1 }),
      ).toBeInTheDocument();
      expect(
        within(container).getByRole("tab", { name: "Users" }),
      ).toBeInTheDocument();
    });
  });

  describe("User Management Journey", () => {
    it("searches users without offering account creation", async () => {
      const user = userEvent.setup();
      const { container } = render(<UsersGroups />, {
        wrapper: createWrapper(),
      });

      // Wait for initial load - check for main heading and users data
      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { level: 1 }),
        ).toBeInTheDocument();
        expect(within(container).getByRole("table")).toBeInTheDocument();
        expect(within(container).getByText("Sarah Chen")).toBeInTheDocument();
      });

      // Step 1: Navigate to Users tab (should be default)
      const usersTab = within(container).getByRole("tab", { name: "Users" });
      expect(usersTab).toHaveAttribute("aria-selected", "true");

      // Step 2: Verify all users are visible initially
      expect(within(container).getByText("James Wilson")).toBeInTheDocument();
      expect(within(container).getByText("Alex Rodriguez")).toBeInTheDocument();

      // Step 3: Test search functionality
      const searchInput = within(container).getByRole("textbox", {
        name: /search users/i,
      });
      await user.type(searchInput, "Sarah");
      expect(searchInput).toHaveValue("Sarah");

      // Wait for debounce (300ms) + API call + re-render
      // The search is server-side via debouncedSearch, so we need to wait
      await waitFor(
        () => {
          // Verify other users are filtered out
          expect(
            within(container).queryByText("James Wilson"),
          ).not.toBeInTheDocument();
        },
        { timeout: 3000, interval: 100 },
      );

      // Should still show Sarah Chen after search filters
      await waitFor(() => {
        expect(within(container).getByText("Sarah Chen")).toBeInTheDocument();
      });

      // Step 4: Clear search and verify all users return
      // Re-fetch the search input in case it was re-rendered
      const searchInputAfterSearch = within(container).getByRole("textbox", {
        name: /search users/i,
      });
      await user.clear(searchInputAfterSearch);

      await waitFor(
        () => {
          expect(
            within(container).getByText("James Wilson"),
          ).toBeInTheDocument();
          expect(within(container).getByText("Sarah Chen")).toBeInTheDocument();
        },
        { timeout: 3000, interval: 100 },
      );

      expect(within(container).queryByRole("button", { name: /add user/i })).not.toBeInTheDocument();
    });

    it("offers ZDR support without profile, role, or deletion controls", async () => {
      const user = userEvent.setup();
      const { container } = render(<UsersGroups />, { wrapper: createWrapper() });
      await within(container).findByText("Sarah Chen");
      expect(within(container).queryByTitle("Delete user")).not.toBeInTheDocument();
      await user.click(within(container).getAllByTitle("Edit user")[0]);
      const dialog = await screen.findByRole("dialog", { name: /edit user/i });
      expect(within(dialog).queryByRole("textbox", { name: /display name/i })).not.toBeInTheDocument();
      expect(within(dialog).queryByRole("checkbox", { name: /standard user/i })).not.toBeInTheDocument();
      expect(within(dialog).getByRole("switch", { name: /zero data retention/i })).toBeInTheDocument();
      await user.click(within(dialog).getByRole("button", { name: /cancel/i }));
      await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    });

    it("tests Edit Group Modal through group dropdown", async () => {
      const user = userEvent.setup();
      const { container } = render(<UsersGroups />, {
        wrapper: createWrapper(),
      });

      // Switch to Groups tab
      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { level: 1 }),
        ).toBeInTheDocument();
      });

      const groupsTab = within(container).getByRole("tab", { name: "Groups" });
      await user.click(groupsTab);
      expect(groupsTab).toHaveAttribute("aria-selected", "true");

      // Wait for groups to be visible
      await waitFor(() => {
        expect(within(container).getByText("Engineering")).toBeInTheDocument();
      });

      // Find and click group actions dropdown
      const groupActionMenus = within(container).getAllByRole("button", {
        name: /open menu/i,
      });
      expect(groupActionMenus.length).toBeGreaterThan(0);

      await user.click(groupActionMenus[0]);

      await waitFor(() => {
        expect(screen.getByRole("menu")).toBeInTheDocument();
        expect(
          screen.getByRole("menuitem", { name: "Edit Group" }),
        ).toBeInTheDocument();
      });

      // Click Edit Group
      const editGroupButton = screen.getByRole("menuitem", {
        name: "Edit Group",
      });
      await user.click(editGroupButton);

      await waitFor(() => {
        expect(
          screen.getByRole("dialog", { name: /edit group/i }),
        ).toBeInTheDocument();
        // Verify form fields with injected group data
        expect(screen.getByDisplayValue("Engineering")).toBeInTheDocument();
        expect(screen.getByLabelText(/name/i)).toBeInTheDocument();
        expect(screen.getByLabelText(/description/i)).toBeInTheDocument();
      });

      // Cancel edit group modal
      const cancelButton = screen.getByRole("button", {
        name: /cancel/i,
      });
      await user.click(cancelButton);

      await waitFor(() => {
        expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
      });
    });

    it("tests Delete Group Modal through group dropdown", async () => {
      const user = userEvent.setup();
      const { container } = render(<UsersGroups />, {
        wrapper: createWrapper(),
      });

      // Switch to Groups tab
      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { level: 1 }),
        ).toBeInTheDocument();
      });

      const groupsTab = within(container).getByRole("tab", { name: "Groups" });
      await user.click(groupsTab);

      // Wait for groups and find dropdown
      await waitFor(() => {
        expect(within(container).getByText("Engineering")).toBeInTheDocument();
      });

      const groupActionMenus = within(container).getAllByRole("button", {
        name: /open menu/i,
      });
      await user.click(groupActionMenus[0]);

      await waitFor(() => {
        expect(
          screen.getByRole("menuitem", { name: "Delete Group" }),
        ).toBeInTheDocument();
      });

      // Click Delete Group
      const deleteGroupButton = screen.getByRole("menuitem", {
        name: "Delete Group",
      });
      await user.click(deleteGroupButton);

      await waitFor(() => {
        expect(
          screen.getByRole("dialog", { name: /delete group/i }),
        ).toBeInTheDocument();
        // Verify injected group data is shown in confirmation
        expect(screen.getAllByText("Engineering").length).toBeGreaterThan(0);
        // Verify delete action is available
        expect(
          screen.getByRole("button", { name: /delete group/i }),
        ).toBeInTheDocument();
      });

      // Cancel deletion
      const cancelButton = screen.getByRole("button", {
        name: /cancel/i,
      });
      await user.click(cancelButton);

      await waitFor(() => {
        expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
      });
    });

    it("tests Group Management Modal through group dropdown", async () => {
      const user = userEvent.setup();
      const { container } = render(<UsersGroups />, {
        wrapper: createWrapper(),
      });

      // Switch to Groups tab
      await waitFor(() => {
        expect(
          within(container).getByRole("heading", { level: 1 }),
        ).toBeInTheDocument();
      });

      const groupsTab = within(container).getByRole("tab", { name: "Groups" });
      await user.click(groupsTab);

      // Wait for groups and find dropdown
      await waitFor(() => {
        expect(within(container).getByText("Engineering")).toBeInTheDocument();
      });

      const groupActionMenus = within(container).getAllByRole("button", {
        name: /open menu/i,
      });
      await user.click(groupActionMenus[0]);

      await waitFor(() => {
        expect(
          screen.getByRole("menuitem", { name: "Manage Members" }),
        ).toBeInTheDocument();
      });

      // Click Manage Members
      const manageGroupButton = screen.getByRole("menuitem", {
        name: "Manage Members",
      });
      await user.click(manageGroupButton);

      await waitFor(() => {
        expect(screen.getByRole("dialog")).toBeInTheDocument();
        // Verify this is the GroupManagementModal
        expect(
          screen.getByRole("button", { name: /done/i }),
        ).toBeInTheDocument();
      });

      // Close modal
      const doneButton = screen.getByRole("button", {
        name: /done/i,
      });
      await user.click(doneButton);

      await waitFor(() => {
        expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
      });
    });

    it("retains model-group management in the SSO dashboard", async () => {
      const user = userEvent.setup();
      const { container } = render(<UsersGroups />, { wrapper: createWrapper() });
      await within(container).findByText("Sarah Chen");
      await user.click(within(container).getAllByTitle("Manage groups")[0]);
      const dialog = await screen.findByRole("dialog");
      expect(within(dialog).getByRole("button", { name: /done/i })).toBeInTheDocument();
    });
  });
});
