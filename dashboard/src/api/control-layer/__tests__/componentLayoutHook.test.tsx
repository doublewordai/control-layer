import type { ReactNode } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { renderHook, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useUpdateModelComponentLayout } from "../hooks";
import { queryKeys } from "../keys";

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("component layout mutation", () => {
  it("invalidates stale component data after an exact-set conflict", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn().mockResolvedValue(
        new Response("Component set changed", { status: 409 }),
      ),
    );
    const queryClient = new QueryClient({
      defaultOptions: {
        queries: { retry: false },
        mutations: { retry: false },
      },
    });
    const key = queryKeys.models.components("pool");
    queryClient.setQueryData(key, []);
    const wrapper = ({ children }: { children: ReactNode }) => (
      <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
    );
    const { result } = renderHook(() => useUpdateModelComponentLayout(), {
      wrapper,
    });

    result.current.mutate({ modelId: "pool", components: [] });

    await waitFor(() => expect(result.current.isError).toBe(true));
    expect(queryClient.getQueryState(key)?.isInvalidated).toBe(true);
  });
});
