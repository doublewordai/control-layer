import { beforeEach, describe, expect, it, vi } from "vitest";
import { renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { useOrganizationsByIds } from "./hooks";
import { dwctlApi } from "./client";
import { queryKeys } from "./keys";
import type { Organization } from "./types";

vi.mock("./client", () => ({
  dwctlApi: { organizations: { get: vi.fn() } },
  setAiApiBaseUrl: vi.fn(),
}));

describe("organisation price-scope names", () => {
  let client: QueryClient;
  beforeEach(() => {
    client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    vi.clearAllMocks();
  });
  const wrapper = ({ children }: { children: React.ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );

  it("reuses detail cache and fetches each missing organisation only once", async () => {
    const cached = { id: "cached", username: "cached-org", display_name: "Cached" };
    const missing = { id: "missing", username: "price-org", display_name: "Price Organisation" };
    client.setQueryData(queryKeys.organizations.byId("cached"), cached);
    vi.mocked(dwctlApi.organizations.get).mockResolvedValue(missing as Organization);
    const { result } = renderHook(
      () => useOrganizationsByIds(["cached", "missing", "missing"]),
      { wrapper },
    );
    await waitFor(() => expect(result.current.every((query) => query.isSuccess)).toBe(true));
    expect(result.current.map((query) => query.data)).toEqual([cached, missing]);
    expect(dwctlApi.organizations.get).toHaveBeenCalledTimes(1);
    expect(dwctlApi.organizations.get).toHaveBeenCalledWith("missing");
  });

  it("makes no requests when no names are needed", () => {
    const { result } = renderHook(() => useOrganizationsByIds([]), { wrapper });
    expect(result.current).toEqual([]);
    expect(dwctlApi.organizations.get).not.toHaveBeenCalled();
  });
});
