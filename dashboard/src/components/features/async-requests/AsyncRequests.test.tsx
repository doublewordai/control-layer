import { describe, it, expect, vi, beforeEach } from "vitest";
import { render, within } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { MemoryRouter } from "react-router-dom";
import { AsyncRequests } from "./AsyncRequests";
import * as hooks from "../../../api/control-layer/hooks";

// Mock the hooks
vi.mock("../../../api/control-layer/hooks", () => ({
  useConfig: vi.fn(),
  useAsyncRequests: vi.fn(),
  useModels: vi.fn(),
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
});
