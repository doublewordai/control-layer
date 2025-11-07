import { http, HttpResponse } from "msw";
import type {
  UserCreateRequest,
  UserUpdateRequest,
  GroupCreateRequest,
  GroupUpdateRequest,
  ModelUpdateRequest,
  ApiKeyCreateRequest,
  EndpointCreateRequest,
  EndpointUpdateRequest,
  EndpointValidateRequest,
  Model,
  User,
  ApiKey,
  Endpoint,
  Group,
  FileObject,
  Batch,
  BatchRequest,
  FileRequest,
  BatchCreateRequest,
} from "../types";
import usersDataRaw from "./users.json";
import groupsDataRaw from "./groups.json";
import endpointsDataRaw from "./endpoints.json";
import modelsDataRaw from "./models.json";
import apiKeysDataRaw from "./api-keys.json";
import userGroups from "./user-groups.json";
import modelsGroups from "./models-groups.json";
import requestsDataRaw from "../../demo/data/requests.json";
import filesDataRaw from "./files.json";
import batchesDataRaw from "./batches.json";
import batchRequestsDataRaw from "./batch-requests.json";
import fileRequestsDataRaw from "./file-requests.json";
import {
  loadDemoState,
  addModelToGroup as addModelToGroupState,
  removeModelFromGroup as removeModelFromGroupState,
  addUserToGroup as addUserToGroupState,
  removeUserFromGroup as removeUserFromGroupState,
} from "./demoState";

// Type for demo requests
interface DemoRequest {
  id: string;
  timestamp: string;
  model: string;
  response: {
    usage?: {
      prompt_tokens: number;
      completion_tokens: number;
      total_tokens: number;
    };
  };
  duration_ms: number;
  metadata?: {
    email?: string;
    [key: string]: any;
  };
}

// Type assert the imported JSON data
const usersData = usersDataRaw as User[];
const groupsData = groupsDataRaw as Group[];
const endpointsData = endpointsDataRaw as Endpoint[];
const modelsData = modelsDataRaw.data as Model[];
const apiKeysData = apiKeysDataRaw as ApiKey[];
const userGroupsInitial = userGroups as Record<string, string[]>;
const modelsGroupsInitial = modelsGroups as Record<string, string[]>;
const requestsData = requestsDataRaw as DemoRequest[];
const filesData = filesDataRaw as FileObject[];
const batchesData = batchesDataRaw as Batch[];
const batchRequestsData = batchRequestsDataRaw as Record<string, BatchRequest[]>;
const fileRequestsData = fileRequestsDataRaw as Record<string, FileRequest[]>;

// Initialize demo state (loads from localStorage or uses initial data)
let demoState = loadDemoState(modelsGroupsInitial, userGroupsInitial);

// Get current state accessors
const getUserGroupsData = () => demoState.userGroups;
const getModelsGroupsData = () => demoState.modelsGroups;

// Create reverse mapping: group ID -> user IDs (regenerated on each access)
const getGroupUsersData = (): Record<string, string[]> => {
  const groupUsersData: Record<string, string[]> = {};
  const userGroupsData = getUserGroupsData();
  Object.entries(userGroupsData).forEach(([userId, groupIds]) => {
    groupIds.forEach((groupId) => {
      if (!groupUsersData[groupId]) {
        groupUsersData[groupId] = [];
      }
      groupUsersData[groupId].push(userId);
    });
  });
  return groupUsersData;
};

// Function to compute real metrics from requests data, shifted to appear as today's activity
function computeModelMetrics(modelAlias: string) {
  const modelRequests = requestsData.filter((req) => req.model === modelAlias);

  if (modelRequests.length === 0) {
    return {
      total_requests: 0,
      total_input_tokens: 0,
      total_output_tokens: 0,
      avg_latency_ms: 0,
      last_active_at: undefined,
      time_series: [],
    };
  }

  // Calculate totals
  const total_requests = modelRequests.length;
  const total_input_tokens = modelRequests.reduce(
    (sum, req) => sum + (req.response.usage?.prompt_tokens || 0),
    0,
  );
  const total_output_tokens = modelRequests.reduce(
    (sum, req) => sum + (req.response.usage?.completion_tokens || 0),
    0,
  );
  const avg_latency_ms = Math.round(
    modelRequests.reduce((sum, req) => sum + req.duration_ms, 0) /
      total_requests,
  );

  // Shift timestamps to today while preserving relative timing
  const now = new Date();
  const originalLatestDate = new Date(
    Math.max(...modelRequests.map((req) => new Date(req.timestamp).getTime())),
  );
  const timeShift = now.getTime() - originalLatestDate.getTime();

  // Find last active time (shifted to today)
  const shiftedTimestamps = modelRequests.map(
    (req) => new Date(new Date(req.timestamp).getTime() + timeShift),
  );
  const last_active_at = new Date(
    Math.max(...shiftedTimestamps.map((d) => d.getTime())),
  ).toISOString();

  // Create time series (24 hourly buckets) - shift all requests to appear as today's activity
  const timeSeries = [];

  for (let i = 23; i >= 0; i--) {
    const hourStart = new Date(now.getTime() - i * 60 * 60 * 1000);
    hourStart.setMinutes(0, 0, 0);
    const hourEnd = new Date(hourStart.getTime() + 60 * 60 * 1000);

    const requestsInHour = modelRequests.filter((req) => {
      const originalTime = new Date(req.timestamp);
      const shiftedTime = new Date(originalTime.getTime() + timeShift);
      return shiftedTime >= hourStart && shiftedTime < hourEnd;
    }).length;

    timeSeries.push({
      timestamp: hourStart.toISOString(),
      requests: requestsInHour,
    });
  }

  return {
    total_requests,
    total_input_tokens,
    total_output_tokens,
    avg_latency_ms,
    last_active_at,
    time_series: timeSeries,
  };
}

// Function to aggregate requests by user email
function computeUserUsageByModel(
  modelAlias?: string,
  startDate?: string,
  endDate?: string,
) {
  // Filter requests by model first
  let filteredRequests = requestsData;

  if (modelAlias) {
    filteredRequests = filteredRequests.filter(
      (req) => req.model === modelAlias,
    );
  }

  if (filteredRequests.length === 0) {
    return {
      model: modelAlias || "all",
      start_date: startDate || new Date(0).toISOString(),
      end_date: endDate || new Date().toISOString(),
      total_requests: 0,
      total_tokens: 0,
      users: [],
    };
  }

  // Shift timestamps to today while preserving relative timing (same as computeModelMetrics)
  const now = new Date();
  const originalLatestDate = new Date(
    Math.max(
      ...filteredRequests.map((req) => new Date(req.timestamp).getTime()),
    ),
  );
  const timeShift = now.getTime() - originalLatestDate.getTime();

  // Filter by date range using shifted timestamps
  if (startDate || endDate) {
    const start = startDate ? new Date(startDate).getTime() : 0;
    const end = endDate ? new Date(endDate).getTime() : Date.now();

    filteredRequests = filteredRequests.filter((req) => {
      const originalTime = new Date(req.timestamp).getTime();
      const shiftedTime = originalTime + timeShift;
      return shiftedTime >= start && shiftedTime <= end;
    });
  }

  // Group by user email
  const userMap = new Map<
    string,
    {
      user_email?: string;
      request_count: number;
      input_tokens: number;
      output_tokens: number;
      total_tokens: number;
      last_active_at?: string;
    }
  >();

  filteredRequests.forEach((req) => {
    const email = req.metadata?.email || "anonymous";
    const existing = userMap.get(email) || {
      user_email: email !== "anonymous" ? email : undefined,
      request_count: 0,
      input_tokens: 0,
      output_tokens: 0,
      total_tokens: 0,
      last_active_at: undefined,
    };

    existing.request_count += 1;
    existing.input_tokens += req.response.usage?.prompt_tokens || 0;
    existing.output_tokens += req.response.usage?.completion_tokens || 0;
    existing.total_tokens += req.response.usage?.total_tokens || 0;

    // Update last active with shifted timestamp
    const shiftedTimestamp = new Date(
      new Date(req.timestamp).getTime() + timeShift,
    ).toISOString();
    if (
      !existing.last_active_at ||
      shiftedTimestamp > existing.last_active_at
    ) {
      existing.last_active_at = shiftedTimestamp;
    }

    userMap.set(email, existing);
  });

  // Convert to array and calculate totals
  const users = Array.from(userMap.values());
  const total_requests = users.reduce((sum, u) => sum + u.request_count, 0);
  const total_tokens = users.reduce((sum, u) => sum + u.total_tokens, 0);

  return {
    model: modelAlias || "all",
    start_date: startDate || new Date(0).toISOString(),
    end_date: endDate || new Date().toISOString(),
    total_requests,
    total_tokens,
    users,
  };
}

export const handlers = [
  // Error scenarios for testing - must come first to match before generic patterns
  http.get("/admin/api/v1/users/error-500", () => {
    return HttpResponse.json(
      { error: "Internal server error" },
      { status: 500 },
    );
  }),

  http.get("/admin/api/v1/users/network-error", () => {
    return HttpResponse.error();
  }),

  // Users API
  http.get("/admin/api/v1/users", ({ request }) => {
    const url = new URL(request.url);
    const include = url.searchParams.get("include");

    let users = [...usersData];

    if (include === "groups") {
      const userGroupsData = getUserGroupsData();
      users = users.map((user) => ({
        ...user,
        groups: (userGroupsData[user.id] || [])
          .map((id) => groupsData.find((v) => v.id === id))
          .filter((g): g is Group => g !== undefined),
      }));
    }

    return HttpResponse.json(users);
  }),

  http.get("/admin/api/v1/users/:id", ({ params }) => {
    let user;
    if (params.id === "current") {
      // Return the first user as the current user for demo purposes
      user = usersData[0];
    } else {
      user = usersData.find((u) => u.id === params.id);
    }

    if (!user) {
      return HttpResponse.json({ error: "User not found" }, { status: 404 });
    }
    return HttpResponse.json(user);
  }),

  http.post("/admin/api/v1/users", async ({ request }) => {
    const body = (await request.json()) as UserCreateRequest;
    const newUser: User = {
      id: `550e8400-e29b-41d4-a716-${Date.now()}`,
      username: body.username,
      email: body.email,
      display_name: body.display_name,
      avatar_url: body.avatar_url,
      roles: body.roles,
      created_at: new Date().toISOString(),
      updated_at: new Date().toISOString(),
      auth_source: "vouch",
    };
    return HttpResponse.json(newUser, { status: 201 });
  }),

  http.patch("/admin/api/v1/users/:id", async ({ params, request }) => {
    const user = usersData.find((u) => u.id === params.id);
    if (!user) {
      return HttpResponse.json({ error: "User not found" }, { status: 404 });
    }
    const body = (await request.json()) as UserUpdateRequest;
    const updatedUser = {
      ...user,
      ...body,
      updated_at: new Date().toISOString(),
    };
    return HttpResponse.json(updatedUser);
  }),

  http.delete("/admin/api/v1/users/:id", ({ params }) => {
    const user = usersData.find((u) => u.id === params.id);
    if (!user) {
      return HttpResponse.json({ error: "User not found" }, { status: 404 });
    }
    return HttpResponse.json(null, { status: 204 });
  }),

  // API Keys under users
  http.get("/admin/api/v1/users/:userId/api-keys", () => {
    return HttpResponse.json(apiKeysData);
  }),

  http.get("/admin/api/v1/users/:userId/api-keys/:id", ({ params }) => {
    const apiKey = apiKeysData.find((k) => k.id === params.id);
    if (!apiKey) {
      return HttpResponse.json({ error: "API key not found" }, { status: 404 });
    }
    return HttpResponse.json(apiKey);
  }),

  http.post("/admin/api/v1/users/:userId/api-keys", async ({ request }) => {
    const body = (await request.json()) as ApiKeyCreateRequest;
    const newApiKey = {
      id: `key-${Date.now()}`,
      name: body.name,
      description: body.description,
      created_at: new Date().toISOString(),
      key: `sk-${Math.random().toString(36).substring(2, 50)}`,
    };
    return HttpResponse.json(newApiKey, { status: 201 });
  }),

  http.delete("/admin/api/v1/users/:userId/api-keys/:keyId", ({ params }) => {
    const apiKey = apiKeysData.find((k) => k.id === params.keyId);
    if (!apiKey) {
      return HttpResponse.json({ error: "API key not found" }, { status: 404 });
    }
    return HttpResponse.json(null, { status: 204 });
  }),

  // Models API
  http.get("/admin/api/v1/models", ({ request }) => {
    const url = new URL(request.url);
    const endpoint = url.searchParams.get("endpoint");
    const include = url.searchParams.get("include");
    const accessible = url.searchParams.get("accessible");

    let models: Model[] = [...modelsData];

    if (endpoint) {
      models = models.filter((m) => m.hosted_on === endpoint);
    }

    // Filter models by accessibility if requested
    if (accessible === "true") {
      // For now, use the first user as the "current user" for demo purposes
      // In real implementation, this would get the actual current user
      const currentUser = usersData[0]; // Use first user as demo

      if (currentUser) {
        const userGroupsData = getUserGroupsData();
        const modelsGroupsData = getModelsGroupsData();
        // Get user's group IDs
        const userGroupIds = new Set(userGroupsData[currentUser.id] || []);

        // Filter models to only those with shared groups
        models = models.filter((model) => {
          const modelGroupIds = modelsGroupsData[model.id] ?? [];
          return modelGroupIds.some((groupId) => userGroupIds.has(groupId));
        });
      }
    }

    if (include?.includes("groups")) {
      const modelsGroupsData = getModelsGroupsData();
      models = models.map((model) => ({
        ...model,
        groups:
          modelsGroupsData[model.id]
            ?.map((id) => groupsData.find((g) => g.id === id))
            .filter((g): g is Group => g !== undefined) ?? [],
      }));
    }

    if (include?.includes("metrics")) {
      models = models.map((model) => ({
        ...model,
        metrics: computeModelMetrics(model.alias),
      }));
    }

    return HttpResponse.json(models);
  }),

  http.get("/admin/api/v1/models/:id", ({ params }) => {
    const model = modelsData.find((m) => m.id === params.id);
    if (!model) {
      return HttpResponse.json({ error: "Model not found" }, { status: 404 });
    }
    return HttpResponse.json(model);
  }),

  http.patch("/admin/api/v1/models/:id", async ({ params, request }) => {
    const model = modelsData.find((m) => m.id === params.id);
    if (!model) {
      return HttpResponse.json({ error: "Model not found" }, { status: 404 });
    }
    const body = (await request.json()) as ModelUpdateRequest;
    const updatedModel = { ...model, ...body };
    return HttpResponse.json(updatedModel);
  }),

  // Endpoints API
  http.get("/admin/api/v1/endpoints", () => {
    return HttpResponse.json(endpointsData);
  }),

  http.get("/admin/api/v1/endpoints/:id", ({ params }) => {
    const endpoint = endpointsData.find((e) => e.id === params.id);
    if (!endpoint) {
      return HttpResponse.json(
        { error: "Endpoint not found" },
        { status: 404 },
      );
    }
    return HttpResponse.json(endpoint);
  }),

  // Endpoint validation
  http.post("/admin/api/v1/endpoints/validate", async ({ request }) => {
    const body = (await request.json()) as EndpointValidateRequest;

    // Simulate different responses based on URL for testing
    const url = body.type === "new" ? body.url : "existing-endpoint-url";

    if (url === "https://invalid-endpoint.com") {
      return HttpResponse.json({
        status: "error",
        error: "Connection timeout - unable to reach endpoint",
      });
    }

    if (url === "https://unauthorized-endpoint.com") {
      return HttpResponse.json({
        status: "error",
        error: "Authentication failed - invalid API key",
      });
    }

    // Mock successful validation with different model sets
    let mockModels;
    if (url.includes("openai")) {
      mockModels = [
        {
          id: "gpt-4",
          created: 1687882411,
          object: "model" as const,
          owned_by: "openai",
        },
        {
          id: "gpt-3.5-turbo",
          created: 1677610602,
          object: "model" as const,
          owned_by: "openai",
        },
        {
          id: "text-embedding-ada-002",
          created: 1671217299,
          object: "model" as const,
          owned_by: "openai",
        },
      ];
    } else if (url.includes("anthropic")) {
      mockModels = [
        {
          id: "claude-3-opus-20240229",
          created: 1708982400,
          object: "model" as const,
          owned_by: "anthropic",
        },
        {
          id: "claude-3-sonnet-20240229",
          created: 1708982400,
          object: "model" as const,
          owned_by: "anthropic",
        },
      ];
    } else if (url.includes("openrouter")) {
      mockModels = [
        {
          id: "google/gemma-3-4b-it",
          created: 1754651774,
          object: "model" as const,
          owned_by: "google",
        },
        {
          id: "Qwen/Qwen3-Embedding-8B",
          created: 1754651774,
          object: "model" as const,
          owned_by: "alibaba",
        },
        {
          id: "google/gemma-3-12b-it",
          created: 1754651774,
          object: "model" as const,
          owned_by: "google",
        },
        {
          id: "anthropic/claude-3-haiku",
          created: 1708982400,
          object: "model" as const,
          owned_by: "anthropic",
        },
        {
          id: "openai/gpt-4o",
          created: 1715367600,
          object: "model" as const,
          owned_by: "openai",
        },
      ];
    } else if (url.includes("internal-models")) {
      mockModels = [
        {
          id: "google/gemma-3-12b-it",
          created: 1709078400,
          object: "model" as const,
          owned_by: "google",
        },
        {
          id: "Qwen/Qwen3-Embedding-8B",
          created: 1709078400,
          object: "model" as const,
          owned_by: "alibaba",
        },
        {
          id: "meta-llama/Meta-Llama-3.1-8B-Instruct",
          created: 1709078400,
          object: "model" as const,
          owned_by: "meta",
        },
      ];
    } else {
      // Default set for unknown URLs
      mockModels = [
        {
          id: "mock-model-1",
          created: Date.now() / 1000,
          object: "model" as const,
          owned_by: "mock-provider",
        },
        {
          id: "mock-model-2",
          created: Date.now() / 1000,
          object: "model" as const,
          owned_by: "mock-provider",
        },
      ];
    }

    return HttpResponse.json({
      status: "success",
      models: {
        object: "list" as const,
        data: mockModels,
      },
    });
  }),

  // Endpoint creation
  http.post("/admin/api/v1/endpoints", async ({ request }) => {
    const body = (await request.json()) as EndpointCreateRequest;

    const newEndpoint = {
      id: crypto.randomUUID(),
      name: body.name,
      description: body.description,
      url: body.url,
      created_by: "550e8400-e29b-41d4-a716-446655440000",
      created_at: new Date().toISOString(),
      updated_at: new Date().toISOString(),
    };

    return HttpResponse.json(newEndpoint, { status: 201 });
  }),

  // Endpoint update
  http.patch("/admin/api/v1/endpoints/:id", async ({ params, request }) => {
    const endpoint = endpointsData.find((e) => e.id === params.id);
    if (!endpoint) {
      return HttpResponse.json(
        { error: "Endpoint not found" },
        { status: 404 },
      );
    }

    const body = (await request.json()) as EndpointUpdateRequest;
    const updatedEndpoint = {
      ...endpoint,
      ...body,
      updated_at: new Date().toISOString(),
    };

    return HttpResponse.json(updatedEndpoint);
  }),

  // Endpoint deletion
  http.delete("/admin/api/v1/endpoints/:id", ({ params }) => {
    const endpoint = endpointsData.find((e) => e.id === params.id);
    if (!endpoint) {
      return HttpResponse.json(
        { error: "Endpoint not found" },
        { status: 404 },
      );
    }

    return HttpResponse.json(null, { status: 204 });
  }),

  // Endpoint synchronization
  http.post("/admin/api/v1/endpoints/:id/synchronize", ({ params }) => {
    const endpoint = endpointsData.find((e) => e.id === params.id);
    if (!endpoint) {
      return HttpResponse.json(
        { error: "Endpoint not found" },
        { status: 404 },
      );
    }

    // Mock synchronization response
    return HttpResponse.json({
      endpoint_id: endpoint.id,
      changes_made: 3,
      new_models_created: 1,
      models_reactivated: 1,
      models_deactivated: 0,
      models_deleted: 1,
      total_models_fetched: 5,
      filtered_models_count: 5,
      synced_at: new Date().toISOString(),
    });
  }),

  // Groups API
  http.get("/admin/api/v1/groups", ({ request }) => {
    const url = new URL(request.url);
    const include = url.searchParams.get("include");

    let groups: Group[] = [...groupsData];

    if (include?.includes("users")) {
      const groupUsersData = getGroupUsersData();
      groups = groups.map((group) => ({
        ...group,
        users: (groupUsersData[group.id] || [])
          .map((id) => usersData.find((u) => u.id === id))
          .filter((u): u is User => u !== undefined),
      }));
    }

    if (include?.includes("models")) {
      const modelsGroupsData = getModelsGroupsData();
      groups = groups.map((group) => ({
        ...group,
        models: Object.entries(modelsGroupsData)
          .filter(([_, groupIds]) => groupIds.includes(group.id))
          .map(([modelId, _]) => modelsData.find((m) => m.id === modelId))
          .filter((model): model is Model => model !== undefined),
      }));
    }

    return HttpResponse.json(groups);
  }),

  http.get("/admin/api/v1/groups/:id", ({ params }) => {
    const group = groupsData.find((g) => g.id === params.id);
    if (!group) {
      return HttpResponse.json({ error: "Group not found" }, { status: 404 });
    }
    return HttpResponse.json(group);
  }),

  http.post("/admin/api/v1/groups", async ({ request }) => {
    const body = (await request.json()) as GroupCreateRequest;
    const newGroup = {
      id: `550e8400-e29b-41d4-a716-${Date.now()}`,
      name: body.name,
      description: body.description,
      created_by: "550e8400-e29b-41d4-a716-446655440000",
      created_at: new Date().toISOString(),
      updated_at: new Date().toISOString(),
      source: "native",
    };
    return HttpResponse.json(newGroup, { status: 201 });
  }),

  http.patch("/admin/api/v1/groups/:id", async ({ params, request }) => {
    const group = groupsData.find((g) => g.id === params.id);
    if (!group) {
      return HttpResponse.json({ error: "Group not found" }, { status: 404 });
    }
    const body = (await request.json()) as GroupUpdateRequest;
    const updatedGroup = {
      ...group,
      ...body,
      updated_at: new Date().toISOString(),
    };
    return HttpResponse.json(updatedGroup);
  }),

  http.delete("/admin/api/v1/groups/:id", ({ params }) => {
    const group = groupsData.find((g) => g.id === params.id);
    if (!group) {
      return HttpResponse.json({ error: "Group not found" }, { status: 404 });
    }
    return HttpResponse.json(null, { status: 204 });
  }),

  // Group relationship management
  http.post("/admin/api/v1/groups/:groupId/users/:userId", ({ params }) => {
    const group = groupsData.find((g) => g.id === params.groupId);
    const user = usersData.find((u) => u.id === params.userId);
    if (!group || !user) {
      return HttpResponse.json(
        { error: "Group or user not found" },
        { status: 404 },
      );
    }
    // Update state and persist to localStorage
    demoState = addUserToGroupState(
      demoState,
      params.userId as string,
      params.groupId as string,
    );
    return HttpResponse.json(null, { status: 204 });
  }),

  http.delete("/admin/api/v1/groups/:groupId/users/:userId", ({ params }) => {
    const group = groupsData.find((g) => g.id === params.groupId);
    const user = usersData.find((u) => u.id === params.userId);
    if (!group || !user) {
      return HttpResponse.json(
        { error: "Group or user not found" },
        { status: 404 },
      );
    }
    // Update state and persist to localStorage
    demoState = removeUserFromGroupState(
      demoState,
      params.userId as string,
      params.groupId as string,
    );
    return HttpResponse.json(null, { status: 204 });
  }),

  http.post("/admin/api/v1/groups/:groupId/models/:modelId", ({ params }) => {
    const group = groupsData.find((g) => g.id === params.groupId);
    const model = modelsData.find((m) => m.id === params.modelId);
    if (!group || !model) {
      return HttpResponse.json(
        { error: "Group or model not found" },
        { status: 404 },
      );
    }
    // Update state and persist to localStorage
    demoState = addModelToGroupState(
      demoState,
      params.modelId as string,
      params.groupId as string,
    );
    return HttpResponse.json(null, { status: 204 });
  }),

  http.delete("/admin/api/v1/groups/:groupId/models/:modelId", ({ params }) => {
    const group = groupsData.find((g) => g.id === params.groupId);
    const model = modelsData.find((m) => m.id === params.modelId);
    if (!group || !model) {
      return HttpResponse.json(
        { error: "Group or model not found" },
        { status: 404 },
      );
    }
    // Update state and persist to localStorage
    demoState = removeModelFromGroupState(
      demoState,
      params.modelId as string,
      params.groupId as string,
    );
    return HttpResponse.json(null, { status: 204 });
  }),

  // Config API
  http.get("/admin/api/v1/config", () => {
    return HttpResponse.json({
      region: "UK South",
      organization: "ACME Corp",
    });
  }),

  // AI Endpoints for Playground
  // Chat completions
  http.post("/admin/api/v1/ai/v1/chat/completions", async ({ request }) => {
    const body = await request.json();
    const messages = (body as any).messages || [];
    const stream = (body as any).stream;
    const model = (body as any).model || "mock-model";

    // Get the last user message
    const lastUserMessage = messages
      .filter((m: any) => m.role === "user")
      .pop();
    const userContent = lastUserMessage?.content || "Hello";

    // Read custom response from settings
    const storedSettings = localStorage.getItem("app-settings");
    let responseTemplate =
      'This is a demo response in demo mode. You asked: "{userMessage}"';

    if (storedSettings) {
      try {
        const settings = JSON.parse(storedSettings);
        if (settings.demoConfig?.customResponse) {
          responseTemplate = settings.demoConfig.customResponse;
        }
      } catch (e) {
        console.error("Failed to parse settings:", e);
      }
    }

    // Replace {userMessage} placeholder with actual user content
    const responseContent = responseTemplate.replace(
      /{userMessage}/g,
      userContent,
    );

    if (stream) {
      // Return a streaming response
      const encoder = new TextEncoder();
      // Split response into chunks for streaming (roughly 10-20 chars per chunk)
      const chunkSize = Math.max(10, Math.floor(responseContent.length / 5));
      const chunks: string[] = [];
      for (let i = 0; i < responseContent.length; i += chunkSize) {
        chunks.push(responseContent.substring(i, i + chunkSize));
      }

      const stream = new ReadableStream({
        start(controller) {
          let chunkIndex = 0;

          const sendChunk = () => {
            if (chunkIndex < chunks.length) {
              const chunk = {
                id: `chatcmpl-${Date.now()}`,
                object: "chat.completion.chunk",
                created: Math.floor(Date.now() / 1000),
                model: model,
                choices: [
                  {
                    index: 0,
                    delta:
                      chunkIndex === 0
                        ? { role: "assistant", content: chunks[chunkIndex] }
                        : { content: chunks[chunkIndex] },
                    finish_reason: null,
                  },
                ],
              };
              controller.enqueue(
                encoder.encode(`data: ${JSON.stringify(chunk)}\n\n`),
              );
              chunkIndex++;
              setTimeout(sendChunk, 100);
            } else {
              // Send final chunk with usage
              const finalChunk = {
                id: `chatcmpl-${Date.now()}`,
                object: "chat.completion.chunk",
                created: Math.floor(Date.now() / 1000),
                model: model,
                choices: [
                  {
                    index: 0,
                    delta: {},
                    finish_reason: "stop",
                  },
                ],
                usage: {
                  prompt_tokens: 20,
                  completion_tokens: 15,
                  total_tokens: 35,
                },
              };
              controller.enqueue(
                encoder.encode(`data: ${JSON.stringify(finalChunk)}\n\n`),
              );
              controller.enqueue(encoder.encode("data: [DONE]\n\n"));
              controller.close();
            }
          };

          sendChunk();
        },
      });

      return new HttpResponse(stream, {
        headers: {
          "Content-Type": "text/event-stream",
          "Cache-Control": "no-cache",
          Connection: "keep-alive",
        },
      });
    } else {
      // Return a regular response
      return HttpResponse.json({
        id: `chatcmpl-${Date.now()}`,
        object: "chat.completion",
        created: Math.floor(Date.now() / 1000),
        model: model,
        choices: [
          {
            index: 0,
            message: {
              role: "assistant",
              content: responseContent,
            },
            finish_reason: "stop",
          },
        ],
        usage: {
          prompt_tokens: 20,
          completion_tokens: 15,
          total_tokens: 35,
        },
      });
    }
  }),

  // Embeddings
  http.post("/admin/api/v1/ai/v1/embeddings", async ({ request }) => {
    const body = await request.json();
    const input = (body as any).input;
    const model = (body as any).model || "mock-embedding-model";
    const encodingFormat = (body as any).encoding_format || "float";

    // Generate a mock embedding vector (1536 dimensions for OpenAI compatibility)
    const generateEmbedding = (text: string) => {
      const embedding = [];
      for (let i = 0; i < 1536; i++) {
        // Use text length and position to create deterministic but varied values
        embedding.push(
          Math.sin(i * 0.01 + text.length * 0.1) * 0.1 +
            Math.cos(i * 0.02) * 0.05,
        );
      }

      // Handle base64 encoding if requested
      if (encodingFormat === "base64") {
        // Convert float array to base64
        const buffer = new Float32Array(embedding).buffer;
        const bytes = new Uint8Array(buffer);
        let binary = "";
        for (let i = 0; i < bytes.length; i++) {
          binary += String.fromCharCode(bytes[i]);
        }
        return btoa(binary);
      }

      return embedding;
    };

    const inputs = Array.isArray(input) ? input : [input];
    const embeddings = inputs.map((text, index) => ({
      object: "embedding",
      index: index,
      embedding: generateEmbedding(text),
    }));

    return HttpResponse.json(
      {
        object: "list",
        data: embeddings,
        model: model,
        usage: {
          prompt_tokens: inputs.reduce(
            (sum, text) => sum + Math.ceil(text.length / 4),
            0,
          ),
          total_tokens: inputs.reduce(
            (sum, text) => sum + Math.ceil(text.length / 4),
            0,
          ),
        },
      } as any,
      {
        headers: {
          "Content-Type": "application/json",
          "Access-Control-Allow-Origin": "*",
          "Access-Control-Allow-Methods": "POST, OPTIONS",
          "Access-Control-Allow-Headers": "Content-Type, Authorization",
        },
      },
    );
  }),

  // Rerank
  http.post("/admin/api/v1/ai/rerank", async ({ request }) => {
    const body = await request.json();
    const query = (body as any).query;
    const documents = (body as any).documents || [];
    const model = (body as any).model || "mock-rerank-model";

    // Simple relevance scoring based on word overlap
    const scoreDocument = (doc: string, query: string) => {
      const docWords = new Set(doc.toLowerCase().split(/\s+/).filter(Boolean));
      const queryWords = query.toLowerCase().split(/\s+/).filter(Boolean);
      const matches = queryWords.filter((word) => docWords.has(word)).length;
      return matches / queryWords.length;
    };

    const results = documents
      .map((doc: string, index: number) => ({
        index: index,
        document: doc,
        relevance_score: scoreDocument(doc, query),
      }))
      .sort(
        (a: { relevance_score: number }, b: { relevance_score: number }) =>
          b.relevance_score - a.relevance_score,
      );

    return HttpResponse.json({
      id: `rerank-${Date.now()}`,
      results: results,
      model: model,
      usage: {
        total_tokens: Math.ceil(
          (query.length + documents.join(" ").length) / 4,
        ),
      },
    });
  }),

  // Requests aggregate by user
  http.get("/admin/api/v1/requests/aggregate-by-user", ({ request }) => {
    const url = new URL(request.url);
    const model = url.searchParams.get("model") || undefined;
    const startDate = url.searchParams.get("start_date") || undefined;
    const endDate = url.searchParams.get("end_date") || undefined;

    const result = computeUserUsageByModel(model, startDate, endDate);
    return HttpResponse.json(result);
  }),

  // Probes API
  http.get("/admin/api/v1/probes", () => {
    return HttpResponse.json([]);
  }),

  // ===== FILES API =====

  http.get("/admin/api/v1/files", ({ request }) => {
    const url = new URL(request.url);
    const after = url.searchParams.get("after");
    const limit = parseInt(url.searchParams.get("limit") || "10000");
    const order = url.searchParams.get("order") || "desc";
    const purpose = url.searchParams.get("purpose");

    let files = [...filesData];

    // Filter by purpose
    if (purpose) {
      files = files.filter((f) => f.purpose === purpose);
    }

    // Sort by created_at
    files.sort((a, b) => {
      const diff = b.created_at - a.created_at;
      return order === "asc" ? -diff : diff;
    });

    // Pagination with 'after' cursor
    if (after) {
      const afterIndex = files.findIndex((f) => f.id === after);
      if (afterIndex !== -1) {
        files = files.slice(afterIndex + 1);
      }
    }

    const hasMore = files.length > limit;
    const returnFiles = files.slice(0, limit);

    return HttpResponse.json({
      object: "list",
      data: returnFiles,
      first_id: returnFiles[0]?.id,
      last_id: returnFiles[returnFiles.length - 1]?.id,
      has_more: hasMore,
    });
  }),

  http.get("/admin/api/v1/files/:id", ({ params }) => {
    const file = filesData.find((f) => f.id === params.id);
    if (!file) {
      return HttpResponse.json({ error: "File not found" }, { status: 404 });
    }
    return HttpResponse.json(file);
  }),

  http.post("/admin/api/v1/files", async ({ request }) => {
    const formData = await request.formData();
    const file = formData.get("file") as File;
    const purpose = formData.get("purpose") as string;
    const expiresAfterSeconds = formData.get("expires_after[seconds]");

    if (!file || !purpose) {
      return HttpResponse.json(
        { error: "Missing required fields: file and purpose" },
        { status: 400 }
      );
    }

    const now = Math.floor(Date.now() / 1000);
    let expiresAt: number | undefined;

    if (expiresAfterSeconds) {
      const seconds = parseInt(expiresAfterSeconds as string);
      expiresAt = now + seconds;
    }

    const newFile: FileObject = {
      id: `file-${Date.now()}`,
      object: "file",
      bytes: file.size,
      created_at: now,
      expires_at: expiresAt,
      filename: file.name,
      purpose: purpose as any,
    };

    return HttpResponse.json(newFile, { status: 201 });
  }),

  http.delete("/admin/api/v1/files/:id", ({ params }) => {
    const file = filesData.find((f) => f.id === params.id);
    if (!file) {
      return HttpResponse.json({ error: "File not found" }, { status: 404 });
    }

    return HttpResponse.json({
      id: file.id,
      object: "file",
      deleted: true,
    });
  }),

  // Custom endpoint: Get requests in a file
  http.get("/admin/api/v1/files/:id/requests", ({ request, params }) => {
    const url = new URL(request.url);
    const limit = parseInt(url.searchParams.get("limit") || "100");
    const skip = parseInt(url.searchParams.get("skip") || "0");

    const fileId = params.id as string;
    const requests = fileRequestsData[fileId] || [];

    const paginatedRequests = requests.slice(skip, skip + limit);
    const hasMore = skip + limit < requests.length;

    return HttpResponse.json({
      object: "list",
      data: paginatedRequests,
      has_more: hasMore,
      total: requests.length,
    });
  }),

  // ===== BATCHES API =====

  http.get("/admin/api/v1/batches", ({ request }) => {
    const url = new URL(request.url);
    const after = url.searchParams.get("after");
    const limit = parseInt(url.searchParams.get("limit") || "20");

    let batches = [...batchesData];

    // Sort by created_at desc
    batches.sort((a, b) => b.created_at - a.created_at);

    // Pagination with 'after' cursor
    if (after) {
      const afterIndex = batches.findIndex((b) => b.id === after);
      if (afterIndex !== -1) {
        batches = batches.slice(afterIndex + 1);
      }
    }

    const hasMore = batches.length > limit;
    const returnBatches = batches.slice(0, limit);

    return HttpResponse.json({
      object: "list",
      data: returnBatches,
      first_id: returnBatches[0]?.id,
      last_id: returnBatches[returnBatches.length - 1]?.id,
      has_more: hasMore,
    });
  }),

  http.get("/admin/api/v1/batches/:id", ({ params }) => {
    const batch = batchesData.find((b) => b.id === params.id);
    if (!batch) {
      return HttpResponse.json({ error: "Batch not found" }, { status: 404 });
    }
    return HttpResponse.json(batch);
  }),

  http.post("/admin/api/v1/batches", async ({ request }) => {
    const body = (await request.json()) as BatchCreateRequest;

    const now = Math.floor(Date.now() / 1000);
    const newBatch: Batch = {
      id: `batch-${Date.now()}`,
      object: "batch",
      endpoint: body.endpoint,
      errors: null,
      input_file_id: body.input_file_id,
      completion_window: body.completion_window,
      status: "validating",
      output_file_id: null,
      error_file_id: null,
      created_at: now,
      in_progress_at: null,
      expires_at: now + 86400, // 24 hours
      finalizing_at: null,
      completed_at: null,
      failed_at: null,
      expired_at: null,
      cancelling_at: null,
      cancelled_at: null,
      request_counts: {
        total: 0,
        completed: 0,
        failed: 0,
      },
      metadata: body.metadata,
    };

    return HttpResponse.json(newBatch, { status: 201 });
  }),

  http.post("/admin/api/v1/batches/:id/cancel", ({ params }) => {
    const batch = batchesData.find((b) => b.id === params.id);
    if (!batch) {
      return HttpResponse.json({ error: "Batch not found" }, { status: 404 });
    }

    const cancelledBatch: Batch = {
      ...batch,
      status: "cancelling",
      cancelling_at: Math.floor(Date.now() / 1000),
    };

    return HttpResponse.json(cancelledBatch);
  }),

  // Custom endpoint: Get requests in a batch
  http.get("/admin/api/v1/batches/:id/requests", ({ request, params }) => {
    const url = new URL(request.url);
    const limit = parseInt(url.searchParams.get("limit") || "100");
    const skip = parseInt(url.searchParams.get("skip") || "0");
    const status = url.searchParams.get("status");

    const batchId = params.id as string;
    let requests = batchRequestsData[batchId] || [];

    // Filter by status if provided
    if (status) {
      requests = requests.filter((r) => r.status === status);
    }

    const paginatedRequests = requests.slice(skip, skip + limit);
    const hasMore = skip + limit < requests.length;

    return HttpResponse.json({
      object: "list",
      data: paginatedRequests,
      has_more: hasMore,
      total: requests.length,
    });
  }),

  // Custom endpoint: Download batch results
  http.get("/admin/api/v1/batches/:id/results/download", ({ params }) => {
    const batchId = params.id as string;
    const requests = batchRequestsData[batchId] || [];

    // Generate JSONL output
    const jsonl = requests
      .filter((r) => r.status === "completed")
      .map((r) => JSON.stringify({
        id: r.id,
        custom_id: r.custom_id,
        response: r.response,
        error: null,
      }))
      .join("\n");

    return HttpResponse.text(jsonl, {
      headers: {
        "Content-Type": "application/jsonl",
        "Content-Disposition": `attachment; filename="batch-${batchId}-results.jsonl"`,
      },
    });
  }),
];
