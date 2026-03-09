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
  ModelTariff,
  FileObject,
  Batch,
  BatchRequest,
  FileRequest,
  BatchCreateRequest,
  Transaction,
  AddFundsRequest,
  Role,
  ModelType,
} from "../types";
import usersDataRaw from "./users.json";
import groupsDataRaw from "./groups.json";
import endpointsDataRaw from "./endpoints.json";
import modelsDataRaw from "./models.json";
import apiKeysDataRaw from "./api-keys.json";
// Mock transactions for MSW test handlers
// For demo mode UI data, see: src/components/features/cost-management/demoTransactions.ts
import transactionsDataRaw from "./transactions.json";
import userGroups from "./user-groups.json";
import modelsGroups from "./models-groups.json";
import requestsDataRaw from "../../demo/data/requests.json";
import filesDataRaw from "./files.json";
import batchesDataRaw from "./batches.json";
import batchRequestsDataRaw from "./batch-requests.json";
import fileRequestsDataRaw from "./file-requests.json";
import organizationsDataRaw from "./organizations.json";
import type {
  Organization,
  OrganizationMember,
  OrganizationCreateRequest,
  OrganizationUpdateRequest,
  InviteMemberRequest,
  InviteDetailsResponse,
} from "../types";
import {
  loadDemoState,
  addModelToGroup as addModelToGroupState,
  removeModelFromGroup as removeModelFromGroupState,
  addUserToGroup as addUserToGroupState,
  removeUserFromGroup as removeUserFromGroupState,
  setCurrentUserRoles,
  getCurrentUserRoles,
  getModelComponents as getModelComponentsState,
  addModelComponent as addModelComponentState,
  updateModelComponent as updateModelComponentState,
  removeModelComponent as removeModelComponentState,
  type StoredComponent,
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
    created?: number;
    [key: string]: unknown;
  };
  duration_ms: number;
  metadata?: {
    email?: string;
    [key: string]: any;
  };
}

// Type assert the imported JSON data
const usersData = usersDataRaw as unknown as User[];
const groupsData = groupsDataRaw as Group[];
const endpointsData = endpointsDataRaw as Endpoint[];
const modelsData = modelsDataRaw.data as Model[];
const apiKeysData = apiKeysDataRaw as ApiKey[];
const transactionsData = transactionsDataRaw as Transaction[];
const organizationsData = organizationsDataRaw as unknown as Organization[];

// Mock organization members
const orgMembersData: Record<string, OrganizationMember[]> = {
  "org-550e8400-0001": [
    {
      id: "mem-001",
      user: usersData[0],
      role: "owner",
      status: "active",
      created_at: "2025-01-15T10:00:00Z",
    },
    {
      id: "mem-002",
      user: usersData[1],
      role: "member",
      status: "active",
      created_at: "2025-02-01T10:00:00Z",
    },
    {
      id: "mem-003",
      role: "member",
      status: "pending",
      created_at: "2025-06-01T10:00:00Z",
      invite_email: "newuser@acme.com",
    },
  ],
  "org-550e8400-0002": [
    {
      id: "mem-004",
      user: usersData[2],
      role: "owner",
      status: "active",
      created_at: "2025-03-20T14:00:00Z",
    },
  ],
};
const userGroupsInitial = userGroups as Record<string, string[]>;
const modelsGroupsInitial = modelsGroups as Record<string, string[]>;
const requestsData = requestsDataRaw as DemoRequest[];
const filesData = filesDataRaw as FileObject[];
const batchesData = batchesDataRaw as Batch[];
const batchRequestsData = batchRequestsDataRaw as Record<
  string,
  BatchRequest[]
>;
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

// Initial component relationships: virtual model ID -> hosted model components
const initialModelComponents: Record<string, StoredComponent[]> = {
  // Qwen3.5-397B → 2 hosted models (primary + burst)
  "d1a2b3c4-e5f6-4a7b-8c9d-0e1f2a3b4c5d": [
    { componentModelId: "h1000001-aaaa-4bbb-8ccc-ddddeeee0001", weight: 80, enabled: true, sort_order: 0 },
    { componentModelId: "h1000001-aaaa-4bbb-8ccc-ddddeeee0002", weight: 20, enabled: true, sort_order: 1 },
  ],
  // Qwen3.5-35B → 2 hosted models
  "d2a3b4c5-e6f7-4a8b-9c0d-1e2f3a4b5c6d": [
    { componentModelId: "h1000001-aaaa-4bbb-8ccc-ddddeeee0003", weight: 70, enabled: true, sort_order: 0 },
    { componentModelId: "h1000001-aaaa-4bbb-8ccc-ddddeeee0004", weight: 30, enabled: true, sort_order: 1 },
  ],
  // gpt-oss-20b → 1 hosted model (priority mode)
  "d3a4b5c6-e7f8-4a9b-0c1d-2e3f4a5b6c7d": [
    { componentModelId: "h1000001-aaaa-4bbb-8ccc-ddddeeee0005", weight: 1, enabled: true, sort_order: 0 },
  ],
  // Qwen3-VL-235B → 1 hosted model
  "d4a5b6c7-e8f9-4a0b-1c2d-3e4f5a6b7c8d": [
    { componentModelId: "h1000001-aaaa-4bbb-8ccc-ddddeeee0006", weight: 1, enabled: true, sort_order: 0 },
  ],
  // Qwen3-VL-30B → 2 hosted models
  "d5a6b7c8-e9f0-4a1b-2c3d-4e5f6a7b8c9d": [
    { componentModelId: "h1000001-aaaa-4bbb-8ccc-ddddeeee0007", weight: 60, enabled: true, sort_order: 0 },
    { componentModelId: "h1000001-aaaa-4bbb-8ccc-ddddeeee0008", weight: 40, enabled: true, sort_order: 1 },
  ],
  // Qwen3-14B → 2 hosted models
  "d6a7b8c9-e0f1-4a2b-3c4d-5e6f7a8b9c0d": [
    { componentModelId: "h1000001-aaaa-4bbb-8ccc-ddddeeee0009", weight: 70, enabled: true, sort_order: 0 },
    { componentModelId: "h1000001-aaaa-4bbb-8ccc-ddddeeee0010", weight: 30, enabled: true, sort_order: 1 },
  ],
  // Qwen3-Embedding-8B → 1 hosted model
  "d7a8b9c0-e1f2-4a3b-4c5d-6e7f8a9b0c1d": [
    { componentModelId: "h1000001-aaaa-4bbb-8ccc-ddddeeee0011", weight: 1, enabled: true, sort_order: 0 },
  ],
  // Qwen3.5-9B → 2 hosted models (primary + burst)
  "d8b9c0d1-e2f3-4a4b-5c6d-7e8f9a0b1c2d": [
    { componentModelId: "h1000001-aaaa-4bbb-8ccc-ddddeeee0012", weight: 70, enabled: true, sort_order: 0 },
    { componentModelId: "h1000001-aaaa-4bbb-8ccc-ddddeeee0013", weight: 30, enabled: true, sort_order: 1 },
  ],
};

// Resolve stored components to full ModelComponent objects for API responses
function resolveComponents(modelId: string): import("../types").ModelComponent[] {
  const stored = getModelComponentsState(demoState, modelId, initialModelComponents);
  return stored.map((sc) => {
    const componentModel = modelsData.find((m) => m.id === sc.componentModelId);
    const endpoint = componentModel?.hosted_on
      ? endpointsData.find((e) => e.id === componentModel.hosted_on)
      : undefined;
    return {
      weight: sc.weight,
      enabled: sc.enabled,
      sort_order: sc.sort_order,
      created_at: "2025-09-15T00:00:00Z",
      model: {
        id: componentModel?.id ?? sc.componentModelId,
        alias: componentModel?.alias ?? "unknown",
        model_name: componentModel?.model_name ?? "unknown",
        description: componentModel?.description ?? undefined,
        model_type: componentModel?.model_type ?? undefined,
        endpoint: endpoint ? { id: endpoint.id, name: endpoint.name } : undefined,
        trusted: componentModel?.trusted,
        open_responses_adapter: componentModel?.open_responses_adapter,
      },
    };
  });
}

// Model tariff data - maps model ID to tariffs
const modelTariffs: Record<string, ModelTariff[]> = {
  // Qwen3.5-397B-A17B-FP8 — Realtime $0.60/$3.60 per M
  "d1a2b3c4-e5f6-4a7b-8c9d-0e1f2a3b4c5d": [
    {
      id: "tariff-001",
      deployed_model_id: "d1a2b3c4-e5f6-4a7b-8c9d-0e1f2a3b4c5d",
      name: "Realtime",
      input_price_per_token: "0.0000006",
      output_price_per_token: "0.0000036",
      valid_from: "2025-09-15T00:00:00Z",
      valid_until: null,
      api_key_purpose: "realtime" as const,
      completion_window: null,
      is_active: true,
    },
    {
      id: "tariff-002",
      deployed_model_id: "d1a2b3c4-e5f6-4a7b-8c9d-0e1f2a3b4c5d",
      name: "Batch (24h)",
      input_price_per_token: "0.0000003",
      output_price_per_token: "0.0000018",
      valid_from: "2025-09-15T00:00:00Z",
      valid_until: null,
      api_key_purpose: "batch" as const,
      completion_window: "24h",
      is_active: true,
    },
    {
      id: "tariff-003",
      deployed_model_id: "d1a2b3c4-e5f6-4a7b-8c9d-0e1f2a3b4c5d",
      name: "Batch (1h)",
      input_price_per_token: "0.00000045",
      output_price_per_token: "0.0000027",
      valid_from: "2025-09-15T00:00:00Z",
      valid_until: null,
      api_key_purpose: "batch" as const,
      completion_window: "1h",
      is_active: true,
    },
    {
      id: "tariff-004",
      deployed_model_id: "d1a2b3c4-e5f6-4a7b-8c9d-0e1f2a3b4c5d",
      name: "Playground",
      input_price_per_token: "0.0000006",
      output_price_per_token: "0.0000036",
      valid_from: "2025-09-15T00:00:00Z",
      valid_until: null,
      api_key_purpose: "playground" as const,
      completion_window: null,
      is_active: true,
    },
  ],
  // Qwen3.5-35B-A3B-FP8 — Realtime $0.25/$2.00 per M
  "d2a3b4c5-e6f7-4a8b-9c0d-1e2f3a4b5c6d": [
    {
      id: "tariff-005",
      deployed_model_id: "d2a3b4c5-e6f7-4a8b-9c0d-1e2f3a4b5c6d",
      name: "Realtime",
      input_price_per_token: "0.00000025",
      output_price_per_token: "0.000002",
      valid_from: "2025-09-15T00:00:00Z",
      valid_until: null,
      api_key_purpose: "realtime" as const,
      completion_window: null,
      is_active: true,
    },
    {
      id: "tariff-006",
      deployed_model_id: "d2a3b4c5-e6f7-4a8b-9c0d-1e2f3a4b5c6d",
      name: "Batch (24h)",
      input_price_per_token: "0.000000125",
      output_price_per_token: "0.000001",
      valid_from: "2025-09-15T00:00:00Z",
      valid_until: null,
      api_key_purpose: "batch" as const,
      completion_window: "24h",
      is_active: true,
    },
    {
      id: "tariff-007",
      deployed_model_id: "d2a3b4c5-e6f7-4a8b-9c0d-1e2f3a4b5c6d",
      name: "Batch (1h)",
      input_price_per_token: "0.0000001875",
      output_price_per_token: "0.0000015",
      valid_from: "2025-09-15T00:00:00Z",
      valid_until: null,
      api_key_purpose: "batch" as const,
      completion_window: "1h",
      is_active: true,
    },
    {
      id: "tariff-008",
      deployed_model_id: "d2a3b4c5-e6f7-4a8b-9c0d-1e2f3a4b5c6d",
      name: "Playground",
      input_price_per_token: "0.00000025",
      output_price_per_token: "0.000002",
      valid_from: "2025-09-15T00:00:00Z",
      valid_until: null,
      api_key_purpose: "playground" as const,
      completion_window: null,
      is_active: true,
    },
  ],
  // openai/gpt-oss-20b — Realtime $0.04/$0.30 per M
  "d3a4b5c6-e7f8-4a9b-0c1d-2e3f4a5b6c7d": [
    {
      id: "tariff-009",
      deployed_model_id: "d3a4b5c6-e7f8-4a9b-0c1d-2e3f4a5b6c7d",
      name: "Realtime",
      input_price_per_token: "0.00000004",
      output_price_per_token: "0.0000003",
      valid_from: "2025-11-01T00:00:00Z",
      valid_until: null,
      api_key_purpose: "realtime" as const,
      completion_window: null,
      is_active: true,
    },
    {
      id: "tariff-010",
      deployed_model_id: "d3a4b5c6-e7f8-4a9b-0c1d-2e3f4a5b6c7d",
      name: "Batch (24h)",
      input_price_per_token: "0.00000002",
      output_price_per_token: "0.00000015",
      valid_from: "2025-11-01T00:00:00Z",
      valid_until: null,
      api_key_purpose: "batch" as const,
      completion_window: "24h",
      is_active: true,
    },
    {
      id: "tariff-011",
      deployed_model_id: "d3a4b5c6-e7f8-4a9b-0c1d-2e3f4a5b6c7d",
      name: "Batch (1h)",
      input_price_per_token: "0.00000003",
      output_price_per_token: "0.000000225",
      valid_from: "2025-11-01T00:00:00Z",
      valid_until: null,
      api_key_purpose: "batch" as const,
      completion_window: "1h",
      is_active: true,
    },
    {
      id: "tariff-012",
      deployed_model_id: "d3a4b5c6-e7f8-4a9b-0c1d-2e3f4a5b6c7d",
      name: "Playground",
      input_price_per_token: "0.00000004",
      output_price_per_token: "0.0000003",
      valid_from: "2025-11-01T00:00:00Z",
      valid_until: null,
      api_key_purpose: "playground" as const,
      completion_window: null,
      is_active: true,
    },
  ],
  // Qwen3-VL-235B-A22B-Instruct-FP8 — Realtime $0.60/$1.20 per M
  "d4a5b6c7-e8f9-4a0b-1c2d-3e4f5a6b7c8d": [
    {
      id: "tariff-013",
      deployed_model_id: "d4a5b6c7-e8f9-4a0b-1c2d-3e4f5a6b7c8d",
      name: "Realtime",
      input_price_per_token: "0.0000006",
      output_price_per_token: "0.0000012",
      valid_from: "2025-08-20T00:00:00Z",
      valid_until: null,
      api_key_purpose: "realtime" as const,
      completion_window: null,
      is_active: true,
    },
    {
      id: "tariff-014",
      deployed_model_id: "d4a5b6c7-e8f9-4a0b-1c2d-3e4f5a6b7c8d",
      name: "Batch (24h)",
      input_price_per_token: "0.0000003",
      output_price_per_token: "0.0000006",
      valid_from: "2025-08-20T00:00:00Z",
      valid_until: null,
      api_key_purpose: "batch" as const,
      completion_window: "24h",
      is_active: true,
    },
    {
      id: "tariff-015",
      deployed_model_id: "d4a5b6c7-e8f9-4a0b-1c2d-3e4f5a6b7c8d",
      name: "Batch (1h)",
      input_price_per_token: "0.00000045",
      output_price_per_token: "0.0000009",
      valid_from: "2025-08-20T00:00:00Z",
      valid_until: null,
      api_key_purpose: "batch" as const,
      completion_window: "1h",
      is_active: true,
    },
    {
      id: "tariff-016",
      deployed_model_id: "d4a5b6c7-e8f9-4a0b-1c2d-3e4f5a6b7c8d",
      name: "Playground",
      input_price_per_token: "0.0000006",
      output_price_per_token: "0.0000012",
      valid_from: "2025-08-20T00:00:00Z",
      valid_until: null,
      api_key_purpose: "playground" as const,
      completion_window: null,
      is_active: true,
    },
  ],
  // Qwen3-VL-30B-A3B-Instruct-FP8 — Realtime $0.16/$0.80 per M
  "d5a6b7c8-e9f0-4a1b-2c3d-4e5f6a7b8c9d": [
    {
      id: "tariff-017",
      deployed_model_id: "d5a6b7c8-e9f0-4a1b-2c3d-4e5f6a7b8c9d",
      name: "Realtime",
      input_price_per_token: "0.00000016",
      output_price_per_token: "0.0000008",
      valid_from: "2025-08-20T00:00:00Z",
      valid_until: null,
      api_key_purpose: "realtime" as const,
      completion_window: null,
      is_active: true,
    },
    {
      id: "tariff-018",
      deployed_model_id: "d5a6b7c8-e9f0-4a1b-2c3d-4e5f6a7b8c9d",
      name: "Batch (24h)",
      input_price_per_token: "0.00000008",
      output_price_per_token: "0.0000004",
      valid_from: "2025-08-20T00:00:00Z",
      valid_until: null,
      api_key_purpose: "batch" as const,
      completion_window: "24h",
      is_active: true,
    },
    {
      id: "tariff-019",
      deployed_model_id: "d5a6b7c8-e9f0-4a1b-2c3d-4e5f6a7b8c9d",
      name: "Batch (1h)",
      input_price_per_token: "0.00000012",
      output_price_per_token: "0.0000006",
      valid_from: "2025-08-20T00:00:00Z",
      valid_until: null,
      api_key_purpose: "batch" as const,
      completion_window: "1h",
      is_active: true,
    },
    {
      id: "tariff-020",
      deployed_model_id: "d5a6b7c8-e9f0-4a1b-2c3d-4e5f6a7b8c9d",
      name: "Playground",
      input_price_per_token: "0.00000016",
      output_price_per_token: "0.0000008",
      valid_from: "2025-08-20T00:00:00Z",
      valid_until: null,
      api_key_purpose: "playground" as const,
      completion_window: null,
      is_active: true,
    },
  ],
  // Qwen3-14B-FP8 — Realtime $0.05/$0.60 per M
  "d6a7b8c9-e0f1-4a2b-3c4d-5e6f7a8b9c0d": [
    {
      id: "tariff-021",
      deployed_model_id: "d6a7b8c9-e0f1-4a2b-3c4d-5e6f7a8b9c0d",
      name: "Realtime",
      input_price_per_token: "0.00000005",
      output_price_per_token: "0.0000006",
      valid_from: "2025-07-10T00:00:00Z",
      valid_until: null,
      api_key_purpose: "realtime" as const,
      completion_window: null,
      is_active: true,
    },
    {
      id: "tariff-022",
      deployed_model_id: "d6a7b8c9-e0f1-4a2b-3c4d-5e6f7a8b9c0d",
      name: "Batch (24h)",
      input_price_per_token: "0.000000025",
      output_price_per_token: "0.0000003",
      valid_from: "2025-07-10T00:00:00Z",
      valid_until: null,
      api_key_purpose: "batch" as const,
      completion_window: "24h",
      is_active: true,
    },
    {
      id: "tariff-023",
      deployed_model_id: "d6a7b8c9-e0f1-4a2b-3c4d-5e6f7a8b9c0d",
      name: "Batch (1h)",
      input_price_per_token: "0.0000000375",
      output_price_per_token: "0.00000045",
      valid_from: "2025-07-10T00:00:00Z",
      valid_until: null,
      api_key_purpose: "batch" as const,
      completion_window: "1h",
      is_active: true,
    },
    {
      id: "tariff-024",
      deployed_model_id: "d6a7b8c9-e0f1-4a2b-3c4d-5e6f7a8b9c0d",
      name: "Playground",
      input_price_per_token: "0.00000005",
      output_price_per_token: "0.0000006",
      valid_from: "2025-07-10T00:00:00Z",
      valid_until: null,
      api_key_purpose: "playground" as const,
      completion_window: null,
      is_active: true,
    },
  ],
  // Qwen3-Embedding-8B — Realtime $0.04/$0.00 per M
  "d7a8b9c0-e1f2-4a3b-4c5d-6e7f8a9b0c1d": [
    {
      id: "tariff-025",
      deployed_model_id: "d7a8b9c0-e1f2-4a3b-4c5d-6e7f8a9b0c1d",
      name: "Realtime",
      input_price_per_token: "0.00000004",
      output_price_per_token: "0",
      valid_from: "2025-07-10T00:00:00Z",
      valid_until: null,
      api_key_purpose: "realtime" as const,
      completion_window: null,
      is_active: true,
    },
    {
      id: "tariff-026",
      deployed_model_id: "d7a8b9c0-e1f2-4a3b-4c5d-6e7f8a9b0c1d",
      name: "Batch (24h)",
      input_price_per_token: "0.00000002",
      output_price_per_token: "0",
      valid_from: "2025-07-10T00:00:00Z",
      valid_until: null,
      api_key_purpose: "batch" as const,
      completion_window: "24h",
      is_active: true,
    },
    {
      id: "tariff-027",
      deployed_model_id: "d7a8b9c0-e1f2-4a3b-4c5d-6e7f8a9b0c1d",
      name: "Playground",
      input_price_per_token: "0.00000004",
      output_price_per_token: "0",
      valid_from: "2025-07-10T00:00:00Z",
      valid_until: null,
      api_key_purpose: "playground" as const,
      completion_window: null,
      is_active: true,
    },
  ],
  // Qwen3.5-9B — Realtime $0.10/$0.40 per M, Batch $0.05/$0.20 per M
  "d8b9c0d1-e2f3-4a4b-5c6d-7e8f9a0b1c2d": [
    {
      id: "tariff-028",
      deployed_model_id: "d8b9c0d1-e2f3-4a4b-5c6d-7e8f9a0b1c2d",
      name: "Realtime",
      input_price_per_token: "0.0000001",
      output_price_per_token: "0.0000004",
      valid_from: "2026-03-02T00:00:00Z",
      valid_until: null,
      api_key_purpose: "realtime" as const,
      completion_window: null,
      is_active: true,
    },
    {
      id: "tariff-029",
      deployed_model_id: "d8b9c0d1-e2f3-4a4b-5c6d-7e8f9a0b1c2d",
      name: "Batch (24h)",
      input_price_per_token: "0.00000005",
      output_price_per_token: "0.0000002",
      valid_from: "2026-03-02T00:00:00Z",
      valid_until: null,
      api_key_purpose: "batch" as const,
      completion_window: "24h",
      is_active: true,
    },
    {
      id: "tariff-030",
      deployed_model_id: "d8b9c0d1-e2f3-4a4b-5c6d-7e8f9a0b1c2d",
      name: "Playground",
      input_price_per_token: "0.0000001",
      output_price_per_token: "0.0000004",
      valid_from: "2026-03-02T00:00:00Z",
      valid_until: null,
      api_key_purpose: "playground" as const,
      completion_window: null,
      is_active: true,
    },
  ],
};

// Compute user balance from transactions (sum of credits minus debits)
function computeUserBalance(userId: string): number {
  const userTransactions = transactionsData.filter((t) => t.user_id === userId);
  return userTransactions.reduce((balance, tx) => {
    const isCredit =
      tx.transaction_type === "admin_grant" ||
      tx.transaction_type === "purchase";
    return isCredit ? balance + tx.amount : balance - tx.amount;
  }, 0);
}

// Compute the time shift needed to make request data appear recent.
// The raw JSON has fixed timestamps; we shift them so the latest request is "now".
function getRequestsTimeShift(): number {
  if (requestsData.length === 0) return 0;
  const latestOriginal = Math.max(
    ...requestsData.map((req) => new Date(req.timestamp).getTime()),
  );
  return Date.now() - latestOriginal;
}

// Return a shifted copy of a request (timestamp + response.created)
function shiftRequest(req: DemoRequest, timeShift: number): DemoRequest {
  const shiftedTs = new Date(
    new Date(req.timestamp).getTime() + timeShift,
  ).toISOString();
  return {
    ...req,
    timestamp: shiftedTs,
    response: {
      ...req.response,
      created: Math.floor(
        (new Date(req.timestamp).getTime() + timeShift) / 1000,
      ),
    },
  };
}

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
    const skip = parseInt(url.searchParams.get("skip") || "0");
    const limit = parseInt(url.searchParams.get("limit") || "10");
    const search = url.searchParams.get("search");

    let users = [...usersData];

    // Apply search filter (case-insensitive substring match on username, email, or display_name)
    if (search) {
      const searchLower = search.toLowerCase();
      users = users.filter(
        (u) =>
          u.username.toLowerCase().includes(searchLower) ||
          u.email.toLowerCase().includes(searchLower) ||
          (u.display_name?.toLowerCase().includes(searchLower) ?? false),
      );
    }

    if (include?.includes("groups")) {
      const userGroupsData = getUserGroupsData();
      users = users.map((user) => ({
        ...user,
        groups: (userGroupsData[user.id] || [])
          .map((id) => groupsData.find((v) => v.id === id))
          .filter((g): g is Group => g !== undefined),
      }));
    }

    const totalCount = users.length;
    const paginatedUsers = users.slice(skip, skip + limit);

    return HttpResponse.json({
      data: paginatedUsers,
      total_count: totalCount,
      skip,
      limit,
    });
  }),

  http.get("/admin/api/v1/users/:id", ({ params, request }) => {
    const url = new URL(request.url);
    const include = url.searchParams.get("include");

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

    // Add billing information if requested
    let result = { ...user };
    if (include?.includes("billing")) {
      // Compute current balance from all transactions
      const creditBalance = computeUserBalance(user.id);
      result = { ...result, credit_balance: creditBalance };
    }

    // Apply persisted role overrides for current user
    if (params.id === "current") {
      const persistedRoles = getCurrentUserRoles(demoState);
      if (persistedRoles) {
        result = { ...result, roles: persistedRoles as Role[] };
      }
    }

    return HttpResponse.json(result);
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
      external_user_id: crypto.randomUUID(),
      has_payment_provider_id: false,
      batch_notifications_enabled: false,
      low_balance_threshold: 2.0,
      auto_topup_amount: null,
      auto_topup_threshold: null,
      has_auto_topup_payment_method: false,
    };
    return HttpResponse.json(newUser, { status: 201 });
  }),

  http.patch("/admin/api/v1/users/:id", async ({ params, request }) => {
    const user = usersData.find((u) => u.id === params.id);
    if (!user) {
      return HttpResponse.json({ error: "User not found" }, { status: 404 });
    }
    const body = (await request.json()) as UserUpdateRequest;

    // Persist role changes for the current user (first user in demo)
    if (user.id === usersData[0].id && body.roles) {
      demoState = setCurrentUserRoles(demoState, body.roles);
    }

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
  http.get("/admin/api/v1/users/:userId/api-keys", ({ request }) => {
    const url = new URL(request.url);
    const skip = parseInt(url.searchParams.get("skip") || "0", 10);
    const limit = parseInt(url.searchParams.get("limit") || "10", 10);

    const paginatedData = apiKeysData.slice(skip, skip + limit);

    return HttpResponse.json({
      data: paginatedData,
      total_count: apiKeysData.length,
      skip,
      limit,
    });
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

  // Webhooks under users
  http.get("/admin/api/v1/users/:userId/webhooks", () => {
    return HttpResponse.json([]);
  }),

  http.post("/admin/api/v1/users/:userId/webhooks", async ({ request }) => {
    const body = (await request.json()) as {
      url: string;
      event_types?: string[];
      description?: string;
    };
    const now = new Date().toISOString();
    return HttpResponse.json(
      {
        id: `wh-${Date.now()}`,
        user_id: "550e8400-e29b-41d4-a716-446655440000",
        url: body.url,
        enabled: true,
        event_types: body.event_types || null,
        description: body.description || null,
        created_at: now,
        updated_at: now,
        disabled_at: null,
        secret: `whsec_${Math.random().toString(36).substring(2, 34)}`,
      },
      { status: 201 },
    );
  }),

  http.patch(
    "/admin/api/v1/users/:userId/webhooks/:webhookId",
    async ({ params, request }) => {
      const body = (await request.json()) as Record<string, unknown>;
      return HttpResponse.json({
        id: params.webhookId,
        user_id: "550e8400-e29b-41d4-a716-446655440000",
        url: (body.url as string) || "https://example.com/webhook",
        enabled: body.enabled !== undefined ? body.enabled : true,
        event_types: body.event_types !== undefined ? body.event_types : null,
        description: body.description !== undefined ? body.description : null,
        created_at: new Date().toISOString(),
        updated_at: new Date().toISOString(),
        disabled_at: null,
      });
    },
  ),

  http.delete("/admin/api/v1/users/:userId/webhooks/:webhookId", () => {
    return HttpResponse.json(null, { status: 204 });
  }),

  http.post(
    "/admin/api/v1/users/:userId/webhooks/:webhookId/rotate-secret",
    () => {
      return HttpResponse.json({
        id: "wh-mock",
        user_id: "550e8400-e29b-41d4-a716-446655440000",
        url: "https://example.com/webhook",
        enabled: true,
        event_types: null,
        description: null,
        created_at: new Date().toISOString(),
        updated_at: new Date().toISOString(),
        disabled_at: null,
        secret: `whsec_${Math.random().toString(36).substring(2, 34)}`,
      });
    },
  ),

  // Models API
  http.get("/admin/api/v1/models", ({ request }) => {
    const url = new URL(request.url);
    const endpoint = url.searchParams.get("endpoint");
    const include = url.searchParams.get("include");
    const accessible = url.searchParams.get("accessible");
    const skip = parseInt(url.searchParams.get("skip") || "0", 10);
    const limit = parseInt(url.searchParams.get("limit") || "10", 10);
    const search = url.searchParams.get("search");
    const provider = url.searchParams.get("provider");
    const modelType = url.searchParams.get("model_type");
    const capability = url.searchParams.get("capability");
    const sort = url.searchParams.get("sort");
    const sortDirection = url.searchParams.get("sort_direction");
    const isComposite = url.searchParams.get("is_composite");
    const groupFilter = url.searchParams.get("group");

    let models: Model[] = [...modelsData];

    if (endpoint) {
      models = models.filter((m) => m.hosted_on === endpoint);
    }

    // Apply group filter (comma-separated group UUIDs)
    if (groupFilter) {
      const filterGroupIds = new Set(groupFilter.split(",").map((s) => s.trim()));
      const modelsGroupsData = getModelsGroupsData();
      models = models.filter((model) => {
        const modelGroupIds = modelsGroupsData[model.id] ?? [];
        return modelGroupIds.some((gid) => filterGroupIds.has(gid));
      });
    }

    // Apply is_composite filter (virtual vs hosted)
    if (isComposite !== null) {
      const wantComposite = isComposite === "true";
      models = models.filter((m) => !!m.is_composite === wantComposite);
    }

    // Apply search filter (case-insensitive substring match on alias or model_name)
    if (search) {
      const searchLower = search.toLowerCase();
      models = models.filter(
        (m) =>
          m.alias.toLowerCase().includes(searchLower) ||
          m.model_name.toLowerCase().includes(searchLower),
      );
    }

    // Apply provider filter
    if (provider) {
      const providerLower = provider.toLowerCase();
      models = models.filter(
        (m) => m.metadata?.provider?.toLowerCase() === providerLower,
      );
    }

    // Apply model_type filter
    if (modelType) {
      models = models.filter((m) => m.model_type === modelType);
    }

    // Apply capability filter
    if (capability) {
      models = models.filter((m) => m.capabilities?.includes(capability));
    }

    // Filter models by accessibility if requested
    if (accessible === "true") {
      const currentUser = usersData[0];

      if (currentUser) {
        const userGroupsData = getUserGroupsData();
        const modelsGroupsData = getModelsGroupsData();
        const userGroupIds = new Set(userGroupsData[currentUser.id] || []);

        models = models.filter((model) => {
          const modelGroupIds = modelsGroupsData[model.id] ?? [];
          return modelGroupIds.some((groupId) => userGroupIds.has(groupId));
        });
      }
    }

    // Apply sorting
    if (sort) {
      const dir = sortDirection === "asc" ? 1 : sortDirection === "desc" ? -1 : 0;
      models.sort((a, b) => {
        let cmp = 0;
        switch (sort) {
          case "alias":
            cmp = a.alias.localeCompare(b.alias);
            return dir || cmp;
          case "intelligence_index": {
            const ai = a.metadata?.intelligence_index ?? -Infinity;
            const bi = b.metadata?.intelligence_index ?? -Infinity;
            cmp = bi - ai;
            return dir ? (dir === 1 ? -cmp : cmp) : cmp;
          }
          case "released_at": {
            const ad = a.metadata?.released_at ?? "";
            const bd = b.metadata?.released_at ?? "";
            cmp = bd.localeCompare(ad);
            return dir ? (dir === 1 ? -cmp : cmp) : cmp;
          }
          case "provider": {
            const ap = a.metadata?.provider ?? "\uffff";
            const bp = b.metadata?.provider ?? "\uffff";
            cmp = ap.localeCompare(bp);
            return dir || cmp;
          }
          default:
            return 0;
        }
      });
    }

    // Store total count before pagination
    const total_count = models.length;

    // Apply pagination
    models = models.slice(skip, skip + limit);

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

    if (include?.includes("pricing")) {
      models = models.map((model) => ({
        ...model,
        tariffs: modelTariffs[model.id] || [],
      }));
    }

    if (include?.includes("components")) {
      models = models.map((model) => ({
        ...model,
        components: model.is_composite ? resolveComponents(model.id) : undefined,
      }));
    }

    // Build facets from all models (unfiltered) if requested
    const facets = include?.includes("facets")
      ? {
          providers: [
            ...new Set(
              modelsData
                .map((m) => m.metadata?.provider)
                .filter((p): p is string => !!p),
            ),
          ].sort(),
          capabilities: [
            ...new Set(modelsData.flatMap((m) => m.capabilities ?? [])),
          ].sort(),
          model_types: [
            ...new Set(
              modelsData
                .map((m) => m.model_type)
                .filter((t): t is ModelType => !!t),
            ),
          ].sort(),
        }
      : undefined;

    return HttpResponse.json({
      data: models,
      total_count,
      skip,
      limit,
      ...(facets && { facets }),
    });
  }),

  http.get("/admin/api/v1/models/:id", ({ params, request }) => {
    const url = new URL(request.url);
    const include = url.searchParams.get("include");
    const model = modelsData.find((m) => m.id === params.id);
    if (!model) {
      return HttpResponse.json({ error: "Model not found" }, { status: 404 });
    }
    let result: Model = { ...model };
    if (include?.includes("pricing")) {
      result = { ...result, tariffs: modelTariffs[model.id] || [] };
    }
    if (include?.includes("groups")) {
      const modelsGroupsData = getModelsGroupsData();
      result = {
        ...result,
        groups:
          modelsGroupsData[model.id]
            ?.map((id) => groupsData.find((g) => g.id === id))
            .filter((g): g is Group => g !== undefined) ?? [],
      };
    }
    if (include?.includes("components") && model.is_composite) {
      result = { ...result, components: resolveComponents(model.id) };
    }
    return HttpResponse.json(result);
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

  // Virtual model components
  http.get("/admin/api/v1/models/:id/components", ({ params }) => {
    const model = modelsData.find((m) => m.id === params.id);
    if (!model) {
      return HttpResponse.json({ error: "Model not found" }, { status: 404 });
    }
    return HttpResponse.json(resolveComponents(model.id));
  }),

  http.post(
    "/admin/api/v1/models/:id/components/:componentId",
    async ({ params, request }) => {
      const model = modelsData.find((m) => m.id === params.id);
      if (!model) {
        return HttpResponse.json({ error: "Model not found" }, { status: 404 });
      }
      const componentModel = modelsData.find((m) => m.id === params.componentId);
      if (!componentModel) {
        return HttpResponse.json({ error: "Component model not found" }, { status: 404 });
      }
      const body = (await request.json()) as { weight?: number; enabled?: boolean };
      const component: StoredComponent = {
        componentModelId: String(params.componentId),
        weight: body.weight ?? 1,
        enabled: body.enabled ?? true,
        sort_order: getModelComponentsState(demoState, model.id, initialModelComponents).length,
      };
      demoState = addModelComponentState(demoState, model.id, component, initialModelComponents);

      const endpoint = componentModel.hosted_on
        ? endpointsData.find((e) => e.id === componentModel.hosted_on)
        : undefined;
      return HttpResponse.json(
        {
          weight: component.weight,
          enabled: component.enabled,
          sort_order: component.sort_order,
          created_at: new Date().toISOString(),
          model: {
            id: componentModel.id,
            alias: componentModel.alias,
            model_name: componentModel.model_name,
            description: componentModel.description,
            model_type: componentModel.model_type,
            endpoint: endpoint ? { id: endpoint.id, name: endpoint.name } : undefined,
          },
        },
        { status: 201 },
      );
    },
  ),

  http.patch(
    "/admin/api/v1/models/:id/components/:componentId",
    async ({ params, request }) => {
      const model = modelsData.find((m) => m.id === params.id);
      if (!model) {
        return HttpResponse.json({ error: "Model not found" }, { status: 404 });
      }
      const body = (await request.json()) as { weight?: number; enabled?: boolean; sort_order?: number };
      demoState = updateModelComponentState(
        demoState,
        model.id,
        String(params.componentId),
        body,
        initialModelComponents,
      );
      // Return the updated component list entry
      const resolved = resolveComponents(model.id);
      const updated = resolved.find((c) => c.model.id === params.componentId);
      if (!updated) {
        return HttpResponse.json({ error: "Component not found" }, { status: 404 });
      }
      return HttpResponse.json(updated);
    },
  ),

  http.delete(
    "/admin/api/v1/models/:id/components/:componentId",
    ({ params }) => {
      const model = modelsData.find((m) => m.id === params.id);
      if (!model) {
        return HttpResponse.json({ error: "Model not found" }, { status: 404 });
      }
      demoState = removeModelComponentState(
        demoState,
        model.id,
        String(params.componentId),
        initialModelComponents,
      );
      return new HttpResponse(null, { status: 204 });
    },
  ),

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
          id: "gpt-4o",
          created: 1715367049,
          object: "model" as const,
          owned_by: "openai",
        },
        {
          id: "gpt-4o-mini",
          created: 1721172049,
          object: "model" as const,
          owned_by: "openai",
        },
        {
          id: "text-embedding-3-small",
          created: 1705948997,
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
    const skip = parseInt(url.searchParams.get("skip") || "0");
    const limit = parseInt(url.searchParams.get("limit") || "10");

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

    const totalCount = groups.length;
    const paginatedGroups = groups.slice(skip, skip + limit);

    return HttpResponse.json({
      data: paginatedGroups,
      total_count: totalCount,
      skip,
      limit,
    });
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
      region: "EU West",
      organization: "Acme Corp",
      payment_enabled: true,
      docs_url: "https://docs.doubleword.ai/control-layer",
      batches: {
        enabled: true,
        allowed_completion_windows: ["24h", "1h"],
      },
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

      // Check if this model has reasoning capability
      const modelEntry = modelsData.find(
        (m) => m.alias === model || m.model_name === model,
      );
      const isReasoningModel =
        modelEntry?.capabilities?.includes("reasoning") ?? false;

      // Build reasoning chunks if applicable
      const reasoningText = isReasoningModel
        ? `Let me think about this step by step...\n\nThe user asked: "${userContent}"\n\nI need to consider the key aspects of this question and formulate a clear, helpful response. Let me break this down into parts and address each one carefully.`
        : "";
      const reasoningChunkSize = Math.max(
        10,
        Math.floor(reasoningText.length / 6),
      );
      const reasoningChunks: string[] = [];
      for (let i = 0; i < reasoningText.length; i += reasoningChunkSize) {
        reasoningChunks.push(
          reasoningText.substring(i, i + reasoningChunkSize),
        );
      }

      // Split response into chunks for streaming (roughly 10-20 chars per chunk)
      const chunkSize = Math.max(10, Math.floor(responseContent.length / 5));
      const contentChunks: string[] = [];
      for (let i = 0; i < responseContent.length; i += chunkSize) {
        contentChunks.push(responseContent.substring(i, i + chunkSize));
      }

      const stream = new ReadableStream({
        start(controller) {
          let reasoningIndex = 0;
          let contentIndex = 0;
          let sentRole = false;

          const sendChunk = () => {
            const chunkBase = {
              id: `chatcmpl-${Date.now()}`,
              object: "chat.completion.chunk",
              created: Math.floor(Date.now() / 1000),
              model: model,
            };

            if (reasoningIndex < reasoningChunks.length) {
              // Send reasoning_content chunks first
              const delta: Record<string, string> = {
                reasoning_content: reasoningChunks[reasoningIndex],
              };
              if (!sentRole) {
                delta.role = "assistant";
                sentRole = true;
              }
              controller.enqueue(
                encoder.encode(
                  `data: ${JSON.stringify({ ...chunkBase, choices: [{ index: 0, delta, finish_reason: null }] })}\n\n`,
                ),
              );
              reasoningIndex++;
              setTimeout(sendChunk, 80);
            } else if (contentIndex < contentChunks.length) {
              // Then send content chunks
              const delta: Record<string, string> = {
                content: contentChunks[contentIndex],
              };
              if (!sentRole) {
                delta.role = "assistant";
                sentRole = true;
              }
              controller.enqueue(
                encoder.encode(
                  `data: ${JSON.stringify({ ...chunkBase, choices: [{ index: 0, delta, finish_reason: null }] })}\n\n`,
                ),
              );
              contentIndex++;
              setTimeout(sendChunk, 100);
            } else {
              // Send final chunk with usage
              const finalChunk = {
                ...chunkBase,
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

  // List requests — returns { entries: AnalyticsEntry[] } matching ListAnalyticsResponse
  http.get("/admin/api/v1/requests", ({ request }) => {
    const url = new URL(request.url);
    const limitParam = url.searchParams.get("limit");
    const skipParam = url.searchParams.get("skip");
    const orderDesc = url.searchParams.get("order_desc") === "true";
    const timestampAfter = url.searchParams.get("timestamp_after");
    const timestampBefore = url.searchParams.get("timestamp_before");
    const modelFilter = url.searchParams.get("model");
    const customIdFilter = url.searchParams.get("custom_id");

    // Build model alias -> tariff lookup for pricing
    const aliasTariffMap: Record<
      string,
      { input: string; output: string }
    > = {};
    for (const model of modelsData) {
      const tariffs = modelTariffs[model.id];
      if (tariffs) {
        // Use standard (non-batch) tariff
        const std = tariffs.find((t) => t.api_key_purpose === null) || tariffs[0];
        if (std) {
          aliasTariffMap[model.alias] = {
            input: std.input_price_per_token,
            output: std.output_price_per_token,
          };
        }
      }
    }

    // Shift all request timestamps to be relative to now
    const timeShift = getRequestsTimeShift();
    let filtered = requestsData.map((req) => shiftRequest(req, timeShift));

    // Filter by timestamp range
    if (timestampAfter) {
      const afterDate = new Date(timestampAfter);
      filtered = filtered.filter((req) => new Date(req.timestamp) >= afterDate);
    }
    if (timestampBefore) {
      const beforeDate = new Date(timestampBefore);
      filtered = filtered.filter(
        (req) => new Date(req.timestamp) <= beforeDate,
      );
    }

    // Filter by model
    if (modelFilter) {
      filtered = filtered.filter((req) => req.model === modelFilter);
    }

    // Filter by custom_id (substring match)
    if (customIdFilter) {
      filtered = filtered.filter(
        (req) =>
          req.metadata?.custom_id &&
          req.metadata.custom_id
            .toLowerCase()
            .includes(customIdFilter.toLowerCase()),
      );
    }

    // Sort by timestamp
    filtered.sort((a, b) => {
      const aTime = new Date(a.timestamp).getTime();
      const bTime = new Date(b.timestamp).getTime();
      return orderDesc ? bTime - aTime : aTime - bTime;
    });

    // Apply skip + limit pagination
    const skip = skipParam ? parseInt(skipParam, 10) : 0;
    const limit = limitParam ? parseInt(limitParam, 10) : 50;
    const paginated = filtered.slice(skip, skip + limit);

    // Transform DemoRequest[] -> AnalyticsEntry[]
    const entries = paginated.map((req, idx) => {
      const pricing = aliasTariffMap[req.model];
      return {
        id: skip + idx + 1,
        timestamp: req.timestamp,
        method: "POST",
        uri: req.model.includes("embedding")
          ? "/v1/embeddings"
          : req.model.includes("rerank")
            ? "/v1/rerank"
            : "/v1/chat/completions",
        model: req.model,
        status_code: 200,
        duration_ms: req.duration_ms,
        prompt_tokens: req.response?.usage?.prompt_tokens ?? 0,
        completion_tokens: req.response?.usage?.completion_tokens ?? 0,
        total_tokens: req.response?.usage?.total_tokens ?? 0,
        response_type: req.model.includes("embedding")
          ? "embeddings"
          : req.model.includes("rerank")
            ? "rerank"
            : "chat_completions",
        input_price_per_token: pricing?.input ?? null,
        output_price_per_token: pricing?.output ?? null,
      };
    });

    return HttpResponse.json({ entries });
  }),

  // Requests aggregate
  http.get("/admin/api/v1/requests/aggregate", ({ request }) => {
    const url = new URL(request.url);
    const model = url.searchParams.get("model") || undefined;
    const timestampAfter = url.searchParams.get("timestamp_after");
    const timestampBefore = url.searchParams.get("timestamp_before");

    // Shift all request timestamps to be relative to now
    const timeShift = getRequestsTimeShift();
    let filtered = requestsData.map((req) => shiftRequest(req, timeShift));

    // Filter by model
    if (model) {
      filtered = filtered.filter((req) => req.model === model);
    }

    // Filter by timestamp range
    if (timestampAfter) {
      const afterDate = new Date(timestampAfter);
      filtered = filtered.filter((req) => new Date(req.timestamp) >= afterDate);
    }
    if (timestampBefore) {
      const beforeDate = new Date(timestampBefore);
      filtered = filtered.filter(
        (req) => new Date(req.timestamp) <= beforeDate,
      );
    }

    // Aggregate by model (ModelUsage[])
    const modelAgg: Record<
      string,
      { count: number; totalLatency: number; inputTokens: number; outputTokens: number }
    > = {};
    filtered.forEach((req) => {
      if (!modelAgg[req.model]) {
        modelAgg[req.model] = { count: 0, totalLatency: 0, inputTokens: 0, outputTokens: 0 };
      }
      modelAgg[req.model].count++;
      modelAgg[req.model].totalLatency += req.duration_ms || 0;
      modelAgg[req.model].inputTokens += req.response?.usage?.prompt_tokens || 0;
      modelAgg[req.model].outputTokens += req.response?.usage?.completion_tokens || 0;
    });
    const totalReqs = filtered.length;
    const models = Object.entries(modelAgg).map(([name, agg]) => ({
      model: name,
      count: agg.count,
      percentage: totalReqs > 0 ? Math.round((agg.count / totalReqs) * 100) : 0,
      avg_latency_ms: agg.count > 0 ? Math.round(agg.totalLatency / agg.count) : 0,
    }));

    // Aggregate by status code (StatusCodeBreakdown[])
    const statusCodes = [
      { status: "200", count: totalReqs, percentage: 100 },
    ];

    // Time series data (group by hour, matching TimeSeriesPoint)
    const timeSeriesMap: Record<
      string,
      { timestamp: string; requests: number; input_tokens: number; output_tokens: number; totalLatency: number }
    > = {};
    filtered.forEach((req) => {
      const date = new Date(req.timestamp);
      const hourKey = new Date(
        date.getFullYear(),
        date.getMonth(),
        date.getDate(),
        date.getHours(),
      ).toISOString();

      if (!timeSeriesMap[hourKey]) {
        timeSeriesMap[hourKey] = { timestamp: hourKey, requests: 0, input_tokens: 0, output_tokens: 0, totalLatency: 0 };
      }
      timeSeriesMap[hourKey].requests++;
      timeSeriesMap[hourKey].input_tokens += req.response?.usage?.prompt_tokens || 0;
      timeSeriesMap[hourKey].output_tokens += req.response?.usage?.completion_tokens || 0;
      timeSeriesMap[hourKey].totalLatency += req.duration_ms || 0;
    });

    const timeSeries = Object.values(timeSeriesMap)
      .map(({ totalLatency, ...point }) => ({
        ...point,
        avg_latency_ms: point.requests > 0 ? Math.round(totalLatency / point.requests) : null,
      }))
      .sort(
        (a, b) =>
          new Date(a.timestamp).getTime() - new Date(b.timestamp).getTime(),
      );

    return HttpResponse.json({
      models,
      status_codes: statusCodes,
      time_series: timeSeries,
      total_requests: totalReqs,
      total_tokens: filtered.reduce(
        (sum, req) => sum + (req.response?.usage?.total_tokens || 0),
        0,
      ),
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

  // Monitoring: pending request counts
  http.get("/admin/api/v1/monitoring/pending-request-counts", () => {
    // Demo mode: static example data (real data comes from fusillade)
    return HttpResponse.json({
      "Qwen/Qwen3.5-397B-A17B-FP8": { "1h": 12, "24h": 87 },
      "Qwen/Qwen3.5-35B-A3B-FP8": { "1h": 8, "24h": 45 },
      "Qwen/Qwen3-14B-FP8": { "1h": 3, "24h": 22 },
    });
  }),

  // Transactions API
  http.get("/admin/api/v1/transactions", ({ request }) => {
    const url = new URL(request.url);
    const userIdParam = url.searchParams.get("user_id");
    const limitParam = url.searchParams.get("limit");
    const skipParam = url.searchParams.get("skip");

    const limit = limitParam ? parseInt(limitParam, 10) : 100;
    const skip = skipParam ? parseInt(skipParam, 10) : 0;

    // If no userId provided, default to current user (first user in demo)
    const userId = userIdParam || usersData[0]?.id;

    // Filter by userId
    const filteredTransactions = userId
      ? transactionsData.filter((t) => t.user_id === userId)
      : [...transactionsData];

    // Sort by created_at descending (newest first)
    filteredTransactions.sort(
      (a, b) =>
        new Date(b.created_at).getTime() - new Date(a.created_at).getTime(),
    );

    // Compute current balance for the user
    const currentBalance = computeUserBalance(userId || "");

    // Apply pagination
    const paginatedTransactions = filteredTransactions.slice(
      skip,
      skip + limit,
    );

    // Return new response format with page_start_balance
    return HttpResponse.json({
      data: paginatedTransactions,
      total_count: filteredTransactions.length,
      skip,
      limit,
      page_start_balance: currentBalance,
    });
  }),

  http.post("/admin/api/v1/transactions", async ({ request }) => {
    const body = (await request.json()) as AddFundsRequest;

    // Validate user exists
    const user = usersData.find((u) => u.id === body.user_id);
    if (!user) {
      return HttpResponse.json({ error: "User not found" }, { status: 404 });
    }

    // Create new transaction (no balance_after stored)
    const newTransaction: Transaction = {
      id: `txn-${Date.now()}`,
      user_id: body.user_id,
      transaction_type: "admin_grant",
      amount: body.amount,
      source_id: "admin",
      description: body.description || "Funds added by admin",
      created_at: new Date().toISOString(),
    };

    // Persist the transaction to the mock data array
    transactionsData.unshift(newTransaction);

    return HttpResponse.json(newTransaction, { status: 201 });
  }),

  // Probes API
  http.get("/admin/api/v1/probes", () => {
    return HttpResponse.json([]);
  }),

  // ===== FILES API =====

  http.get("/ai/v1/files", ({ request }) => {
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

  http.get("/ai/v1/files/:id", ({ params }) => {
    const file = filesData.find((f) => f.id === params.id);
    if (!file) {
      return HttpResponse.json({ error: "File not found" }, { status: 404 });
    }
    return HttpResponse.json(file);
  }),

  http.post("/ai/v1/files", async ({ request }) => {
    const formData = await request.formData();
    const file = formData.get("file") as File;
    const purpose = formData.get("purpose") as string;
    const expiresAfterSeconds = formData.get("expires_after[seconds]");

    if (!file || !purpose) {
      return HttpResponse.json(
        { error: "Missing required fields: file and purpose" },
        { status: 400 },
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

  http.delete("/ai/v1/files/:id", ({ params }) => {
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

  // Get file content (supports pagination via limit/offset)
  http.get("/ai/v1/files/:id/content", ({ request, params }) => {
    const fileId = params.id as string;
    const url = new URL(request.url);
    const limit = url.searchParams.get("limit")
      ? parseInt(url.searchParams.get("limit")!)
      : undefined;
    const offset = parseInt(url.searchParams.get("offset") || "0");

    // Check if it's a batch output file
    const batch = batchesData.find(
      (b) => b.output_file_id === fileId || b.error_file_id === fileId,
    );

    if (batch) {
      // Batch output or error file
      const isErrorFile = batch.error_file_id === fileId;
      const requests = batchRequestsData[batch.id] || [];

      // Filter by type and apply pagination
      const filtered = isErrorFile
        ? requests.filter((r) => r.status === "failed")
        : requests.filter((r) => r.status === "completed");

      const paginated = limit
        ? filtered.slice(offset, offset + limit)
        : filtered.slice(offset);

      // Generate JSONL output
      const jsonl = paginated
        .map((r) =>
          JSON.stringify({
            id: r.id,
            custom_id: r.custom_id,
            response: isErrorFile ? null : r.response,
            error: isErrorFile ? r.error : null,
          }),
        )
        .join("\n");

      return HttpResponse.text(jsonl, {
        headers: {
          "Content-Type": "application/jsonl",
        },
      });
    }

    // Regular input file - return templates
    const templates = fileRequestsData[fileId] || [];
    const paginated = limit
      ? templates.slice(offset, offset + limit)
      : templates.slice(offset);

    const jsonl = paginated
      .map((t) =>
        JSON.stringify({
          custom_id: t.custom_id,
          method: t.method,
          url: t.url,
          body: t.body,
        }),
      )
      .join("\n");

    return HttpResponse.text(jsonl, {
      headers: {
        "Content-Type": "application/jsonl",
      },
    });
  }),

  // ===== BATCHES API =====

  http.get("/ai/v1/batches", ({ request }) => {
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

  http.get("/ai/v1/batches/:id", ({ params }) => {
    const batch = batchesData.find((b) => b.id === params.id);
    if (!batch) {
      return HttpResponse.json({ error: "Batch not found" }, { status: 404 });
    }
    return HttpResponse.json(batch);
  }),

  http.post("/ai/v1/batches", async ({ request }) => {
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

  http.post("/ai/v1/batches/:id/cancel", ({ params }) => {
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

  // ===== ORGANIZATIONS =====

  http.get("/admin/api/v1/organizations", ({ request }) => {
    const url = new URL(request.url);
    const skip = Number(url.searchParams.get("skip") || "0");
    const limit = Number(url.searchParams.get("limit") || "10");
    const search = url.searchParams.get("search") || "";

    let filtered = [...organizationsData];
    if (search) {
      const lower = search.toLowerCase();
      filtered = filtered.filter(
        (o) =>
          o.username.toLowerCase().includes(lower) ||
          (o.display_name || "").toLowerCase().includes(lower) ||
          o.email.toLowerCase().includes(lower),
      );
    }

    return HttpResponse.json({
      data: filtered.slice(skip, skip + limit),
      total_count: filtered.length,
      skip,
      limit,
    });
  }),

  http.get("/admin/api/v1/organizations/:id", ({ params }) => {
    const org = organizationsData.find((o) => o.id === params.id);
    if (!org) return HttpResponse.json({ error: "Not found" }, { status: 404 });
    return HttpResponse.json(org);
  }),

  http.post("/admin/api/v1/organizations", async ({ request }) => {
    const body = (await request.json()) as OrganizationCreateRequest;
    const newOrg = {
      id: `org-${Date.now()}`,
      username: body.name,
      external_user_id: `org|${body.name}`,
      email: body.email,
      display_name: body.display_name || null,
      avatar_url: null,
      is_admin: false,
      roles: ["StandardUser"],
      created_at: new Date().toISOString(),
      updated_at: new Date().toISOString(),
      auth_source: "proxy-header",
      credit_balance: 0,
      has_payment_provider_id: false,
      batch_notifications_enabled: false,
      low_balance_threshold: null,
      user_type: "organization",
      member_count: 1,
    };
    return HttpResponse.json(newOrg, { status: 201 });
  }),

  http.patch("/admin/api/v1/organizations/:id", async ({ params, request }) => {
    const body = (await request.json()) as OrganizationUpdateRequest;
    const org = organizationsData.find((o) => o.id === params.id);
    if (!org) return HttpResponse.json({ error: "Not found" }, { status: 404 });
    return HttpResponse.json({ ...org, ...body });
  }),

  http.delete("/admin/api/v1/organizations/:id", ({ params }) => {
    const org = organizationsData.find((o) => o.id === params.id);
    if (!org) return HttpResponse.json({ error: "Not found" }, { status: 404 });
    return new HttpResponse(null, { status: 204 });
  }),

  http.get("/admin/api/v1/organizations/:orgId/members", ({ params }) => {
    const members = orgMembersData[params.orgId as string] || [];
    return HttpResponse.json(members);
  }),

  http.post(
    "/admin/api/v1/organizations/:orgId/invites",
    async ({ request }) => {
      const body = (await request.json()) as InviteMemberRequest;
      return HttpResponse.json(
        {
          id: `inv-${Date.now()}`,
          email: body.email,
          role: body.role || "member",
          status: "pending",
          created_at: new Date().toISOString(),
          expires_at: new Date(
            Date.now() + 7 * 24 * 60 * 60 * 1000,
          ).toISOString(),
        },
        { status: 201 },
      );
    },
  ),

  http.delete(
    "/admin/api/v1/organizations/:orgId/invites/:inviteId",
    () => {
      return new HttpResponse(null, { status: 204 });
    },
  ),

  http.patch(
    "/admin/api/v1/organizations/:orgId/members/:userId",
    async ({ request }) => {
      const body = (await request.json()) as Record<string, unknown>;
      return HttpResponse.json({ role: body.role });
    },
  ),

  http.delete(
    "/admin/api/v1/organizations/:orgId/members/:userId",
    () => {
      return new HttpResponse(null, { status: 204 });
    },
  ),

  http.get("/admin/api/v1/organizations/invites/:token", () => {
    return HttpResponse.json({
      org_name: "Acme Corporation",
      role: "member",
      inviter_name: "Sarah Chen",
      expires_at: new Date(
        Date.now() + 7 * 24 * 60 * 60 * 1000,
      ).toISOString(),
    } satisfies InviteDetailsResponse);
  }),

  http.post("/admin/api/v1/organizations/invites/:token/accept", () => {
    return HttpResponse.json({ success: true });
  }),

  http.post("/admin/api/v1/organizations/invites/:token/decline", () => {
    return HttpResponse.json({ success: true });
  }),

  http.post("/admin/api/v1/session/organization", () => {
    return HttpResponse.json({ active_organization_id: null });
  }),

  // Usage API
  http.get("/admin/api/v1/usage", () => {
    return HttpResponse.json({
      total_input_tokens: 4_823_190,
      total_output_tokens: 1_247_830,
      total_request_count: 3842,
      total_batch_count: 127,
      avg_requests_per_batch: 30.2,
      total_cost: "48.23",
      estimated_realtime_cost: "96.46",
      by_model: [
        {
          model: "Qwen/Qwen3.5-397B-A17B-FP8",
          input_tokens: 2_100_000,
          output_tokens: 580_000,
          cost: "21.40",
          request_count: 1420,
        },
        {
          model: "Qwen/Qwen3.5-35B-A3B-FP8",
          input_tokens: 1_350_000,
          output_tokens: 390_000,
          cost: "13.50",
          request_count: 1180,
        },
        {
          model: "openai/gpt-oss-20b",
          input_tokens: 890_000,
          output_tokens: 210_000,
          cost: "8.90",
          request_count: 742,
        },
        {
          model: "Qwen/Qwen3-VL-235B-A22B-Instruct-FP8",
          input_tokens: 483_190,
          output_tokens: 67_830,
          cost: "4.43",
          request_count: 500,
        },
      ],
    });
  }),

  // Daemons API
  http.get("/ai/v1/daemons", () => {
    const now = Math.floor(Date.now() / 1000);
    return HttpResponse.json({
      daemons: [
        {
          id: "d-1a2b3c4d",
          status: "running",
          hostname: "dwctl-prod-7f8d9a-xk4np",
          pid: 1,
          version: "8.13.0",
          started_at: now - 86400 * 3,
          last_heartbeat: now - 2,
          stopped_at: null,
          stats: {
            requests_processed: 48210,
            requests_failed: 23,
            requests_in_flight: 7,
          },
          config: {
            claim_batch_size: 50,
            default_model_concurrency: 10,
            model_concurrency_limits: {},
            claim_interval_ms: 1000,
            min_retries: 1,
            stop_before_deadline_ms: null,
            max_retries: 3,
            backoff_ms: 1000,
            backoff_factor: 2.0,
            max_backoff_ms: 30000,
            timeout_ms: 300000,
            status_log_interval_ms: null,
            heartbeat_interval_ms: 5000,
            claim_timeout_ms: 60000,
            processing_timeout_ms: 600000,
          },
        },
        {
          id: "d-5e6f7a8b",
          status: "running",
          hostname: "dwctl-prod-7f8d9a-rm2qz",
          pid: 1,
          version: "8.13.0",
          started_at: now - 86400 * 3,
          last_heartbeat: now - 4,
          stopped_at: null,
          stats: {
            requests_processed: 45830,
            requests_failed: 19,
            requests_in_flight: 4,
          },
          config: {
            claim_batch_size: 50,
            default_model_concurrency: 10,
            model_concurrency_limits: {},
            claim_interval_ms: 1000,
            min_retries: 1,
            stop_before_deadline_ms: null,
            max_retries: 3,
            backoff_ms: 1000,
            backoff_factor: 2.0,
            max_backoff_ms: 30000,
            timeout_ms: 300000,
            status_log_interval_ms: null,
            heartbeat_interval_ms: 5000,
            claim_timeout_ms: 60000,
            processing_timeout_ms: 600000,
          },
        },
        {
          id: "d-9c0d1e2f",
          status: "dead",
          hostname: "dwctl-prod-6e7c8b-jw5tp",
          pid: 1,
          version: "8.12.0",
          started_at: now - 86400 * 7,
          last_heartbeat: now - 86400 * 2,
          stopped_at: now - 86400 * 2,
          stats: {
            requests_processed: 91204,
            requests_failed: 41,
            requests_in_flight: 0,
          },
          config: {
            claim_batch_size: 50,
            default_model_concurrency: 10,
            model_concurrency_limits: {},
            claim_interval_ms: 1000,
            min_retries: 1,
            stop_before_deadline_ms: null,
            max_retries: 3,
            backoff_ms: 1000,
            backoff_factor: 2.0,
            max_backoff_ms: 30000,
            timeout_ms: 300000,
            status_log_interval_ms: null,
            heartbeat_interval_ms: 5000,
            claim_timeout_ms: 60000,
            processing_timeout_ms: 600000,
          },
        },
      ],
    });
  }),
];
