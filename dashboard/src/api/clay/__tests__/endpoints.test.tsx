import { renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { setupServer } from "msw/node";
import { http, HttpResponse } from "msw";
import React, { ReactNode } from "react";
import {
  vi,
  describe,
  it,
  expect,
  beforeAll,
  afterEach,
  afterAll,
} from "vitest";
import { handlers } from "../mocks/handlers";
import {
  useEndpoints,
  useEndpoint,
  useValidateEndpoint,
  useCreateEndpoint,
  useUpdateEndpoint,
  useDeleteEndpoint,
  useSynchronizeEndpoint,
} from "../hooks";
import type {
  EndpointValidateRequest,
  EndpointCreateRequest,
  EndpointUpdateRequest,
} from "../types";

// Setup MSW server
const server = setupServer(...handlers);

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

// Test wrapper with QueryClient
function createWrapper() {
  const queryClient = new QueryClient({
    defaultOptions: {
      queries: {
        retry: false, // Disable retries for tests
      },
    },
  });

  return ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={queryClient}>{children}</QueryClientProvider>
  );
}

describe("Endpoint Hooks", () => {
  describe("useEndpoints", () => {
    it("should fetch endpoints successfully", async () => {
      const { result } = renderHook(() => useEndpoints(), {
        wrapper: createWrapper(),
      });

      // Initially loading
      expect(result.current.isLoading).toBe(true);
      expect(result.current.data).toBeUndefined();

      // Wait for the query to resolve
      await waitFor(() => {
        expect(result.current.isSuccess).toBe(true);
      });

      expect(result.current.data).toBeDefined();
      expect(Array.isArray(result.current.data)).toBe(true);
      expect(result.current.data!.length).toBeGreaterThan(0);
      expect(result.current.data![0]).toHaveProperty("id");
      expect(result.current.data![0]).toHaveProperty("name");
      expect(result.current.data![0]).toHaveProperty("url");
    });

    it("should handle errors", async () => {
      // Mock an error response
      server.use(
        http.get("/admin/api/v1/endpoints", () => {
          return HttpResponse.json({ error: "Server error" }, { status: 500 });
        }),
      );

      const { result } = renderHook(() => useEndpoints(), {
        wrapper: createWrapper(),
      });

      await waitFor(() => {
        expect(result.current.isError).toBe(true);
      });

      expect(result.current.error).toBeDefined();
    });
  });

  describe("useEndpoint", () => {
    it("should fetch a specific endpoint", async () => {
      const endpointId = "a1b2c3d4-e5f6-7890-1234-567890abcdef";
      const { result } = renderHook(() => useEndpoint(endpointId), {
        wrapper: createWrapper(),
      });

      await waitFor(() => {
        expect(result.current.isSuccess).toBe(true);
      });

      expect(result.current.data).toBeDefined();
      expect(result.current.data!.id).toBe(
        "a1b2c3d4-e5f6-7890-1234-567890abcdef",
      );
      expect(result.current.data!).toHaveProperty("name");
      expect(result.current.data!).toHaveProperty("url");
    });

    it("should handle endpoint not found", async () => {
      server.use(
        http.get(
          "/admin/api/v1/endpoints/99999999-9999-9999-9999-999999999999",
          () => {
            return HttpResponse.json(
              { error: "Endpoint not found" },
              { status: 404 },
            );
          },
        ),
      );

      const { result } = renderHook(
        () => useEndpoint("99999999-9999-9999-9999-999999999999"),
        {
          wrapper: createWrapper(),
        },
      );

      await waitFor(() => {
        expect(result.current.isError).toBe(true);
      });

      expect(result.current.error).toBeDefined();
    });
  });

  describe("useValidateEndpoint", () => {
    it("should validate endpoint successfully", async () => {
      const { result } = renderHook(() => useValidateEndpoint(), {
        wrapper: createWrapper(),
      });

      const validateData: EndpointValidateRequest = {
        type: "new",
        url: "https://api.openai.com/v1",
        api_key: "sk-test123",
      };

      result.current.mutate(validateData);

      await waitFor(() => {
        expect(result.current.isSuccess).toBe(true);
      });

      expect(result.current.data).toBeDefined();
      expect(result.current.data!.status).toBe("success");
      expect(result.current.data!.models).toBeDefined();
      expect(result.current.data!.models!.data).toBeDefined();
      expect(Array.isArray(result.current.data!.models!.data)).toBe(true);
    });

    it("should handle validation errors", async () => {
      const { result } = renderHook(() => useValidateEndpoint(), {
        wrapper: createWrapper(),
      });

      const validateData: EndpointValidateRequest = {
        type: "new",
        url: "https://invalid-endpoint.com",
      };

      result.current.mutate(validateData);

      await waitFor(() => {
        expect(result.current.isSuccess).toBe(true);
      });

      expect(result.current.data!.status).toBe("error");
      expect(result.current.data!.error).toBeDefined();
    });

    it("should validate existing endpoint", async () => {
      const { result } = renderHook(() => useValidateEndpoint(), {
        wrapper: createWrapper(),
      });

      const validateData: EndpointValidateRequest = {
        type: "existing",
        endpoint_id: "a1b2c3d4-e5f6-7890-1234-567890abcdef",
      };

      result.current.mutate(validateData);

      await waitFor(() => {
        expect(result.current.isSuccess).toBe(true);
      });

      expect(result.current.data!.status).toBe("success");
    });
  });

  describe("useCreateEndpoint", () => {
    it("should create endpoint successfully", async () => {
      const queryClient = new QueryClient({
        defaultOptions: { queries: { retry: false } },
      });

      const wrapper = ({ children }: { children: ReactNode }) => (
        <QueryClientProvider client={queryClient}>
          {children}
        </QueryClientProvider>
      );

      const { result } = renderHook(() => useCreateEndpoint(), { wrapper });

      const endpointData: EndpointCreateRequest = {
        name: "Test Endpoint",
        description: "Test description",
        url: "https://api.example.com/v1",
        api_key: "sk-test123",
        model_filter: ["model1", "model2"],
      };

      result.current.mutate(endpointData);

      await waitFor(() => {
        expect(result.current.isSuccess).toBe(true);
      });

      expect(result.current.data).toBeDefined();
      expect(result.current.data!.name).toBe(endpointData.name);
      expect(result.current.data!.url).toBe(endpointData.url);
    });

    it("should invalidate endpoints cache after creation", async () => {
      const queryClient = new QueryClient({
        defaultOptions: { queries: { retry: false } },
      });

      const invalidateQueriesSpy = vi.spyOn(queryClient, "invalidateQueries");

      const wrapper = ({ children }: { children: ReactNode }) => (
        <QueryClientProvider client={queryClient}>
          {children}
        </QueryClientProvider>
      );

      const { result } = renderHook(() => useCreateEndpoint(), { wrapper });

      const endpointData: EndpointCreateRequest = {
        name: "Test Endpoint",
        url: "https://api.example.com/v1",
      };

      result.current.mutate(endpointData);

      await waitFor(() => {
        expect(result.current.isSuccess).toBe(true);
      });

      expect(invalidateQueriesSpy).toHaveBeenCalledWith({
        queryKey: ["endpoints"],
      });
    });
  });

  describe("useUpdateEndpoint", () => {
    it("should update endpoint successfully", async () => {
      const queryClient = new QueryClient({
        defaultOptions: { queries: { retry: false } },
      });

      const wrapper = ({ children }: { children: ReactNode }) => (
        <QueryClientProvider client={queryClient}>
          {children}
        </QueryClientProvider>
      );

      const { result } = renderHook(() => useUpdateEndpoint(), { wrapper });

      const updateData: EndpointUpdateRequest = {
        name: "Updated Endpoint Name",
        description: "Updated description",
      };

      result.current.mutate({
        id: "a1b2c3d4-e5f6-7890-1234-567890abcdef",
        data: updateData,
      });

      await waitFor(() => {
        expect(result.current.isSuccess).toBe(true);
      });

      expect(result.current.data).toBeDefined();
      expect(result.current.data!.name).toBe(updateData.name);
    });

    it("should invalidate cache after update", async () => {
      const queryClient = new QueryClient({
        defaultOptions: { queries: { retry: false } },
      });

      const invalidateQueriesSpy = vi.spyOn(queryClient, "invalidateQueries");

      const wrapper = ({ children }: { children: ReactNode }) => (
        <QueryClientProvider client={queryClient}>
          {children}
        </QueryClientProvider>
      );

      const { result } = renderHook(() => useUpdateEndpoint(), { wrapper });

      result.current.mutate({
        id: "a1b2c3d4-e5f6-7890-1234-567890abcdef",
        data: { name: "Updated Name" },
      });

      await waitFor(() => {
        expect(result.current.isSuccess).toBe(true);
      });

      expect(invalidateQueriesSpy).toHaveBeenCalledWith({
        queryKey: ["endpoints"],
      });
      expect(invalidateQueriesSpy).toHaveBeenCalledWith({
        queryKey: ["models"],
      });
      expect(invalidateQueriesSpy).toHaveBeenCalledWith({
        queryKey: ["endpoints", "byId", "a1b2c3d4-e5f6-7890-1234-567890abcdef"],
      });
    });
  });

  describe("useDeleteEndpoint", () => {
    it("should delete endpoint successfully", async () => {
      const queryClient = new QueryClient({
        defaultOptions: { queries: { retry: false } },
      });

      const wrapper = ({ children }: { children: ReactNode }) => (
        <QueryClientProvider client={queryClient}>
          {children}
        </QueryClientProvider>
      );

      const { result } = renderHook(() => useDeleteEndpoint(), { wrapper });

      result.current.mutate("a1b2c3d4-e5f6-7890-1234-567890abcdef");

      await waitFor(() => {
        expect(result.current.isSuccess).toBe(true);
      });
    });

    it("should invalidate cache after deletion", async () => {
      const queryClient = new QueryClient({
        defaultOptions: { queries: { retry: false } },
      });

      const invalidateQueriesSpy = vi.spyOn(queryClient, "invalidateQueries");

      const wrapper = ({ children }: { children: ReactNode }) => (
        <QueryClientProvider client={queryClient}>
          {children}
        </QueryClientProvider>
      );

      const { result } = renderHook(() => useDeleteEndpoint(), { wrapper });

      result.current.mutate("a1b2c3d4-e5f6-7890-1234-567890abcdef");

      await waitFor(() => {
        expect(result.current.isSuccess).toBe(true);
      });

      expect(invalidateQueriesSpy).toHaveBeenCalledWith({
        queryKey: ["endpoints"],
      });
      expect(invalidateQueriesSpy).toHaveBeenCalledWith({
        queryKey: ["models"],
      });
    });
  });

  describe("useSynchronizeEndpoint", () => {
    it("should synchronize endpoint successfully", async () => {
      const queryClient = new QueryClient({
        defaultOptions: { queries: { retry: false } },
      });

      const wrapper = ({ children }: { children: ReactNode }) => (
        <QueryClientProvider client={queryClient}>
          {children}
        </QueryClientProvider>
      );

      const { result } = renderHook(() => useSynchronizeEndpoint(), {
        wrapper,
      });

      result.current.mutate("a1b2c3d4-e5f6-7890-1234-567890abcdef");

      await waitFor(() => {
        expect(result.current.isSuccess).toBe(true);
      });

      expect(result.current.data).toBeDefined();
      expect(result.current.data!.endpoint_id).toBe(
        "a1b2c3d4-e5f6-7890-1234-567890abcdef",
      );
      expect(result.current.data!.changes_made).toBeDefined();
      expect(result.current.data!.synced_at).toBeDefined();
    });

    it("should handle synchronization errors", async () => {
      server.use(
        http.post(
          "/admin/api/v1/endpoints/99999999-9999-9999-9999-999999999999/synchronize",
          () => {
            return HttpResponse.json(
              { error: "Endpoint not found" },
              { status: 404 },
            );
          },
        ),
      );

      const { result } = renderHook(() => useSynchronizeEndpoint(), {
        wrapper: createWrapper(),
      });

      result.current.mutate("99999999-9999-9999-9999-999999999999");

      await waitFor(() => {
        expect(result.current.isError).toBe(true);
      });

      expect(result.current.error).toBeDefined();
    });

    it("should invalidate models cache after synchronization", async () => {
      const queryClient = new QueryClient({
        defaultOptions: { queries: { retry: false } },
      });

      const invalidateQueriesSpy = vi.spyOn(queryClient, "invalidateQueries");

      const wrapper = ({ children }: { children: ReactNode }) => (
        <QueryClientProvider client={queryClient}>
          {children}
        </QueryClientProvider>
      );

      const { result } = renderHook(() => useSynchronizeEndpoint(), {
        wrapper,
      });

      result.current.mutate("a1b2c3d4-e5f6-7890-1234-567890abcdef");

      await waitFor(() => {
        expect(result.current.isSuccess).toBe(true);
      });

      expect(invalidateQueriesSpy).toHaveBeenCalledWith({
        queryKey: ["models"],
      });
    });
  });
});
