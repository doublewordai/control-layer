import { beforeAll, afterEach, afterAll, describe, it, expect } from "vitest";
import { setupServer } from "msw/node";
import { handlers } from "../mocks/handlers";
import { dwctlApi } from "../client";
import type {
  UserCreateRequest,
  GroupCreateRequest,
  UserUpdateRequest,
  GroupUpdateRequest,
  ModelUpdateRequest,
  ApiKeyCreateRequest,
} from "../types";

// Setup MSW server
const server = setupServer(...handlers);

// Start server before all tests
beforeAll(() => {
  server.listen({ onUnhandledRequest: "error" });
});

// Reset handlers after each test
afterEach(() => {
  server.resetHandlers();
});

// Close server after all tests
afterAll(() => {
  server.close();
});

describe("dwctlApi.users", () => {
  describe("list", () => {
    it("should fetch users without query parameters", async () => {
      const response = await dwctlApi.users.list();

      expect(response).toHaveProperty("data");
      expect(response.data).toBeInstanceOf(Array);
      expect(response.data.length).toBeGreaterThan(0);
      expect(response.data[0]).toHaveProperty("id");
      expect(response.data[0]).toHaveProperty("username");
      expect(response.data[0]).toHaveProperty("email");
      expect(response.data[0]).toHaveProperty("roles");
    });

    it("should fetch users with include=groups parameter", async () => {
      const response = await dwctlApi.users.list({ include: "groups" });

      expect(response).toHaveProperty("data");
      expect(response.data).toBeInstanceOf(Array);
      expect(response.data[0]).toHaveProperty("groups");
      expect(response.data[0].groups).toBeInstanceOf(Array);
    });

    it("should construct URL correctly with query parameters", async () => {
      // Test that the URL is constructed properly by checking the response
      const response = await dwctlApi.users.list({ include: "groups" });

      // The handler should return users with groups when include=groups
      expect(response.data[0]).toHaveProperty("groups");
    });
  });

  describe("get", () => {
    it("should fetch a specific user by ID", async () => {
      const userId = "550e8400-e29b-41d4-a716-446655440001";
      const user = await dwctlApi.users.get(userId);

      expect(user).toHaveProperty("id", userId);
      expect(user).toHaveProperty("username");
      expect(user).toHaveProperty("email");
    });

    it("should throw error for non-existent user", async () => {
      const nonExistentId = "non-existent-id";

      await expect(dwctlApi.users.get(nonExistentId)).rejects.toThrow(
        "Failed to fetch user: 404",
      );
    });
  });

  describe("create", () => {
    it("should create a new user", async () => {
      const userData: UserCreateRequest = {
        username: "newuser",
        email: "newuser@example.com",
        display_name: "New User",
        roles: ["StandardUser"],
      };

      const createdUser = await dwctlApi.users.create(userData);

      expect(createdUser).toHaveProperty("id");
      expect(createdUser.username).toBe(userData.username);
      expect(createdUser.email).toBe(userData.email);
      expect(createdUser.display_name).toBe(userData.display_name);
      expect(createdUser.roles).toEqual(userData.roles);
      expect(createdUser).toHaveProperty("created_at");
      expect(createdUser).toHaveProperty("updated_at");
    });

    it("should handle request serialization correctly", async () => {
      const userData: UserCreateRequest = {
        username: "testuser",
        email: "test@example.com",
        roles: ["PlatformManager", "StandardUser"],
        display_name: "Test User",
        avatar_url: "https://example.com/avatar.jpg",
      };

      const createdUser = await dwctlApi.users.create(userData);

      // Verify all fields are properly serialized and returned
      expect(createdUser.username).toBe(userData.username);
      expect(createdUser.email).toBe(userData.email);
      expect(createdUser.roles).toEqual(userData.roles);
      expect(createdUser.display_name).toBe(userData.display_name);
      expect(createdUser.avatar_url).toBe(userData.avatar_url);
    });
  });

  describe("update", () => {
    it("should update an existing user", async () => {
      const userId = "550e8400-e29b-41d4-a716-446655440001";
      const updateData: UserUpdateRequest = {
        display_name: "Updated Name",
        roles: ["PlatformManager"],
      };

      const updatedUser = await dwctlApi.users.update(userId, updateData);

      expect(updatedUser.id).toBe(userId);
      expect(updatedUser.display_name).toBe(updateData.display_name);
      expect(updatedUser.roles).toEqual(updateData.roles);
      expect(updatedUser).toHaveProperty("updated_at");
    });

    it("should throw error for non-existent user", async () => {
      const nonExistentId = "non-existent-id";
      const updateData: UserUpdateRequest = { display_name: "Updated" };

      await expect(
        dwctlApi.users.update(nonExistentId, updateData),
      ).rejects.toThrow("Failed to update user: 404");
    });
  });

  describe("delete", () => {
    it("should delete an existing user", async () => {
      const userId = "550e8400-e29b-41d4-a716-446655440001";

      await expect(dwctlApi.users.delete(userId)).resolves.toBeUndefined();
    });

    it("should throw error when deleting non-existent user", async () => {
      const nonExistentId = "non-existent-id";

      await expect(dwctlApi.users.delete(nonExistentId)).rejects.toThrow(
        "Failed to delete user: 404",
      );
    });
  });

  describe("apiKeys", () => {
    describe("getAll", () => {
      it("should fetch all API keys for current user", async () => {
        const response = await dwctlApi.users.apiKeys.getAll();

        expect(response).toHaveProperty("data");
        expect(response).toHaveProperty("total_count");
        expect(response).toHaveProperty("skip");
        expect(response).toHaveProperty("limit");
        expect(response.data).toBeInstanceOf(Array);
        expect(response.data.length).toBeGreaterThan(0);
        expect(response.data[0]).toHaveProperty("id");
        expect(response.data[0]).toHaveProperty("name");
        expect(response.data[0]).toHaveProperty("created_at");
      });

      it("should fetch API keys for specific user", async () => {
        const userId = "550e8400-e29b-41d4-a716-446655440001";
        const response = await dwctlApi.users.apiKeys.getAll(userId);

        expect(response).toHaveProperty("data");
        expect(response).toHaveProperty("total_count");
        expect(response.data).toBeInstanceOf(Array);
      });
    });

    describe("get", () => {
      it("should fetch specific API key", async () => {
        const keyId = "key-1";
        const apiKey = await dwctlApi.users.apiKeys.get(keyId);

        expect(apiKey).toHaveProperty("id", keyId);
        expect(apiKey).toHaveProperty("name");
      });

      it("should throw error for non-existent API key", async () => {
        const nonExistentId = "non-existent-key";

        await expect(dwctlApi.users.apiKeys.get(nonExistentId)).rejects.toThrow(
          "Failed to fetch API key: 404",
        );
      });
    });

    describe("create", () => {
      it("should create new API key and return key value", async () => {
        const keyData: ApiKeyCreateRequest = {
          name: "Test Key",
          description: "Test description",
          purpose: "realtime",
        };

        const createdKey = await dwctlApi.users.apiKeys.create(keyData);

        expect(createdKey).toHaveProperty("id");
        expect(createdKey).toHaveProperty("key"); // Only returned on creation
        expect(createdKey.name).toBe(keyData.name);
        expect(createdKey.description).toBe(keyData.description);
        expect(createdKey.key).toMatch(/^sk-/); // Should start with sk-
      });

      it("should create API key for specific user", async () => {
        const userId = "550e8400-e29b-41d4-a716-446655440001";
        const keyData: ApiKeyCreateRequest = {
          name: "User Key",
          purpose: "realtime",
        };

        const createdKey = await dwctlApi.users.apiKeys.create(keyData, userId);

        expect(createdKey).toHaveProperty("key");
        expect(createdKey.name).toBe(keyData.name);
      });
    });

    describe("delete", () => {
      it("should delete API key", async () => {
        const keyId = "key-1";

        await expect(
          dwctlApi.users.apiKeys.delete(keyId),
        ).resolves.toBeUndefined();
      });

      it("should throw error when deleting non-existent key", async () => {
        const nonExistentId = "non-existent-key";

        await expect(
          dwctlApi.users.apiKeys.delete(nonExistentId),
        ).rejects.toThrow("Failed to delete API key: 404");
      });
    });
  });
});

describe("dwctlApi.models", () => {
  describe("list", () => {
    it("should fetch all models", async () => {
      const response = await dwctlApi.models.list();

      expect(response).toHaveProperty("data");
      expect(response).toHaveProperty("total_count");
      expect(response.data).toBeInstanceOf(Array);
      expect(response.data.length).toBeGreaterThan(0);

      const firstModel = response.data[0];
      expect(firstModel).toHaveProperty("id");
      expect(firstModel).toHaveProperty("alias");
      expect(firstModel).toHaveProperty("model_name");
      expect(firstModel).toHaveProperty("hosted_on");
    });

    it("should filter models by endpoint", async () => {
      const response = await dwctlApi.models.list({ endpoint: "2" });

      expect(response).toHaveProperty("data");
      expect(response.data.every((model) => model.hosted_on === "2")).toBe(
        true,
      );
    });

    it("should include groups when requested", async () => {
      const response = await dwctlApi.models.list({ include: "groups" });

      const firstModel = response.data[0];
      expect(firstModel).toHaveProperty("groups");
      expect(firstModel.groups).toBeInstanceOf(Array);
    });

    it("should construct URL correctly with multiple parameters", async () => {
      const response = await dwctlApi.models.list({
        endpoint: "c3d4e5f6-7890-1234-5678-90abcdef0123",
        include: "groups",
      });

      expect(
        response.data.every(
          (model) => model.hosted_on === "c3d4e5f6-7890-1234-5678-90abcdef0123",
        ),
      ).toBe(true);
      expect(response.data[0]).toHaveProperty("groups");
    });
  });

  describe("get", () => {
    it("should fetch specific model", async () => {
      const modelId = "f914c573-4c00-4a37-a878-53318a6d5a5b";
      const model = await dwctlApi.models.get(modelId);

      expect(model).toHaveProperty("id", modelId);
      expect(model).toHaveProperty("alias");
      expect(model).toHaveProperty("model_name");
    });

    it("should throw error for non-existent model", async () => {
      const nonExistentId = "non-existent-model";

      await expect(dwctlApi.models.get(nonExistentId)).rejects.toThrow(
        "Failed to fetch model: 404",
      );
    });
  });

  describe("update", () => {
    it("should update model properties", async () => {
      const modelId = "f914c573-4c00-4a37-a878-53318a6d5a5b";
      const updateData: ModelUpdateRequest = {
        alias: "Updated Claude",
        description: "Updated description",
        capabilities: ["text", "vision", "code"],
      };

      const updatedModel = await dwctlApi.models.update(modelId, updateData);

      expect(updatedModel.alias).toBe(updateData.alias);
      expect(updatedModel.description).toBe(updateData.description);
      expect(updatedModel.capabilities).toEqual(updateData.capabilities);
    });

    it("should handle null values in updates", async () => {
      const modelId = "4c561f35-4823-4d25-aa70-72bbf314a6ba";
      const updateData: ModelUpdateRequest = {
        description: null,
        model_type: null,
      };

      const updatedModel = await dwctlApi.models.update(modelId, updateData);

      expect(updatedModel.description).toBeNull();
      expect(updatedModel.model_type).toBeNull();
    });
  });
});

describe("dwctlApi.endpoints", () => {
  describe("list", () => {
    it("should fetch all endpoints", async () => {
      const endpoints = await dwctlApi.endpoints.list();

      expect(endpoints).toBeInstanceOf(Array);
      expect(endpoints.length).toBeGreaterThan(0);
      expect(endpoints[0]).toHaveProperty("id");
      expect(endpoints[0]).toHaveProperty("name");
    });
  });

  describe("get", () => {
    it("should fetch specific endpoint", async () => {
      const endpointId = "a1b2c3d4-e5f6-7890-1234-567890abcdef";
      const endpoint = await dwctlApi.endpoints.get(endpointId);

      expect(endpoint).toHaveProperty(
        "id",
        "a1b2c3d4-e5f6-7890-1234-567890abcdef",
      );
      expect(endpoint).toHaveProperty("name");
    });

    it("should throw error for non-existent endpoint", async () => {
      const nonExistentId = "99999999-9999-9999-9999-999999999999";

      await expect(dwctlApi.endpoints.get(nonExistentId)).rejects.toThrow(
        "Failed to fetch endpoint: 404",
      );
    });
  });
});

describe("dwctlApi.groups", () => {
  describe("list", () => {
    it("should fetch all groups", async () => {
      const response = await dwctlApi.groups.list();

      expect(response).toHaveProperty("data");
      expect(response.data).toBeInstanceOf(Array);
      expect(response.data.length).toBeGreaterThan(0);
      expect(response.data[0]).toHaveProperty("id");
      expect(response.data[0]).toHaveProperty("name");
    });

    it("should include users when requested", async () => {
      const response = await dwctlApi.groups.list({ include: "users" });

      expect(response.data[0]).toHaveProperty("users");
      expect(response.data[0].users).toBeInstanceOf(Array);
    });

    it("should include models when requested", async () => {
      const response = await dwctlApi.groups.list({ include: "models" });

      expect(response.data[0]).toHaveProperty("models");
      expect(response.data[0].models).toBeInstanceOf(Array);
    });

    it("should include both users and models when requested", async () => {
      const response = await dwctlApi.groups.list({ include: "users,models" });

      expect(response.data[0]).toHaveProperty("users");
      expect(response.data[0]).toHaveProperty("models");
    });
  });

  describe("get", () => {
    it("should fetch specific group", async () => {
      const groupId = "550e8400-e29b-41d4-a716-446655441001";
      const group = await dwctlApi.groups.get(groupId);

      expect(group).toHaveProperty("id", groupId);
      expect(group).toHaveProperty("name");
    });

    it("should throw error for non-existent group", async () => {
      const nonExistentId = "non-existent-group";

      await expect(dwctlApi.groups.get(nonExistentId)).rejects.toThrow(
        "Failed to fetch group: 404",
      );
    });
  });

  describe("create", () => {
    it("should create new group", async () => {
      const groupData: GroupCreateRequest = {
        name: "New Group",
        description: "Test group",
      };

      const createdGroup = await dwctlApi.groups.create(groupData);

      expect(createdGroup).toHaveProperty("id");
      expect(createdGroup.name).toBe(groupData.name);
      expect(createdGroup.description).toBe(groupData.description);
      expect(createdGroup).toHaveProperty("created_at");
      expect(createdGroup).toHaveProperty("updated_at");
    });
  });

  describe("update", () => {
    it("should update group", async () => {
      const groupId = "550e8400-e29b-41d4-a716-446655441001";
      const updateData: GroupUpdateRequest = {
        name: "Updated Group",
        description: "Updated description",
      };

      const updatedGroup = await dwctlApi.groups.update(groupId, updateData);

      expect(updatedGroup.name).toBe(updateData.name);
      expect(updatedGroup.description).toBe(updateData.description);
    });
  });

  describe("delete", () => {
    it("should delete group", async () => {
      const groupId = "550e8400-e29b-41d4-a716-446655441001";

      await expect(dwctlApi.groups.delete(groupId)).resolves.toBeUndefined();
    });
  });

  describe("relationship management", () => {
    describe("addUser", () => {
      it("should add user to group", async () => {
        const groupId = "550e8400-e29b-41d4-a716-446655441001";
        const userId = "550e8400-e29b-41d4-a716-446655440001";

        await expect(
          dwctlApi.groups.addUser(groupId, userId),
        ).resolves.toBeUndefined();
      });

      it("should throw error for non-existent group or user", async () => {
        const nonExistentGroupId = "non-existent-group";
        const userId = "550e8400-e29b-41d4-a716-446655440001";

        await expect(
          dwctlApi.groups.addUser(nonExistentGroupId, userId),
        ).rejects.toThrow("Failed to add user to group: 404");
      });
    });

    describe("removeUser", () => {
      it("should remove user from group", async () => {
        const groupId = "550e8400-e29b-41d4-a716-446655441001";
        const userId = "550e8400-e29b-41d4-a716-446655440001";

        await expect(
          dwctlApi.groups.removeUser(groupId, userId),
        ).resolves.toBeUndefined();
      });
    });

    describe("addModel", () => {
      it("should add model to group", async () => {
        const groupId = "550e8400-e29b-41d4-a716-446655441001";
        const modelId = "f914c573-4c00-4a37-a878-53318a6d5a5b";

        await expect(
          dwctlApi.groups.addModel(groupId, modelId),
        ).resolves.toBeUndefined();
      });
    });

    describe("removeModel", () => {
      it("should remove model from group", async () => {
        const groupId = "550e8400-e29b-41d4-a716-446655441001";
        const modelId = "f914c573-4c00-4a37-a878-53318a6d5a5b";

        await expect(
          dwctlApi.groups.removeModel(groupId, modelId),
        ).resolves.toBeUndefined();
      });
    });
  });
});

describe("Error Handling", () => {
  it("should handle HTTP 500 errors", async () => {
    await expect(dwctlApi.users.get("error-500")).rejects.toThrow(
      "Failed to fetch user: 500",
    );
  });

  it("should handle network errors", async () => {
    await expect(dwctlApi.users.get("network-error")).rejects.toThrow();
  });

  it("should throw meaningful error messages", async () => {
    await expect(dwctlApi.users.get("non-existent-id")).rejects.toThrow(
      "Failed to fetch user: 404",
    );
    await expect(dwctlApi.models.get("non-existent-model")).rejects.toThrow(
      "Failed to fetch model: 404",
    );
    await expect(dwctlApi.groups.get("non-existent-group")).rejects.toThrow(
      "Failed to fetch group: 404",
    );
    await expect(dwctlApi.endpoints.get("999")).rejects.toThrow(
      "Failed to fetch endpoint: 404",
    );
  });
});

describe("URL Construction", () => {
  it("should handle empty query parameters correctly", async () => {
    // Test that URLs are constructed correctly when no parameters are provided
    const usersResponse = await dwctlApi.users.list();
    const modelsResponse = await dwctlApi.models.list();
    const groupsResponse = await dwctlApi.groups.list();

    expect(usersResponse).toHaveProperty("data");
    expect(usersResponse.data).toBeInstanceOf(Array);
    expect(modelsResponse).toHaveProperty("data");
    expect(modelsResponse.data).toBeInstanceOf(Array);
    expect(groupsResponse).toHaveProperty("data");
    expect(groupsResponse.data).toBeInstanceOf(Array);
  });

  it("should handle single query parameters", async () => {
    const usersWithGroups = await dwctlApi.users.list({ include: "groups" });
    const modelsFiltered = await dwctlApi.models.list({ endpoint: "2" });

    expect(usersWithGroups.data[0]).toHaveProperty("groups");
    expect(modelsFiltered.data.every((model) => model.hosted_on === "2")).toBe(
      true,
    );
  });

  it("should handle multiple query parameters", async () => {
    const modelsResponse = await dwctlApi.models.list({
      endpoint: "c3d4e5f6-7890-1234-5678-90abcdef0123",
      include: "groups",
    });
    const groupsResponse = await dwctlApi.groups.list({
      include: "users,models",
    });

    expect(
      modelsResponse.data.every(
        (model) => model.hosted_on === "c3d4e5f6-7890-1234-5678-90abcdef0123",
      ),
    ).toBe(true);
    expect(modelsResponse.data[0]).toHaveProperty("groups");

    expect(groupsResponse.data[0]).toHaveProperty("users");
    expect(groupsResponse.data[0]).toHaveProperty("models");
  });
});

describe("Type Safety", () => {
  it("should return correctly typed responses", async () => {
    const user = await dwctlApi.users.get(
      "550e8400-e29b-41d4-a716-446655440001",
    );
    const model = await dwctlApi.models.get(
      "f914c573-4c00-4a37-a878-53318a6d5a5b",
    );
    const group = await dwctlApi.groups.get(
      "550e8400-e29b-41d4-a716-446655441001",
    );
    const endpoint = await dwctlApi.endpoints.get(
      "a1b2c3d4-e5f6-7890-1234-567890abcdef",
    );

    // These should compile without TypeScript errors and have the expected properties
    expect(typeof user.id).toBe("string");
    expect(typeof user.username).toBe("string");
    expect(typeof user.email).toBe("string");
    expect(Array.isArray(user.roles)).toBe(true);

    expect(typeof model.id).toBe("string");
    expect(typeof model.alias).toBe("string");
    expect(typeof model.hosted_on).toBe("string");

    expect(typeof group.id).toBe("string");
    expect(typeof group.name).toBe("string");

    expect(typeof endpoint.id).toBe("string");
    expect(typeof endpoint.name).toBe("string");
  });

  it("should handle optional fields correctly", async () => {
    const usersWithGroups = await dwctlApi.users.list({ include: "groups" });
    const modelsWithGroups = await dwctlApi.models.list({
      include: "groups",
    });

    // Optional fields should be present when requested
    expect(usersWithGroups.data[0].groups).toBeDefined();
    expect(modelsWithGroups.data[0].groups).toBeDefined();
  });
});

describe("dwctlApi.cost", () => {
  describe("listTransactions", () => {
    it("should fetch transactions without query parameters", async () => {
      const transactions = await dwctlApi.cost.listTransactions();

      expect(transactions).toBeInstanceOf(Array);
      expect(transactions.length).toBeGreaterThan(0);
      expect(transactions[0]).toHaveProperty("id");
      expect(transactions[0]).toHaveProperty("user_id");
      expect(transactions[0]).toHaveProperty("transaction_type");
      expect(transactions[0]).toHaveProperty("amount");
      expect(transactions[0]).toHaveProperty("balance_after");
      expect(transactions[0]).toHaveProperty("created_at");
    });

    it("should fetch transactions with userId filter", async () => {
      const userId = "550e8400-e29b-41d4-a716-446655440001";
      const transactions = await dwctlApi.cost.listTransactions({
        userId,
      });

      expect(transactions).toBeInstanceOf(Array);
      expect(transactions.every((t) => t.user_id === userId)).toBe(true);
    });

    it("should fetch transactions with pagination", async () => {
      const transactions = await dwctlApi.cost.listTransactions({
        limit: 5,
        skip: 0,
      });

      expect(transactions).toBeInstanceOf(Array);
      expect(transactions.length).toBeLessThanOrEqual(5);
    });

    it("should return transactions with correct types", async () => {
      const transactions = await dwctlApi.cost.listTransactions();
      const transaction = transactions[0];

      expect(typeof transaction.id).toBe("string");
      expect(typeof transaction.user_id).toBe("string");
      expect(typeof transaction.amount).toBe("number");
      expect(typeof transaction.balance_after).toBe("number");
      expect(typeof transaction.created_at).toBe("string");
      expect(["admin_grant", "admin_removal", "usage", "purchase"]).toContain(
        transaction.transaction_type,
      );
    });
  });

  describe("addFunds", () => {
    it("should add funds to a user account", async () => {
      const userId = "550e8400-e29b-41d4-a716-446655440001";
      const amount = 100.0;

      const result = await dwctlApi.cost.addFunds({
        user_id: userId,
        source_id: `550e8400-e29b-41d4-a716-446655440001_${Date.now()}`,
        amount,
        description: "Test fund addition",
      });

      expect(result).toHaveProperty("id");
      expect(result.user_id).toBe(userId);
      expect(result.amount).toBe(amount);
      expect(result.transaction_type).toBe("admin_grant");
      expect(result.balance_after).toBeGreaterThan(0);
    });

    it("should handle funds addition without description", async () => {
      const userId = "550e8400-e29b-41d4-a716-446655440002";
      const amount = 50.0;

      const result = await dwctlApi.cost.addFunds({
        source_id: `550e8400-e29b-41d4-a716-446655440001_${Date.now()}`,
        user_id: userId,
        amount,
      });

      expect(result).toHaveProperty("id");
      expect(result.user_id).toBe(userId);
      expect(result.amount).toBe(amount);
    });

    it("should throw error for invalid user", async () => {
      await expect(
        dwctlApi.cost.addFunds({
          user_id: "non-existent-user",
          source_id: `550e8400-e29b-41d4-a716-446655440001_${Date.now()}`,
          amount: 100,
        }),
      ).rejects.toThrow();
    });
  });
});

describe("User Billing Integration", () => {
  it("should include billing data when requested", async () => {
    const userId = "550e8400-e29b-41d4-a716-446655440001";
    const user = await dwctlApi.users.get(userId);

    // Billing should be included by default now
    expect(user).toHaveProperty("credit_balance");
    expect(typeof user.credit_balance).toBe("number");
  });

  it("should include billing with groups when both requested", async () => {
    const response = await dwctlApi.users.list({ include: "groups" });

    expect(response.data[0]).toHaveProperty("credit_balance");
    expect(response.data[0]).toHaveProperty("groups");
    expect(Array.isArray(response.data[0].groups)).toBe(true);
  });
});
