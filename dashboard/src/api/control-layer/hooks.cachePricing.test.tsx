import { describe, it, expect, beforeEach, vi } from "vitest";
import { renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  useModelCachePricing,
  useUpdateModelCachePricing,
  useDeleteModelCachePricing,
} from "./hooks";
import { dwctlApi } from "./client";
import { queryKeys } from "./keys";
import type { CachePricing } from "./types";

// Only the cache-pricing client surface is exercised here; the hooks under test touch
// nothing else on `dwctlApi`. `setAiApiBaseUrl` is imported by the hooks module but only
// called inside other hooks, so a bare mock is enough.
vi.mock("./client", () => ({
  dwctlApi: {
    models: {
      cachePricing: {
        get: vi.fn(),
        update: vi.fn(),
        disable: vi.fn(),
      },
    },
  },
  setAiApiBaseUrl: vi.fn(),
}));

const mockPricing: CachePricing = {
  enabled: true,
  write_multiplier_5m: "1.2500",
  write_multiplier_1h: "2.0000",
  write_multiplier_24h: "2.5000",
  read_multiplier: "0.1000",
  min_prefix_tokens: 1024,
  valid_from: "2024-01-01T00:00:00Z",
  valid_until: null,
};

describe("Cache pricing hooks", () => {
  let queryClient: QueryClient;

  beforeEach(() => {
    queryClient = new QueryClient({
      defaultOptions: {
        queries: { retry: false },
        mutations: { retry: false },
      },
    });
    vi.clearAllMocks();
  });

  const wrapper = ({ children }: { children: React.ReactNode }) => (
    <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
  );

  it("useModelCachePricing fetches via the dedicated endpoint", async () => {
    vi.mocked(dwctlApi.models.cachePricing.get).mockResolvedValue(mockPricing);

    const { result } = renderHook(() => useModelCachePricing("model-1"), {
      wrapper,
    });

    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    expect(dwctlApi.models.cachePricing.get).toHaveBeenCalledWith("model-1");
    expect(result.current.data).toEqual(mockPricing);
  });

  it("useModelCachePricing respects enabled: false (admin-gated callers)", async () => {
    vi.mocked(dwctlApi.models.cachePricing.get).mockResolvedValue(mockPricing);

    const { result } = renderHook(
      () => useModelCachePricing("model-1", { enabled: false }),
      { wrapper },
    );

    // Give the query a beat — it must not fire when disabled.
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(dwctlApi.models.cachePricing.get).not.toHaveBeenCalled();
    expect(result.current.data).toBeUndefined();
  });

  it("useUpdateModelCachePricing invalidates pricing, model, and list caches", async () => {
    vi.mocked(dwctlApi.models.cachePricing.update).mockResolvedValue(
      mockPricing,
    );
    const invalidateSpy = vi.spyOn(queryClient, "invalidateQueries");

    const { result } = renderHook(() => useUpdateModelCachePricing(), {
      wrapper,
    });

    await result.current.mutateAsync({
      modelId: "model-1",
      data: { write_multiplier_5m: "1.25" },
    });

    expect(dwctlApi.models.cachePricing.update).toHaveBeenCalledWith("model-1", {
      write_multiplier_5m: "1.25",
    });
    expect(invalidateSpy).toHaveBeenCalledWith({
      queryKey: queryKeys.models.cachePricing("model-1"),
    });
    expect(invalidateSpy).toHaveBeenCalledWith({
      queryKey: queryKeys.models.byId("model-1"),
    });
    expect(invalidateSpy).toHaveBeenCalledWith({
      queryKey: queryKeys.models.all,
    });
  });

  it("useDeleteModelCachePricing invalidates pricing, model, and list caches", async () => {
    vi.mocked(dwctlApi.models.cachePricing.disable).mockResolvedValue(undefined);
    const invalidateSpy = vi.spyOn(queryClient, "invalidateQueries");

    const { result } = renderHook(() => useDeleteModelCachePricing(), {
      wrapper,
    });

    await result.current.mutateAsync("model-1");

    expect(dwctlApi.models.cachePricing.disable).toHaveBeenCalledWith("model-1");
    expect(invalidateSpy).toHaveBeenCalledWith({
      queryKey: queryKeys.models.cachePricing("model-1"),
    });
    expect(invalidateSpy).toHaveBeenCalledWith({
      queryKey: queryKeys.models.byId("model-1"),
    });
    expect(invalidateSpy).toHaveBeenCalledWith({
      queryKey: queryKeys.models.all,
    });
  });
});
