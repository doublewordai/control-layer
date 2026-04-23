import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, screen, within } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";
import { AsyncRequests } from "./AsyncRequests";
import * as hooks from "../../../api/control-layer/hooks";
import { useAuthorization } from "../../../utils/authorization";
import { useOrganizationContext } from "../../../contexts/organization/useOrganizationContext";

// Mock the hooks
vi.mock("../../../api/control-layer/hooks", () => ({
  useConfig: vi.fn(),
  useAsyncRequests: vi.fn(),
  useModels: vi.fn(),
  useUsers: vi.fn(),
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

    vi.mocked(useAuthorization).mockReturnValue({
      userRoles: ["PlatformManager"],
      isLoading: false,
      hasPermission: () => true,
      canAccessRoute: () => true,
      getFirstAccessibleRoute: () => "/batches",
    } as any);

    vi.mocked(useOrganizationContext).mockReturnValue({
      activeOrganizationId: null,
      activeOrganization: null,
      isOrgContext: false,
      setActiveOrganization: vi.fn(),
    } as any);

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
      data: { data: [] },
      isLoading: false,
    } as any);
  });

  it("renders the page title", () => {
    const { container } = render(<AsyncRequests />, {
      wrapper: createWrapper(),
    });
    expect(
      within(container).getByRole("heading", { level: 1, name: /async/i }),
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

  it("passes service_tier=flex to the query", () => {
    render(<AsyncRequests />, { wrapper: createWrapper() });

    expect(hooks.useAsyncRequests).toHaveBeenCalledWith(
      expect.objectContaining({
        service_tier: "flex",
      }),
    );
  });

  it("shows the member filter for platform managers", () => {
    vi.mocked(hooks.useUsers).mockReturnValue({
      data: {
        data: [
          { id: "user-1", email: "pm-user@example.com", user_type: "native" },
        ],
      },
      isLoading: false,
    } as any);

    render(<AsyncRequests />, { wrapper: createWrapper() });

    expect(
      screen.getByText("All members"),
    ).toBeInTheDocument();
  });

  it("does not show the member filter in org view for non-platform managers", () => {
    vi.mocked(useAuthorization).mockReturnValue({
      userRoles: ["StandardUser", "BatchAPIUser"],
      isLoading: false,
      hasPermission: (permission: string) => permission === "batches",
      canAccessRoute: () => true,
      getFirstAccessibleRoute: () => "/batches",
    } as any);

    vi.mocked(useOrganizationContext).mockReturnValue({
      activeOrganizationId: "org-1",
      activeOrganization: null,
      isOrgContext: true,
      setActiveOrganization: vi.fn(),
    } as any);

    render(<AsyncRequests />, { wrapper: createWrapper() });

    expect(screen.queryByText("All members")).not.toBeInTheDocument();
    expect(hooks.useAsyncRequests).toHaveBeenCalledWith(
      expect.objectContaining({
        member_id: undefined,
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
});
