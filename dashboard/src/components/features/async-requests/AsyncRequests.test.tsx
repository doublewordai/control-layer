import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";
import { AsyncRequests } from "./AsyncRequests";
import * as hooks from "../../../api/control-layer/hooks";
import * as authorization from "../../../utils/authorization";
import * as orgContext from "../../../contexts/organization/useOrganizationContext";

// Mock the hooks
vi.mock("../../../api/control-layer/hooks", () => ({
  useConfig: vi.fn(),
  useAsyncRequests: vi.fn(),
  useDeleteAsyncRequest: vi.fn(() => ({
    mutateAsync: vi.fn(),
    isPending: false,
  })),
  useModels: vi.fn(),
  useUsers: vi.fn(),
}));

// Mock authorization hook
vi.mock("../../../utils/authorization", () => ({
  useAuthorization: vi.fn(),
}));

// Mock organization context
vi.mock("../../../contexts/organization/useOrganizationContext", () => ({
  useOrganizationContext: vi.fn(),
}));

// Re-applied in beforeEach. A per-test mockReturnValue survives
// vi.clearAllMocks() (only mockReset would drop it), so without an explicit
// default every override would leak into the tests that follow it.
const DEFAULT_AUTH = {
  userRoles: ["PlatformManager"],
  isLoading: false,
  hasPermission: () => true,
  canAccessRoute: () => true,
  getFirstAccessibleRoute: () => "/batches",
};

const DEFAULT_ORG = {
  activeOrganizationId: null,
  activeOrganization: null,
  isOrgContext: false,
  setActiveOrganization: vi.fn(),
};

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
    <MemoryRouter initialEntries={["/responses"]}>
      <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
    </MemoryRouter>
  );
};

// One individual and one organization: the account filter has to offer both,
// because org-scoped keys bill their traffic to the org rather than to the
// member who holds the key.
const ACCOUNTS = [
  { id: "user-1", email: "dev@example.com", user_type: "individual" },
  { id: "org-1", email: "ops@example.com", user_type: "organization" },
];

describe("AsyncRequests", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    // Filters persist to localStorage, so a value written by one test would
    // otherwise seed the next one.
    localStorage.clear();

    vi.mocked(authorization.useAuthorization).mockReturnValue(
      DEFAULT_AUTH as any,
    );
    vi.mocked(orgContext.useOrganizationContext).mockReturnValue(
      DEFAULT_ORG as any,
    );

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

    vi.mocked(hooks.useUsers).mockReturnValue({
      data: { data: ACCOUNTS },
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

  it("renders Create Response and API buttons", () => {
    const { container } = render(<AsyncRequests />, {
      wrapper: createWrapper(),
    });
    expect(
      within(container).getByRole("button", { name: /create response/i }),
    ).toBeInTheDocument();
    expect(
      within(container).getByRole("button", { name: /api/i }),
    ).toBeInTheDocument();
  });

  it("includes background requests in the default query", () => {
    render(<AsyncRequests />, { wrapper: createWrapper() });

    expect(hooks.useAsyncRequests).toHaveBeenCalledWith(
      expect.objectContaining({
        service_tiers: "flex,priority,background",
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

  describe("Filter persistence", () => {
    it("seeds filters from localStorage on mount", () => {
      localStorage.setItem(
        "filters:responses",
        JSON.stringify({
          status: "completed",
          tier: ["flex"],
          activeFirst: "false",
        }),
      );

      render(<AsyncRequests />, { wrapper: createWrapper() });

      expect(hooks.useAsyncRequests).toHaveBeenCalledWith(
        expect.objectContaining({
          status: "completed",
          service_tiers: "flex",
          active_first: false,
        }),
      );
    });

    it("does not persist date range across sessions even if junk is stored", () => {
      // Pre-seed localStorage with date-range-shaped keys to make sure the
      // page actively ignores them, rather than the test passing only because
      // nothing was stored in the first place.
      localStorage.setItem(
        "filters:responses",
        JSON.stringify({
          created_after: "2025-01-01T00:00:00.000Z",
          created_before: "2025-01-02T00:00:00.000Z",
          dateRange: "2025-01-01_2025-01-02",
        }),
      );

      render(<AsyncRequests />, { wrapper: createWrapper() });

      const lastCall = vi
        .mocked(hooks.useAsyncRequests)
        .mock.calls.at(-1)?.[0];
      expect(lastCall?.created_after).toBeUndefined();
      expect(lastCall?.created_before).toBeUndefined();
    });
  });

  // `member_id` on this endpoint filters `fusillade.requests.created_by` —
  // the account a request billed to, not the person who made it. dwctl
  // rejects the param from anyone below PlatformManager, so the control has
  // to be gated to match or it 400s the very people it renders for.
  describe("Account filter", () => {
    // The filter row lives in the table's headerActions, which DataTable
    // swaps for the empty state when there are no rows — so these tests need
    // a row before any filter is on screen.
    beforeEach(() => {
      vi.mocked(hooks.useAsyncRequests).mockReturnValue({
        data: {
          data: [
            {
              id: "req-1",
              batch_id: null,
              model: "test-model",
              status: "completed",
              created_at: "2026-09-09T12:00:00.000Z",
              completed_at: "2026-09-09T12:00:01.000Z",
              failed_at: null,
              duration_ms: 1000,
              response_status: 200,
              service_tier: "flex",
              prompt_tokens: 1,
              completion_tokens: 1,
              reasoning_tokens: null,
              total_tokens: 2,
              total_cost: 0.01,
              // Deliberately not one of the ACCOUNTS addresses: the row
              // renders its billing email in the User column, and a match
              // would make the popover assertions ambiguous.
              created_by_email: "billed@example.com",
            },
          ],
          total_count: 1,
          skip: 0,
          limit: 10,
        },
        isLoading: false,
      } as any);
    });

    it("offers organizations as well as individuals", async () => {
      const user = userEvent.setup();
      const { container } = render(<AsyncRequests />, {
        wrapper: createWrapper(),
      });

      await user.click(
        within(container).getByRole("combobox", { name: /filter by account/i }),
      );

      // Org-scoped keys bill to the org, so an org is often the only account
      // that has the traffic a platform manager is looking for. Excluding
      // them — as the Batches member filter does — would hide it.
      expect(screen.getByText("ops@example.com")).toBeInTheDocument();
      expect(screen.getByText("dev@example.com")).toBeInTheDocument();
      expect(screen.getByText("Org")).toBeInTheDocument();
    });

    it("sends the selected account as member_id", async () => {
      const user = userEvent.setup();
      const { container } = render(<AsyncRequests />, {
        wrapper: createWrapper(),
      });

      await user.click(
        within(container).getByRole("combobox", { name: /filter by account/i }),
      );
      await user.click(screen.getByText("ops@example.com"));

      const lastCall = vi.mocked(hooks.useAsyncRequests).mock.calls.at(-1)?.[0];
      expect(lastCall?.member_id).toBe("org-1");
    });

    it("hides the filter, and sends no member_id, below PlatformManager", () => {
      vi.mocked(authorization.useAuthorization).mockReturnValue({
        userRoles: ["StandardUser"],
        isLoading: false,
        hasPermission: () => true,
        canAccessRoute: () => true,
        getFirstAccessibleRoute: () => "/batches",
      } as any);

      const { container } = render(<AsyncRequests />, {
        wrapper: createWrapper(),
      });

      expect(
        within(container).queryByRole("combobox", { name: /filter by account/i }),
      ).not.toBeInTheDocument();
      expect(
        vi.mocked(hooks.useAsyncRequests).mock.calls.at(-1)?.[0]?.member_id,
      ).toBeUndefined();
    });

    // dwctl checks member_id BEFORE it checks the active organization, so a
    // stale personal-context selection would silently widen an org-scoped
    // view. The page must not send it while the control is hidden.
    it("drops a persisted account id in an org context", () => {
      localStorage.setItem(
        "filters:responses",
        JSON.stringify({ account: "org-1" }),
      );
      vi.mocked(orgContext.useOrganizationContext).mockReturnValue({
        activeOrganizationId: "org-2",
        activeOrganization: null,
        isOrgContext: true,
        setActiveOrganization: vi.fn(),
      } as any);

      const { container } = render(<AsyncRequests />, {
        wrapper: createWrapper(),
      });

      expect(
        within(container).queryByRole("combobox", { name: /filter by account/i }),
      ).not.toBeInTheDocument();
      expect(
        vi.mocked(hooks.useAsyncRequests).mock.calls.at(-1)?.[0]?.member_id,
      ).toBeUndefined();
    });
  });
});
