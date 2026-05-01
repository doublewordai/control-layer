import { describe, it, expect, vi, beforeEach } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { useContext, type ReactNode } from "react";
import { OrganizationProvider } from "./OrganizationContext";
import { OrganizationContext } from "./context";
import { dwctlApi } from "../../api/control-layer/client";
import { queryKeys } from "../../api/control-layer/keys";

vi.mock("../../api/control-layer/hooks", () => ({
  useUser: vi.fn(() => ({
    data: {
      id: "user-1",
      email: "test@example.com",
      active_organization_id: null,
      organizations: [],
    },
    isLoading: false,
  })),
}));

vi.mock("../../api/control-layer/client", () => ({
  dwctlApi: {
    organizations: {
      setActive: vi.fn().mockResolvedValue(undefined),
    },
  },
}));

function makeWrapper(client: QueryClient) {
  return function Wrapper({ children }: { children: ReactNode }) {
    return (
      <QueryClientProvider client={client}>
        <OrganizationProvider>{children}</OrganizationProvider>
      </QueryClientProvider>
    );
  };
}

describe("OrganizationProvider.setActiveOrganization", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("invalidates the asyncRequests cache when the user switches org", async () => {
    const queryClient = new QueryClient({
      defaultOptions: { queries: { retry: false } },
    });
    const invalidateSpy = vi.spyOn(queryClient, "invalidateQueries");

    const { result } = renderHook(() => useContext(OrganizationContext)!, {
      wrapper: makeWrapper(queryClient),
    });

    await act(async () => {
      await result.current.setActiveOrganization("org-1");
    });

    expect(dwctlApi.organizations.setActive).toHaveBeenCalledWith("org-1");

    // The new asyncRequests invalidation must be issued. Other resource
    // caches are also invalidated; we just assert ours specifically so the
    // test stays focused on the bug being fixed.
    const calls = invalidateSpy.mock.calls.map(([arg]) => arg?.queryKey);
    expect(calls).toContainEqual(queryKeys.asyncRequests.all);
  });
});
