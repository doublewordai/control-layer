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
      const users = await dwctlApi.users.list();

      expect(users).toBeInstanceOf(Array);
      expect(users.length).toBeGreaterThan(0);
      expect(users[0]).toHaveProperty("id");
      expect(users[0]).toHaveProperty("username");
      expect(users[0]).toHaveProperty("email");
      expect(users[0]).toHaveProperty("roles");
    });

    it("should fetch users with include=groups parameter", async () => {
      const users = await dwctlApi.users.list({ include: "groups" });

      expect(users).toBeInstanceOf(Array);
      expect(users[0]).toHaveProperty("groups");
      expect(users[0].groups).toBeInstanceOf(Array);
    });

    it("should construct URL correctly with query parameters", async () => {
      // Test that the URL is constructed properly by checking the response
      const users = await dwctlApi.users.list({ include: "groups" });

      // The handler should return users with groups when include=groups
      expect(users[0]).toHaveProperty("groups");
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
        roles: ["User"],
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
        roles: ["Admin", "User"],
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
        roles: ["Admin"],
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
        const apiKeys = await dwctlApi.users.apiKeys.getAll();

        expect(apiKeys).toBeInstanceOf(Array);
        expect(apiKeys.length).toBeGreaterThan(0);
        expect(apiKeys[0]).toHaveProperty("id");
        expect(apiKeys[0]).toHaveProperty("name");
        expect(apiKeys[0]).toHaveProperty("created_at");
      });

      it("should fetch API keys for specific user", async () => {
        const userId = "550e8400-e29b-41d4-a716-446655440001";
        const apiKeys = await dwctlApi.users.apiKeys.getAll(userId);

        expect(apiKeys).toBeInstanceOf(Array);
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
      const models = await dwctlApi.models.list();

      expect(models).toBeInstanceOf(Object);
      expect(Object.keys(models).length).toBeGreaterThan(0);

      const firstModel = Object.values(models)[0];
      expect(firstModel).toHaveProperty("id");
      expect(firstModel).toHaveProperty("alias");
      expect(firstModel).toHaveProperty("model_name");
      expect(firstModel).toHaveProperty("hosted_on");
    });

    it("should filter models by endpoint", async () => {
      const models = await dwctlApi.models.list({ endpoint: "2" });

      expect(models).toBeInstanceOf(Object);
      const modelValues = Object.values(models);
      expect(modelValues.every((model) => model.hosted_on === 2)).toBe(true);
    });

    it("should include groups when requested", async () => {
      const models = await dwctlApi.models.list({ include: "groups" });

      const firstModel = Object.values(models)[0];
      expect(firstModel).toHaveProperty("groups");
      expect(firstModel.groups).toBeInstanceOf(Array);
    });

    it("should construct URL correctly with multiple parameters", async () => {
      const models = await dwctlApi.models.list({
        endpoint: "c3d4e5f6-7890-1234-5678-90abcdef0123",
        include: "groups",
      });

      const modelValues = Object.values(models);
      expect(
        modelValues.every(
          (model) => model.hosted_on === "c3d4e5f6-7890-1234-5678-90abcdef0123",
        ),
      ).toBe(true);
      expect(modelValues[0]).toHaveProperty("groups");
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
      const groups = await dwctlApi.groups.list();

      expect(groups).toBeInstanceOf(Array);
      expect(groups.length).toBeGreaterThan(0);
      expect(groups[0]).toHaveProperty("id");
      expect(groups[0]).toHaveProperty("name");
    });

    it("should include users when requested", async () => {
      const groups = await dwctlApi.groups.list({ include: "users" });

      expect(groups[0]).toHaveProperty("users");
      expect(groups[0].users).toBeInstanceOf(Array);
    });

    it("should include models when requested", async () => {
      const groups = await dwctlApi.groups.list({ include: "models" });

      expect(groups[0]).toHaveProperty("models");
      expect(groups[0].models).toBeInstanceOf(Array);
    });

    it("should include both users and models when requested", async () => {
      const groups = await dwctlApi.groups.list({ include: "users,models" });

      expect(groups[0]).toHaveProperty("users");
      expect(groups[0]).toHaveProperty("models");
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
    const users = await dwctlApi.users.list();
    const models = await dwctlApi.models.list();
    const groups = await dwctlApi.groups.list();

    expect(users).toBeInstanceOf(Array);
    expect(models).toBeInstanceOf(Object);
    expect(groups).toBeInstanceOf(Array);
  });

  it("should handle single query parameters", async () => {
    const usersWithGroups = await dwctlApi.users.list({ include: "groups" });
    const modelsFiltered = await dwctlApi.models.list({ endpoint: "2" });

    expect(usersWithGroups[0]).toHaveProperty("groups");
    expect(
      Object.values(modelsFiltered).every((model) => model.hosted_on === 2),
    ).toBe(true);
  });

  it("should handle multiple query parameters", async () => {
    const models = await dwctlApi.models.list({
      endpoint: "c3d4e5f6-7890-1234-5678-90abcdef0123",
      include: "groups",
    });
    const groups = await dwctlApi.groups.list({ include: "users,models" });

    const modelValues = Object.values(models);
    expect(
      modelValues.every(
        (model) => model.hosted_on === "c3d4e5f6-7890-1234-5678-90abcdef0123",
      ),
    ).toBe(true);
    expect(modelValues[0]).toHaveProperty("groups");

    expect(groups[0]).toHaveProperty("users");
    expect(groups[0]).toHaveProperty("models");
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
    expect(usersWithGroups[0].groups).toBeDefined();
    expect(Object.values(modelsWithGroups)[0].groups).toBeDefined();
  });
});
