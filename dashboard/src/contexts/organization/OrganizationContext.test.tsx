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
      setActive: vi.fn().mockResolvedValue({ active_organization_id: null }),
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

  it("invalidates the asyncRequests cache when the user switches org, only after the server-side switch resolves", async () => {
    const queryClient = new QueryClient({
      defaultOptions: { queries: { retry: false } },
    });
    const invalidateSpy = vi.spyOn(queryClient, "invalidateQueries");

    // Defer setActive so we can assert no invalidation has fired yet.
    let resolveSetActive!: (
      value: import("../../api/control-layer/types").SetActiveOrganizationResponse,
    ) => void;
    vi.mocked(dwctlApi.organizations.setActive).mockReturnValueOnce(
      new Promise((resolve) => {
        resolveSetActive = resolve;
      }),
    );

    const { result } = renderHook(() => useContext(OrganizationContext)!, {
      wrapper: makeWrapper(queryClient),
    });

    let switchPromise: Promise<void>;
    act(() => {
      switchPromise = result.current.setActiveOrganization("org-1");
    });

    // Before the server confirms the switch, no asyncRequests invalidation
    // should have fired — otherwise the cache would repopulate with the old
    // org's data while the cookie is still being set.
    {
      const calls = invalidateSpy.mock.calls.map(([arg]) => arg?.queryKey);
      expect(calls).not.toContainEqual(queryKeys.asyncRequests.all);
    }

    await act(async () => {
      resolveSetActive({ active_organization_id: "org-1" });
      await switchPromise!;
    });

    expect(dwctlApi.organizations.setActive).toHaveBeenCalledWith("org-1");
    const calls = invalidateSpy.mock.calls.map(([arg]) => arg?.queryKey);
    expect(calls).toContainEqual(queryKeys.asyncRequests.all);
  });
});
