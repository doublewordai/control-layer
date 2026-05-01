import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, within, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";
import { AsyncRequests } from "./AsyncRequests";
import * as hooks from "../../../api/control-layer/hooks";
import { useOrganizationContext } from "../../../contexts/organization/useOrganizationContext";
import { useAuthorization } from "../../../utils/authorization";

// Mock the hooks
vi.mock("../../../api/control-layer/hooks", () => ({
  useConfig: vi.fn(),
  useAsyncRequests: vi.fn(),
  useModels: vi.fn(),
  useOrganizationMembers: vi.fn(() => ({
    data: [],
    isLoading: false,
  })),
  useUsers: vi.fn(() => ({
    data: { data: [], total_count: 0 },
    isLoading: false,
  })),
}));

// Mock authorization hook
vi.mock("../../../utils/authorization", () => ({
  useAuthorization: vi.fn(() => ({
    userRoles: ["PlatformManager"],
    isLoading: false,
    hasPermission: () => true,
    canAccessRoute: () => true,
    getFirstAccessibleRoute: () => "/batches",
  })),
}));

// Mock organization context
vi.mock("../../../contexts/organization/useOrganizationContext", () => ({
  useOrganizationContext: vi.fn(() => ({
    activeOrganizationId: null,
    activeOrganization: null,
    isOrgContext: false,
    setActiveOrganization: vi.fn(),
  })),
}));

// Mock bootstrap content
vi.mock("../../../hooks/use-bootstrap-content", () => ({
  useBootstrapContent: vi.fn(() => ({
    content: "",
    isClosed: true,
    close: vi.fn(),
  })),
}));

// Mock modals
vi.mock("../../modals/CreateAsyncModal/CreateAsyncModal", () => ({
  CreateAsyncModal: () => null,
}));

vi.mock("../../modals", () => ({
  ApiExamples: () => null,
}));

vi.mock("sonner", () => ({
  toast: { success: vi.fn(), error: vi.fn() },
}));

const createWrapper = () => {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return ({ children }: { children: React.ReactNode }) => (
    <MemoryRouter initialEntries={["/async"]}>
      <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
    </MemoryRouter>
  );
};

describe("AsyncRequests", () => {
  beforeEach(() => {
    vi.clearAllMocks();

    vi.mocked(hooks.useConfig).mockReturnValue({
      data: {
        batches: {
          allowed_completion_windows: ["24h", "1h"],
          async_requests: { enabled: true, completion_window: "1h" },
        },
      },
      isLoading: false,
    } as any);

    vi.mocked(hooks.useAsyncRequests).mockReturnValue({
      data: { data: [], total_count: 0, skip: 0, limit: 10 },
      isLoading: false,
    } as any);

    vi.mocked(hooks.useModels).mockReturnValue({
      data: { data: [] },
      isLoading: false,
    } as any);
  });

  it("renders the page title", () => {
    const { container } = render(<AsyncRequests />, {
      wrapper: createWrapper(),
    });
    expect(
      within(container).getByRole("heading", { level: 1, name: /responses/i }),
    ).toBeInTheDocument();
  });

  it("renders Create Async and API buttons", () => {
    const { container } = render(<AsyncRequests />, {
      wrapper: createWrapper(),
    });
    expect(
      within(container).getByRole("button", { name: /create async/i }),
    ).toBeInTheDocument();
    expect(
      within(container).getByRole("button", { name: /api/i }),
    ).toBeInTheDocument();
  });

  it("passes service_tiers=flex,priority to the query", () => {
    render(<AsyncRequests />, { wrapper: createWrapper() });

    expect(hooks.useAsyncRequests).toHaveBeenCalledWith(
      expect.objectContaining({
        service_tiers: "flex,priority",
      }),
    );
  });

  it("shows loading state", () => {
    vi.mocked(hooks.useAsyncRequests).mockReturnValue({
      data: undefined,
      isLoading: true,
    } as any);

    const { container } = render(<AsyncRequests />, {
      wrapper: createWrapper(),
    });

    const skeletonElements = container.querySelectorAll(".animate-pulse");
    expect(skeletonElements.length).toBeGreaterThan(0);
  });

  it("shows empty state when no requests", () => {
    const { container } = render(<AsyncRequests />, {
      wrapper: createWrapper(),
    });

    expect(
      within(container).getByText(/no async requests found/i),
    ).toBeInTheDocument();
  });

  describe("Member filter", () => {
    it("does not render the member filter for a standard user in personal context", () => {
      vi.mocked(useOrganizationContext).mockReturnValue({
        activeOrganizationId: null,
        activeOrganization: null,
        isOrgContext: false,
        setActiveOrganization: vi.fn(),
      });
      // Standard user — no manage-models permission, not a PlatformManager.
      vi.mocked(useAuthorization).mockReturnValue({
        userRoles: ["StandardUser"],
        isLoading: false,
        hasPermission: () => false,
        canAccessRoute: () => true,
        getFirstAccessibleRoute: () => "/",
      } as any);

      const { container } = render(<AsyncRequests />, {
        wrapper: createWrapper(),
      });

      expect(
        within(container).queryByRole("combobox", {
          name: /filter by member/i,
        }),
      ).not.toBeInTheDocument();
    });

    it("renders the member filter for an org member, populated from useOrganizationMembers", async () => {
      vi.mocked(useOrganizationContext).mockReturnValue({
        activeOrganizationId: "org-1",
        activeOrganization: { id: "org-1", name: "Acme" } as any,
        isOrgContext: true,
        setActiveOrganization: vi.fn(),
      });
      vi.mocked(hooks.useOrganizationMembers).mockReturnValue({
        data: [
          {
            status: "active",
            user: { id: "user-1", email: "alice@acme.test" },
          },
          {
            status: "active",
            user: { id: "user-2", email: "bob@acme.test" },
          },
        ],
        isLoading: false,
      } as any);

      const user = userEvent.setup();
      const { container } = render(<AsyncRequests />, {
        wrapper: createWrapper(),
      });

      const combobox = within(container).getByRole("combobox", {
        name: /filter by member/i,
      });
      expect(combobox).toBeInTheDocument();
      await user.click(combobox);
      // Popover content renders in a portal, so use screen.
      expect(await screen.findByText("alice@acme.test")).toBeInTheDocument();
      expect(screen.getByText("bob@acme.test")).toBeInTheDocument();
    });

    it("passes member_id to useAsyncRequests when a member is selected", async () => {
      vi.mocked(useOrganizationContext).mockReturnValue({
        activeOrganizationId: "org-1",
        activeOrganization: { id: "org-1", name: "Acme" } as any,
        isOrgContext: true,
        setActiveOrganization: vi.fn(),
      });
      vi.mocked(hooks.useOrganizationMembers).mockReturnValue({
        data: [
          {
            status: "active",
            user: { id: "user-1", email: "alice@acme.test" },
          },
        ],
        isLoading: false,
      } as any);

      const user = userEvent.setup();
      const { container } = render(<AsyncRequests />, {
        wrapper: createWrapper(),
      });

      await user.click(
        within(container).getByRole("combobox", {
          name: /filter by member/i,
        }),
      );
      await user.click(await screen.findByText("alice@acme.test"));

      const lastCall = vi
        .mocked(hooks.useAsyncRequests)
        .mock.calls.at(-1)?.[0];
      expect(lastCall?.member_id).toBe("user-1");
    });
  });
});
