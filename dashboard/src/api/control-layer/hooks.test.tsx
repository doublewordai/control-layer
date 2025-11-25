import { describe, it, expect, beforeEach, vi } from "vitest";
import { renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { useModels, useModel, useEndpoints, useEndpoint } from "./hooks";
import { dwctlApi } from "./client";
import type { Model, Endpoint } from "./types";

// Mock the API client
vi.mock("./client", () => ({
  dwctlApi: {
    models: {
      list: vi.fn(),
      get: vi.fn(),
    },
    endpoints: {
      list: vi.fn(),
      get: vi.fn(),
    },
  },
}));

// Test data
const mockModels: Model[] = [
  {
    id: "model-1",
    alias: "gpt-4",
    model_name: "gpt-4-turbo",
    hosted_on: "endpoint-1",
    description: "Test model 1",
    model_type: "CHAT",
    capabilities: ["chat"],
  },
  {
    id: "model-2",
    alias: "claude",
    model_name: "claude-3-opus",
    hosted_on: "endpoint-2",
    description: "Test model 2",
    model_type: "CHAT",
    capabilities: ["chat"],
  },
];

const mockModelDetail: Model = {
  ...mockModels[0],
  groups: [{ id: "group-1", name: "Test Group", source: "user" }],
  metrics: {
    total_requests: 100,
    total_input_tokens: 1000,
    total_output_tokens: 2000,
    avg_latency_ms: 150,
    last_active_at: "2024-01-01T00:00:00Z",
  },
};

const mockEndpoints: Endpoint[] = [
  {
    id: "endpoint-1",
    name: "OpenAI",
    url: "https://api.openai.com/v1",
    created_at: "2024-01-01T00:00:00Z",
    updated_at: "2024-01-01T00:00:00Z",
    created_by: "user-1",
    requires_api_key: true,
  },
  {
    id: "endpoint-2",
    name: "Anthropic",
    url: "https://api.anthropic.com/v1",
    created_at: "2024-01-01T00:00:00Z",
    updated_at: "2024-01-01T00:00:00Z",
    created_by: "user-1",
    requires_api_key: true,
  },
];

describe("Model cache optimization", () => {
  let queryClient: QueryClient;

  beforeEach(() => {
    // Create a fresh QueryClient for each test
    queryClient = new QueryClient({
      defaultOptions: {
        queries: {
          retry: false,
        },
      },
    });
    vi.clearAllMocks();
  });

  const wrapper = ({ children }: { children: React.ReactNode }) => (
    <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
  );

  it("should populate individual model cache when fetching list", async () => {
    // Mock the list API call
    vi.mocked(dwctlApi.models.list).mockResolvedValue({
      data: mockModels,
      skip: 0,
      limit: 10,
      total_count: mockModels.length,
    });

    // Fetch the models list
    const { result } = renderHook(() => useModels({ limit: 10 }), { wrapper });

    // Wait for the query to succeed
    await waitFor(() => expect(result.current.isSuccess).toBe(true));

    // Verify the list data is correct
    expect(result.current.data?.data).toEqual(mockModels);

    // Wait for cache population (select function runs after data is returned)
    await waitFor(() => {
      const cachedModel1 = queryClient.getQueryData([
        "models",
        "byId",
        "model-1",
        undefined,
      ]);
      return cachedModel1 !== undefined;
    });

    // Check that individual models are now in the cache (with undefined include param)
    const cachedModel1 = queryClient.getQueryData([
      "models",
      "byId",
      "model-1",
      undefined,
    ]);
    const cachedModel2 = queryClient.getQueryData([
      "models",
      "byId",
      "model-2",
      undefined,
    ]);

    expect(cachedModel1).toEqual(mockModels[0]);
    expect(cachedModel2).toEqual(mockModels[1]);
  });

  it("should use cached data as placeholder then fetch detailed data", async () => {
    // First, populate the cache with list data (basic model without groups/metrics)
    vi.mocked(dwctlApi.models.list).mockResolvedValue({
      data: mockModels,
      skip: 0,
      limit: 10,
      total_count: mockModels.length,
    });

    const { result: listResult } = renderHook(() => useModels({ limit: 10 }), {
      wrapper,
    });

    await waitFor(() => expect(listResult.current.isSuccess).toBe(true));

    // Wait for cache population (select function populates individual caches)
    await waitFor(() => {
      const cachedModel = queryClient.getQueryData([
        "models",
        "byId",
        "model-1",
        undefined,
      ]);
      return cachedModel !== undefined;
    });

    // Verify the basic model is cached without groups/metrics
    const cachedBasicModel = queryClient.getQueryData([
      "models",
      "byId",
      "model-1",
      undefined,
    ]) as Model;
    expect(cachedBasicModel.id).toBe("model-1");
    expect(cachedBasicModel.groups).toBeUndefined();
    expect(cachedBasicModel.metrics).toBeUndefined();

    // Now mock the API to return detailed data with groups/metrics
    vi.mocked(dwctlApi.models.get).mockResolvedValue(mockModelDetail);

    // Fetch individual model with include params
    const { result: detailResult } = renderHook(
      () => useModel("model-1", { include: "groups,metrics" }),
      {
        wrapper,
      },
    );

    // The hook should immediately have placeholder data (basic model)
    expect(detailResult.current.data).toBeDefined();
    expect(detailResult.current.data?.id).toBe("model-1");
    expect(detailResult.current.data?.alias).toBe("gpt-4");
    // Placeholder data doesn't have groups/metrics yet
    expect(detailResult.current.data?.groups).toBeUndefined();

    // Wait for the API to be called
    await waitFor(() => {
      return dwctlApi.models.get.mock.calls.length > 0;
    });

    // Verify the API was called with the include param
    expect(dwctlApi.models.get).toHaveBeenCalledWith("model-1", {
      include: "groups,metrics",
    });

    // Wait for the query to complete successfully and data to update
    await waitFor(() => {
      return (
        detailResult.current.isSuccess &&
        detailResult.current.data?.groups !== undefined
      );
    });

    // Now it should have the detailed data with additional fields
    expect(detailResult.current.data?.id).toBe("model-1");
    expect(detailResult.current.data?.groups).toBeDefined();
    expect(detailResult.current.data?.metrics).toBeDefined();
    expect(detailResult.current.data?.groups?.[0]?.name).toBe("Test Group");
    expect(detailResult.current.data?.metrics?.total_requests).toBe(100);
  });

  it("should use populated cache as placeholder", async () => {
    // Populate list cache which auto-populates individual caches
    vi.mocked(dwctlApi.models.list).mockResolvedValue({
      data: mockModels,
      skip: 0,
      limit: 10,
      total_count: mockModels.length,
    });

    const { result: listResult } = renderHook(() => useModels({ limit: 10 }), {
      wrapper,
    });

    await waitFor(() => expect(listResult.current.isSuccess).toBe(true));

    // Wait for individual cache population via select
    await waitFor(() => {
      const cached = queryClient.getQueryData([
        "models",
        "byId",
        "model-2",
        undefined,
      ]);
      return cached !== undefined;
    });

    // Mock the detail fetch
    vi.mocked(dwctlApi.models.get).mockResolvedValue({
      ...mockModels[1],
      groups: [],
    });

    // Fetch the model - it should use the auto-populated cache as placeholder
    const { result: detailResult } = renderHook(() => useModel("model-2"), {
      wrapper,
    });

    // Should immediately have placeholder data from the populated cache
    expect(detailResult.current.data).toEqual(mockModels[1]);
  });

  it("should not use placeholder data if model not in any cache", async () => {
    // Don't populate any cache
    vi.mocked(dwctlApi.models.get).mockResolvedValue(mockModelDetail);

    const { result } = renderHook(() => useModel("model-1"), { wrapper });

    // Should start with no data
    expect(result.current.data).toBeUndefined();
    expect(result.current.isLoading).toBe(true);

    // Wait for fetch
    await waitFor(() => expect(result.current.isSuccess).toBe(true));

    expect(result.current.data).toEqual(mockModelDetail);
  });

  it("should handle multiple pagination queries and still find cached models", async () => {
    // First page
    vi.mocked(dwctlApi.models.list).mockResolvedValue({
      data: [mockModels[0]],
      skip: 0,
      limit: 10,
      total_count: 2,
    });

    const { result: page1Result } = renderHook(
      () => useModels({ skip: 0, limit: 1 }),
      { wrapper },
    );

    await waitFor(() => expect(page1Result.current.isSuccess).toBe(true));

    // Second page
    vi.mocked(dwctlApi.models.list).mockResolvedValue({
      data: [mockModels[1]],
      skip: 0,
      limit: 10,
      total_count: 2,
    });

    const { result: page2Result } = renderHook(
      () => useModels({ skip: 1, limit: 1 }),
      { wrapper },
    );

    await waitFor(() => expect(page2Result.current.isSuccess).toBe(true));

    // Now fetch a model from page 2
    vi.mocked(dwctlApi.models.get).mockResolvedValue({
      ...mockModels[1],
      groups: [],
    });

    const { result: detailResult } = renderHook(() => useModel("model-2"), {
      wrapper,
    });

    // Should find it in the page 2 cache as placeholder
    expect(detailResult.current.data).toEqual(mockModels[1]);
  });
});

describe("Endpoint cache optimization", () => {
  let queryClient: QueryClient;

  beforeEach(() => {
    // Create a fresh QueryClient for each test
    queryClient = new QueryClient({
      defaultOptions: {
        queries: {
          retry: false,
        },
      },
    });
    vi.clearAllMocks();
  });

  const wrapper = ({ children }: { children: React.ReactNode }) => (
    <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
  );

  it("should populate individual endpoint cache when fetching list", async () => {
    // Mock the list API call
    vi.mocked(dwctlApi.endpoints.list).mockResolvedValue(mockEndpoints);

    // Fetch the endpoints list
    const { result } = renderHook(() => useEndpoints(), { wrapper });

    // Wait for the query to succeed
    await waitFor(() => expect(result.current.isSuccess).toBe(true));

    // Verify the list data is correct
    expect(result.current.data).toEqual(mockEndpoints);

    // Wait for cache population (select function runs after data is returned)
    await waitFor(() => {
      const cachedEndpoint1 = queryClient.getQueryData([
        "endpoints",
        "byId",
        "endpoint-1",
      ]);
      return cachedEndpoint1 !== undefined;
    });

    // Check that individual endpoints are now in the cache
    const cachedEndpoint1 = queryClient.getQueryData([
      "endpoints",
      "byId",
      "endpoint-1",
    ]);
    const cachedEndpoint2 = queryClient.getQueryData([
      "endpoints",
      "byId",
      "endpoint-2",
    ]);

    expect(cachedEndpoint1).toEqual(mockEndpoints[0]);
    expect(cachedEndpoint2).toEqual(mockEndpoints[1]);
  });

  it("should use cached data as placeholder when fetching individual endpoint", async () => {
    // First, populate the cache with list data
    vi.mocked(dwctlApi.endpoints.list).mockResolvedValue(mockEndpoints);

    const { result: listResult } = renderHook(() => useEndpoints(), {
      wrapper,
    });

    await waitFor(() => expect(listResult.current.isSuccess).toBe(true));

    // Wait for cache population
    await waitFor(() => {
      const cachedEndpoint = queryClient.getQueryData([
        "endpoints",
        "byId",
        "endpoint-1",
      ]);
      return cachedEndpoint !== undefined;
    });

    // Now fetch individual endpoint - mock the API to return the same data
    vi.mocked(dwctlApi.endpoints.get).mockResolvedValue(mockEndpoints[0]);

    const { result: detailResult } = renderHook(
      () => useEndpoint("endpoint-1"),
      {
        wrapper,
      },
    );

    // The hook should immediately have placeholder data from the list cache
    expect(detailResult.current.data).toBeDefined();
    expect(detailResult.current.data?.id).toBe("endpoint-1");
    expect(detailResult.current.data?.name).toBe("OpenAI");

    // Wait for the actual fetch to complete
    await waitFor(() => detailResult.current.isSuccess);

    // Verify the data is still correct
    expect(detailResult.current.data?.id).toBe("endpoint-1");

    // Verify the API was called
    expect(dwctlApi.endpoints.get).toHaveBeenCalledWith("endpoint-1");
  });

  it("should respect enabled option in useEndpoint", async () => {
    // Mock the get API call
    vi.mocked(dwctlApi.endpoints.get).mockResolvedValue(mockEndpoints[0]);

    // Fetch endpoint with enabled: false
    const { result } = renderHook(
      () => useEndpoint("endpoint-1", { enabled: false }),
      { wrapper },
    );

    // Wait a bit to ensure no query is made
    await new Promise((resolve) => setTimeout(resolve, 100));

    // Should not have made the API call
    expect(dwctlApi.endpoints.get).not.toHaveBeenCalled();
    expect(result.current.data).toBeUndefined();
    expect(result.current.isLoading).toBe(false);
  });

  it("should use populated cache as placeholder", async () => {
    // Populate list cache which auto-populates individual caches
    vi.mocked(dwctlApi.endpoints.list).mockResolvedValue(mockEndpoints);

    const { result: listResult } = renderHook(() => useEndpoints(), {
      wrapper,
    });

    await waitFor(() => expect(listResult.current.isSuccess).toBe(true));

    // Wait for individual cache population via select
    await waitFor(() => {
      const cached = queryClient.getQueryData([
        "endpoints",
        "byId",
        "endpoint-2",
      ]);
      return cached !== undefined;
    });

    // Mock the detail fetch
    vi.mocked(dwctlApi.endpoints.get).mockResolvedValue(mockEndpoints[1]);

    // Fetch the endpoint - it should use the auto-populated cache as placeholder
    const { result: detailResult } = renderHook(
      () => useEndpoint("endpoint-2"),
      { wrapper },
    );

    // Should immediately have placeholder data from the populated cache
    expect(detailResult.current.data).toEqual(mockEndpoints[1]);
  });
});
