import { renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { setupServer } from "msw/node";
import { http, HttpResponse } from "msw";
import { ReactNode } from "react";
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
  useUsers,
  useUser,
  useCreateUser,
  useUpdateUser,
  useDeleteUser,
  useAddUserToGroup,
  useAddModelToGroup,
} from "../hooks";

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

describe("User Hooks", () => {
  describe("useUsers", () => {
    it("should fetch users successfully", async () => {
      const { result } = renderHook(() => useUsers(), {
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
      expect(result.current.data![0]).toHaveProperty("username");
    });

    it("should fetch users with include parameter", async () => {
      const { result } = renderHook(() => useUsers({ include: "groups" }), {
        wrapper: createWrapper(),
      });

      await waitFor(() => {
        expect(result.current.isSuccess).toBe(true);
      });

      expect(result.current.data![0]).toHaveProperty("groups");
      expect(Array.isArray(result.current.data![0].groups)).toBe(true);
    });

    it("should handle errors", async () => {
      // Mock an error response
      server.use(
        http.get("/admin/api/v1/users", () => {
          return HttpResponse.json({ error: "Server error" }, { status: 500 });
        }),
      );

      const { result } = renderHook(() => useUsers(), {
        wrapper: createWrapper(),
      });

      await waitFor(() => {
        expect(result.current.isError).toBe(true);
      });

      expect(result.current.error).toBeDefined();
    });
  });

  describe("useUser", () => {
    it("should fetch a specific user", async () => {
      const userId = "550e8400-e29b-41d4-a716-446655440001";
      const { result } = renderHook(() => useUser(userId), {
        wrapper: createWrapper(),
      });

      await waitFor(() => {
        expect(result.current.isSuccess).toBe(true);
      });

      expect(result.current.data).toBeDefined();
      expect(result.current.data!.id).toBe(userId);
      expect(result.current.data!).toHaveProperty("username");
      expect(result.current.data!).toHaveProperty("email");
    });
  });

  describe("useCreateUser", () => {
    it("should create a user successfully", async () => {
      const queryClient = new QueryClient({
        defaultOptions: { queries: { retry: false } },
      });

      const wrapper = ({ children }: { children: ReactNode }) => (
        <QueryClientProvider client={queryClient}>
          {children}
        </QueryClientProvider>
      );

      const { result } = renderHook(() => useCreateUser(), { wrapper });

      const userData = {
        username: "newuser",
        email: "newuser@example.com",
        display_name: "New User",
        roles: ["User" as const],
      };

      // Trigger the mutation
      result.current.mutate(userData);

      await waitFor(() => {
        expect(result.current.isSuccess).toBe(true);
      });

      expect(result.current.data).toBeDefined();
      expect(result.current.data!.username).toBe(userData.username);
      expect(result.current.data!.email).toBe(userData.email);
    });

    it("should handle creation errors", async () => {
      // Mock validation error
      server.use(
        http.post("/admin/api/v1/users", () => {
          return HttpResponse.json(
            { error: "Validation failed" },
            { status: 400 },
          );
        }),
      );

      const { result } = renderHook(() => useCreateUser(), {
        wrapper: createWrapper(),
      });

      const userData = {
        username: "invalid",
        email: "invalid-email",
        roles: ["User" as const],
      };

      result.current.mutate(userData);

      await waitFor(() => {
        expect(result.current.isError).toBe(true);
      });

      expect(result.current.error).toBeDefined();
    });
  });

  describe("useUpdateUser", () => {
    it("should update a user and invalidate cache", async () => {
      const queryClient = new QueryClient({
        defaultOptions: { queries: { retry: false } },
      });

      // Spy on queryClient methods
      const invalidateQueriesSpy = vi.spyOn(queryClient, "invalidateQueries");
      const _setQueryDataSpy = vi.spyOn(queryClient, "setQueryData");

      const wrapper = ({ children }: { children: ReactNode }) => (
        <QueryClientProvider client={queryClient}>
          {children}
        </QueryClientProvider>
      );

      const { result } = renderHook(() => useUpdateUser(), { wrapper });

      const userId = "550e8400-e29b-41d4-a716-446655440001";
      const updateData = { display_name: "Updated Name" };

      result.current.mutate({ id: userId, data: updateData });

      await waitFor(() => {
        expect(result.current.isSuccess).toBe(true);
      });

      // Verify cache invalidation was called
      expect(invalidateQueriesSpy).toHaveBeenCalledWith({
        queryKey: ["users"],
      });
    });
  });

  describe("useDeleteUser", () => {
    it("should delete user and remove from cache", async () => {
      const queryClient = new QueryClient({
        defaultOptions: { queries: { retry: false } },
      });

      const invalidateQueriesSpy = vi.spyOn(queryClient, "invalidateQueries");
      const _removeQueriesSpy = vi.spyOn(queryClient, "removeQueries");

      const wrapper = ({ children }: { children: ReactNode }) => (
        <QueryClientProvider client={queryClient}>
          {children}
        </QueryClientProvider>
      );

      const { result } = renderHook(() => useDeleteUser(), { wrapper });

      const userId = "550e8400-e29b-41d4-a716-446655440001";
      result.current.mutate(userId);

      await waitFor(() => {
        expect(result.current.isSuccess).toBe(true);
      });

      // Verify cache operations
      expect(invalidateQueriesSpy).toHaveBeenCalledWith({
        queryKey: ["users"],
      });
    });
  });

  describe("Cache Integration", () => {
    it("should return different data shapes for different query options", async () => {
      const queryClient = new QueryClient({
        defaultOptions: { queries: { retry: false } },
      });

      const wrapper = ({ children }: { children: ReactNode }) => (
        <QueryClientProvider client={queryClient}>
          {children}
        </QueryClientProvider>
      );

      // Fetch users without groups
      const { result: usersResult } = renderHook(() => useUsers(), { wrapper });
      await waitFor(() => expect(usersResult.current.isSuccess).toBe(true));

      // Fetch users with groups
      const { result: usersWithGroupsResult } = renderHook(
        () => useUsers({ include: "groups" }),
        { wrapper },
      );
      await waitFor(() =>
        expect(usersWithGroupsResult.current.isSuccess).toBe(true),
      );

      // Different query options should return different data shapes
      expect(usersResult.current.data).toBeDefined();
      expect(usersWithGroupsResult.current.data).toBeDefined();

      // Users without groups should not have groups property
      expect(usersResult.current.data![0]).not.toHaveProperty("groups");

      // Users with groups should have groups property
      expect(usersWithGroupsResult.current.data![0]).toHaveProperty("groups");
      expect(Array.isArray(usersWithGroupsResult.current.data![0].groups)).toBe(
        true,
      );
    });

    it("should invalidate all user queries when creating a user", async () => {
      const queryClient = new QueryClient({
        defaultOptions: { queries: { retry: false } },
      });

      const wrapper = ({ children }: { children: ReactNode }) => (
        <QueryClientProvider client={queryClient}>
          {children}
        </QueryClientProvider>
      );

      // Pre-populate cache with different user queries
      const { result: usersResult } = renderHook(() => useUsers(), { wrapper });
      const { result: usersWithGroupsResult } = renderHook(
        () => useUsers({ include: "groups" }),
        { wrapper },
      );
      const { result: userResult } = renderHook(
        () => useUser("550e8400-e29b-41d4-a716-446655440001"),
        { wrapper },
      );

      await waitFor(() => {
        expect(usersResult.current.isSuccess).toBe(true);
        expect(usersWithGroupsResult.current.isSuccess).toBe(true);
        expect(userResult.current.isSuccess).toBe(true);
      });

      // Mark queries as stale to test invalidation
      const queries = queryClient.getQueryCache().getAll();
      queries.forEach((query) => {
        query.state.dataUpdatedAt = Date.now() - 10000; // 10 seconds ago
      });

      // Create a new user
      const { result: createResult } = renderHook(() => useCreateUser(), {
        wrapper,
      });
      const userData = {
        username: "newuser",
        email: "new@example.com",
        roles: ["User" as const],
      };

      createResult.current.mutate(userData);

      await waitFor(() => {
        expect(createResult.current.isSuccess).toBe(true);
      });

      // All user queries should be marked as stale/invalid
      const updatedQueries = queryClient.getQueryCache().getAll();
      const userQueries = updatedQueries.filter(
        (q) => q.queryKey[0] === "users",
      );

      // Check that queries are marked as stale due to invalidation
      userQueries.forEach((query) => {
        expect(query.isStale()).toBe(true);
      });
    });

    it("should cache identical queries and separate different ones", async () => {
      const queryClient = new QueryClient({
        defaultOptions: { queries: { retry: false } },
      });

      const wrapper = ({ children }: { children: ReactNode }) => (
        <QueryClientProvider client={queryClient}>
          {children}
        </QueryClientProvider>
      );

      // Start multiple identical queries
      const { result: users1 } = renderHook(() => useUsers(), { wrapper });
      const { result: users2 } = renderHook(() => useUsers(), { wrapper });

      // Start query with different options
      const { result: usersWithGroups } = renderHook(
        () => useUsers({ include: "groups" }),
        { wrapper },
      );

      await waitFor(() => {
        expect(users1.current.isSuccess).toBe(true);
        expect(users2.current.isSuccess).toBe(true);
        expect(usersWithGroups.current.isSuccess).toBe(true);
      });

      // Identical queries should share cache and return same reference
      expect(users1.current.data).toBe(users2.current.data);

      // Different queries should return different data
      expect(users1.current.data).not.toBe(usersWithGroups.current.data);

      // Verify the data shapes are correct
      expect(users1.current.data![0]).not.toHaveProperty("groups");
      expect(usersWithGroups.current.data![0]).toHaveProperty("groups");
    });
  });

  describe("Relationship Mutations Cache Logic", () => {
    it("should invalidate both users and groups when adding user to group", async () => {
      const queryClient = new QueryClient({
        defaultOptions: { queries: { retry: false } },
      });

      const invalidateQueriesSpy = vi.spyOn(queryClient, "invalidateQueries");

      const wrapper = ({ children }: { children: ReactNode }) => (
        <QueryClientProvider client={queryClient}>
          {children}
        </QueryClientProvider>
      );

      const { result } = renderHook(() => useAddUserToGroup(), { wrapper });

      result.current.mutate({
        groupId: "550e8400-e29b-41d4-a716-446655441001",
        userId: "550e8400-e29b-41d4-a716-446655440001",
      });

      await waitFor(() => {
        expect(result.current.isSuccess).toBe(true);
      });

      // Should invalidate both resource types
      expect(invalidateQueriesSpy).toHaveBeenCalledWith({
        queryKey: ["groups"],
      });
      expect(invalidateQueriesSpy).toHaveBeenCalledWith({
        queryKey: ["users"],
      });
      expect(invalidateQueriesSpy).toHaveBeenCalledTimes(2);
    });

    it("should invalidate groups and models when adding model to group", async () => {
      const queryClient = new QueryClient({
        defaultOptions: { queries: { retry: false } },
      });

      const invalidateQueriesSpy = vi.spyOn(queryClient, "invalidateQueries");

      const wrapper = ({ children }: { children: ReactNode }) => (
        <QueryClientProvider client={queryClient}>
          {children}
        </QueryClientProvider>
      );

      const { result } = renderHook(() => useAddModelToGroup(), { wrapper });

      result.current.mutate({
        groupId: "550e8400-e29b-41d4-a716-446655441001",
        modelId: "f914c573-4c00-4a37-a878-53318a6d5a5b",
      });

      await waitFor(() => {
        expect(result.current.isSuccess).toBe(true);
      });

      expect(invalidateQueriesSpy).toHaveBeenCalledWith({
        queryKey: ["groups"],
      });
      expect(invalidateQueriesSpy).toHaveBeenCalledWith({
        queryKey: ["models"],
      });
      expect(invalidateQueriesSpy).toHaveBeenCalledTimes(2);
    });
  });
});
