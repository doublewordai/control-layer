/// A client implementation for the Control Layer (dwctl) backend API.
import type {
  Model,
  Endpoint,
  Group,
  User,
  ApiKey,
  ApiKeyCreateResponse,
  PaginatedResponse,
  ModelsQuery,
  GroupsQuery,
  UsersQuery,
  UserCreateRequest,
  GroupCreateRequest,
  ApiKeyCreateRequest,
  UserUpdateRequest,
  GroupUpdateRequest,
  ModelUpdateRequest,
  EndpointCreateRequest,
  EndpointUpdateRequest,
  EndpointValidateRequest,
  EndpointValidateResponse,
  EndpointSyncResponse,
  ConfigResponse,
  ListAnalyticsResponse,
  ListRequestsQuery,
  RequestsAggregateResponse,
  ModelUserUsageResponse,
  AuthResponse,
  LoginRequest,
  RegisterRequest,
  AuthSuccessResponse,
  RegistrationInfo,
  LoginInfo,
  PasswordResetRequest,
  PasswordResetConfirmRequest,
  ChangePasswordRequest,
  TransactionsQuery,
  Probe,
  CreateProbeRequest,
  ProbeResult,
  ProbeStatistics,
  FileObject,
  FileListResponse,
  FileDeleteResponse,
  FileUploadRequest,
  FilesListQuery,
  FileCostEstimate,
  Batch,
  BatchCreateRequest,
  BatchListResponse,
  BatchesListQuery,
  BatchAnalytics,
  TransactionsListResponse,
  AddFundsRequest,
  AddFundsResponse,
  DaemonsListResponse,
  DaemonsQuery,
  EndpointsQuery,
} from "./types";
import { ApiError } from "./errors";

// Optional override for AI API endpoints (files, batches, daemons)
// Falls back to same origin if not set (relative paths)
const AI_API_BASE_URL = import.meta.env.VITE_AI_API_BASE_URL || "";

// Helper to construct AI API URLs - strips /ai prefix when using override domain
const getAiApiUrl = (path: string): string => {
  if (AI_API_BASE_URL) {
    // When using api.doubleword.ai, strip /ai prefix because ingress adds it
    // Use lookahead to match /ai only when followed by / to avoid false matches
    return `${AI_API_BASE_URL}${path.replace(/^\/ai(?=\/)/, "")}`;
  }
  // When using same origin (app.doubleword.ai), keep /ai prefix
  return path;
};

// Helper to fetch AI API endpoints with proper credentials for cross-origin requests
const fetchAiApi = (path: string, init?: RequestInit): Promise<Response> => {
  const url = getAiApiUrl(path);
  // When making cross-origin requests, include credentials for session cookies
  const options: RequestInit = {
    ...init,
    credentials: AI_API_BASE_URL
      ? "include"
      : init?.credentials || "same-origin",
  };
  return fetch(url, options);
};

// Resource APIs
const userApi = {
  async list(options?: UsersQuery): Promise<PaginatedResponse<User>> {
    const params = new URLSearchParams();
    if (options?.include) {
      params.set("include", options.include);
    }
    if (options?.skip !== undefined) {
      params.set("skip", options.skip.toString());
    }
    if (options?.limit !== undefined) {
      params.set("limit", options.limit.toString());
    }

    const url = `/admin/api/v1/users${params.toString() ? "?" + params.toString() : ""}`;
    const response = await fetch(url);
    if (!response.ok) {
      throw new Error(`Failed to fetch users: ${response.status}`);
    }
    return response.json();
  },

  async get(id: string, options?: { include?: string }): Promise<User> {
    const params = new URLSearchParams();
    if (options?.include) {
      params.set("include", options.include);
    }
    const url = `/admin/api/v1/users/${id}${params.toString() ? "?" + params.toString() : ""}`;
    const response = await fetch(url);
    if (!response.ok) {
      throw new Error(`Failed to fetch user: ${response.status}`);
    }
    return response.json();
  },

  async create(data: UserCreateRequest): Promise<User> {
    const response = await fetch("/admin/api/v1/users", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(data),
    });
    if (!response.ok) {
      throw new Error(`Failed to create user: ${response.status}`);
    }
    return response.json();
  },

  async update(id: string, data: UserUpdateRequest): Promise<User> {
    const response = await fetch(`/admin/api/v1/users/${id}`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(data),
    });
    if (!response.ok) {
      throw new Error(`Failed to update user: ${response.status}`);
    }
    return response.json();
  },

  async delete(id: string): Promise<void> {
    const response = await fetch(`/admin/api/v1/users/${id}`, {
      method: "DELETE",
    });
    if (!response.ok) {
      throw new Error(`Failed to delete user: ${response.status}`);
    }
  },

  // Nested API keys under users
  apiKeys: {
    async getAll(
      userId: string = "current",
      options: { skip?: number; limit?: number } = {},
    ): Promise<PaginatedResponse<ApiKey>> {
      const params = new URLSearchParams();
      if (options.skip !== undefined) params.set("skip", String(options.skip));
      if (options.limit !== undefined)
        params.set("limit", String(options.limit));

      const queryString = params.toString();
      const url = `/admin/api/v1/users/${userId}/api-keys${queryString ? `?${queryString}` : ""}`;

      const response = await fetch(url);
      if (!response.ok) {
        throw new Error(`Failed to fetch API keys: ${response.status}`);
      }
      return response.json();
    },

    async get(id: string, userId: string = "current"): Promise<ApiKey> {
      const response = await fetch(
        `/admin/api/v1/users/${userId}/api-keys/${id}`,
      );
      if (!response.ok) {
        throw new Error(`Failed to fetch API key: ${response.status}`);
      }
      return response.json();
    },

    async create(
      data: ApiKeyCreateRequest,
      userId: string = "current",
    ): Promise<ApiKeyCreateResponse> {
      const response = await fetch(`/admin/api/v1/users/${userId}/api-keys`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(data),
      });
      if (!response.ok) {
        throw new Error(`Failed to create API key: ${response.status}`);
      }
      return response.json();
    },

    async delete(keyId: string, userId: string = "current"): Promise<void> {
      const response = await fetch(
        `/admin/api/v1/users/${userId}/api-keys/${keyId}`,
        {
          method: "DELETE",
        },
      );
      if (!response.ok) {
        throw new Error(`Failed to delete API key: ${response.status}`);
      }
    },
  },
};

const modelApi = {
  async list(options?: ModelsQuery): Promise<PaginatedResponse<Model>> {
    const params = new URLSearchParams();
    if (options?.skip !== undefined)
      params.set("skip", options.skip.toString());
    if (options?.limit !== undefined)
      params.set("limit", options.limit.toString());
    if (options?.endpoint) params.set("endpoint", options.endpoint);
    if (options?.include) params.set("include", options.include);
    if (options?.accessible !== undefined)
      params.set("accessible", options.accessible.toString());
    if (options?.search) params.set("search", options.search);

    const url = `/admin/api/v1/models${params.toString() ? "?" + params.toString() : ""}`;
    const response = await fetch(url);
    if (!response.ok) {
      throw new Error(`Failed to fetch models: ${response.status}`);
    }
    return response.json();
  },

  async get(id: string, options?: { include?: string }): Promise<Model> {
    const params = new URLSearchParams();
    if (options?.include) params.set("include", options.include);

    const url = `/admin/api/v1/models/${id}${params.toString() ? "?" + params.toString() : ""}`;
    const response = await fetch(url);
    if (!response.ok) {
      throw new Error(`Failed to fetch model: ${response.status}`);
    }
    return response.json();
  },

  async update(id: string, data: ModelUpdateRequest): Promise<Model> {
    const response = await fetch(`/admin/api/v1/models/${id}`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(data),
    });

    if (!response.ok) {
      if (response.status === 409) {
        // For 409 conflicts, try to get the actual server error message
        const errorText = await response.text();
        throw new Error(
          errorText || `Failed to update model: ${response.status}`,
        );
      }

      // Generic error for all other cases
      throw new Error(`Failed to update model: ${response.status}`);
    }

    return response.json();
  },
};

const endpointApi = {
  async list(options?: EndpointsQuery): Promise<Endpoint[]> {
    const params = new URLSearchParams();
    if (options?.skip !== undefined)
      params.set("skip", options.skip.toString());
    if (options?.limit !== undefined)
      params.set("limit", options.limit.toString());
    if (options?.enabled) params.set("enabled", options.enabled.toString());

    const url = `/admin/api/v1/endpoints${params.toString() ? "?" + params.toString() : ""}`;
    const response = await fetch(url);
    if (!response.ok) {
      throw new Error(`Failed to fetch endpoints: ${response.status}`);
    }
    return response.json();
  },

  async get(id: string): Promise<Endpoint> {
    const response = await fetch(`/admin/api/v1/endpoints/${id}`);
    if (!response.ok) {
      throw new Error(`Failed to fetch endpoint: ${response.status}`);
    }
    return response.json();
  },

  async validate(
    data: EndpointValidateRequest,
  ): Promise<EndpointValidateResponse> {
    const response = await fetch("/admin/api/v1/endpoints/validate", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(data),
    });
    if (!response.ok) {
      throw new Error(`Failed to validate endpoint: ${response.status}`);
    }
    return response.json();
  },

  async create(data: EndpointCreateRequest): Promise<Endpoint> {
    const response = await fetch("/admin/api/v1/endpoints", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(data),
    });

    if (!response.ok) {
      try {
        // Always try to get the response body first
        const responseText = await response.text();

        // Try to parse as JSON
        let responseData;
        try {
          responseData = JSON.parse(responseText);
        } catch {
          const error = new Error(
            responseText || `Failed to create endpoint: ${response.status}`,
          );
          (error as any).status = response.status;
          throw error;
        }

        // Create a structured error object
        const error = new Error(
          responseData.message ||
            `Failed to create endpoint: ${response.status}`,
        );
        (error as any).status = response.status;
        (error as any).response = {
          status: response.status,
          data: responseData,
        };
        (error as any).data = responseData; // Also add direct data property

        // Handle conflicts specifically
        if (response.status === 409) {
          if (responseData.conflicts) {
            (error as any).isConflict = true;
            (error as any).conflicts = responseData.conflicts;
          }
        }

        throw error;
      } catch (error) {
        // If it's already our custom error, re-throw it
        if (isApiErrorObject(error)) {
          throw error;
        }

        // Otherwise create a structured error
        const structuredError = new Error(
          error instanceof Error ? error.message : "Unknown error",
        );
        (structuredError as any).status = response.status;
        throw structuredError;
      }
    }

    return response.json();
  },

  async update(id: string, data: EndpointUpdateRequest): Promise<Endpoint> {
    const response = await fetch(`/admin/api/v1/endpoints/${id}`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(data),
    });

    if (!response.ok) {
      try {
        // Always try to get the response body first
        const responseText = await response.text();

        // Try to parse as JSON
        let responseData;
        try {
          responseData = JSON.parse(responseText);
        } catch {
          const error = new Error(
            responseText || `Failed to update endpoint: ${response.status}`,
          );
          (error as any).status = response.status;
          throw error;
        }

        // Create a structured error object that matches what your frontend expects
        const error = new Error(
          responseData.message ||
            `Failed to update endpoint: ${response.status}`,
        );
        (error as any).status = response.status;
        (error as any).response = {
          status: response.status,
          data: responseData,
        };
        (error as any).data = responseData; // Also add direct data property

        // Handle conflicts specifically
        if (response.status === 409) {
          if (responseData.conflicts) {
            (error as any).isConflict = true;
            (error as any).conflicts = responseData.conflicts;
          }
        }

        throw error;
      } catch (error) {
        // If it's already our custom error, re-throw it
        if (
          error &&
          typeof error === "object" &&
          ("status" in error || "isConflict" in error)
        ) {
          throw error;
        }

        // Otherwise create a structured error
        const structuredError = new Error(
          error instanceof Error ? error.message : "Unknown error",
        );
        (structuredError as any).status = response.status;
        throw structuredError;
      }
    }

    return response.json();
  },

  async synchronize(id: string): Promise<EndpointSyncResponse> {
    const response = await fetch(`/admin/api/v1/endpoints/${id}/synchronize`, {
      method: "POST",
    });

    if (!response.ok) {
      throw new Error(`Failed to synchronize endpoint: ${response.status}`);
    }

    return response.json();
  },

  async delete(id: string): Promise<void> {
    const response = await fetch(`/admin/api/v1/endpoints/${id}`, {
      method: "DELETE",
    });
    if (!response.ok) {
      throw new Error(`Failed to delete endpoint: ${response.status}`);
    }
  },
};

const groupApi = {
  async list(options?: GroupsQuery): Promise<PaginatedResponse<Group>> {
    const params = new URLSearchParams();
    if (options?.include) params.set("include", options.include);
    if (options?.skip !== undefined) {
      params.set("skip", options.skip.toString());
    }
    if (options?.limit !== undefined) {
      params.set("limit", options.limit.toString());
    }
    if (options?.search) params.set("search", options.search);

    const url = `/admin/api/v1/groups${params.toString() ? "?" + params.toString() : ""}`;
    const response = await fetch(url);
    if (!response.ok) {
      throw new Error(`Failed to fetch groups: ${response.status}`);
    }
    return response.json();
  },

  async get(id: string): Promise<Group> {
    const response = await fetch(`/admin/api/v1/groups/${id}`);
    if (!response.ok) {
      throw new Error(`Failed to fetch group: ${response.status}`);
    }
    return response.json();
  },

  async create(data: GroupCreateRequest): Promise<Group> {
    const response = await fetch("/admin/api/v1/groups", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(data),
    });
    if (!response.ok) {
      throw new Error(`Failed to create group: ${response.status}`);
    }
    return response.json();
  },

  async update(id: string, data: GroupUpdateRequest): Promise<Group> {
    const response = await fetch(`/admin/api/v1/groups/${id}`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(data),
    });
    if (!response.ok) {
      throw new Error(`Failed to update group: ${response.status}`);
    }
    return response.json();
  },

  async delete(id: string): Promise<void> {
    const response = await fetch(`/admin/api/v1/groups/${id}`, {
      method: "DELETE",
    });
    if (!response.ok) {
      throw new Error(`Failed to delete group: ${response.status}`);
    }
  },

  // Group relationship management
  async addUser(groupId: string, userId: string): Promise<void> {
    const response = await fetch(
      `/admin/api/v1/groups/${groupId}/users/${userId}`,
      {
        method: "POST",
      },
    );
    if (!response.ok) {
      throw new Error(`Failed to add user to group: ${response.status}`);
    }
  },

  async removeUser(groupId: string, userId: string): Promise<void> {
    const response = await fetch(
      `/admin/api/v1/groups/${groupId}/users/${userId}`,
      {
        method: "DELETE",
      },
    );
    if (!response.ok) {
      throw new Error(`Failed to remove user from group: ${response.status}`);
    }
  },

  async addModel(groupId: string, modelId: string): Promise<void> {
    const response = await fetch(
      `/admin/api/v1/groups/${groupId}/models/${modelId}`,
      {
        method: "POST",
      },
    );
    if (!response.ok) {
      throw new Error(`Failed to add group to model: ${response.status}`);
    }
  },

  async removeModel(groupId: string, modelId: string): Promise<void> {
    const response = await fetch(
      `/admin/api/v1/groups/${groupId}/models/${modelId}`,
      {
        method: "DELETE",
      },
    );
    if (!response.ok) {
      throw new Error(`Failed to remove group from model: ${response.status}`);
    }
  },
};

const configApi = {
  async get(): Promise<ConfigResponse> {
    const response = await fetch("/admin/api/v1/config");
    if (!response.ok) {
      throw new Error(`Failed to fetch config: ${response.status}`);
    }
    return response.json();
  },
};

const requestsApi = {
  async list(options?: ListRequestsQuery): Promise<ListAnalyticsResponse> {
    const params = new URLSearchParams();
    if (options?.limit !== undefined)
      params.set("limit", options.limit.toString());
    if (options?.skip !== undefined)
      params.set("skip", options.skip.toString());
    if (options?.method) params.set("method", options.method);
    if (options?.uri_pattern) params.set("uri_pattern", options.uri_pattern);
    if (options?.status_code !== undefined)
      params.set("status_code", options.status_code.toString());
    if (options?.status_code_min !== undefined)
      params.set("status_code_min", options.status_code_min.toString());
    if (options?.status_code_max !== undefined)
      params.set("status_code_max", options.status_code_max.toString());
    if (options?.min_duration_ms !== undefined)
      params.set("min_duration_ms", options.min_duration_ms.toString());
    if (options?.max_duration_ms !== undefined)
      params.set("max_duration_ms", options.max_duration_ms.toString());
    if (options?.timestamp_after)
      params.set("timestamp_after", options.timestamp_after);
    if (options?.timestamp_before)
      params.set("timestamp_before", options.timestamp_before);
    if (options?.order_desc !== undefined)
      params.set("order_desc", options.order_desc.toString());
    if (options?.model) params.set("model", options.model);
    if (options?.fusillade_batch_id)
      params.set("fusillade_batch_id", options.fusillade_batch_id);
    if (options?.custom_id) params.set("custom_id", options.custom_id);

    const url = `/admin/api/v1/requests${params.toString() ? "?" + params.toString() : ""}`;
    const response = await fetch(url);
    if (!response.ok) {
      throw new Error(`Failed to fetch requests: ${response.status}`);
    }
    return response.json();
  },

  async aggregate(
    model?: string,
    timestampAfter?: string,
    timestampBefore?: string,
  ): Promise<RequestsAggregateResponse> {
    const params = new URLSearchParams();
    if (model) params.set("model", model);
    if (timestampAfter) params.set("timestamp_after", timestampAfter);
    if (timestampBefore) params.set("timestamp_before", timestampBefore);

    const url = `/admin/api/v1/requests/aggregate${params.toString() ? "?" + params.toString() : ""}`;
    const response = await fetch(url);
    if (!response.ok) {
      throw new Error(`Failed to fetch request analytics: ${response.status}`);
    }
    return response.json();
  },

  async aggregateByUser(
    model?: string,
    startDate?: string,
    endDate?: string,
  ): Promise<ModelUserUsageResponse> {
    const params = new URLSearchParams();
    if (model) params.set("model", model);
    if (startDate) params.set("start_date", startDate);
    if (endDate) params.set("end_date", endDate);

    const url = `/admin/api/v1/requests/aggregate-by-user${params.toString() ? "?" + params.toString() : ""}`;
    const response = await fetch(url);
    if (!response.ok) {
      throw new Error(`Failed to fetch user usage data: ${response.status}`);
    }
    return response.json();
  },
};

const authApi = {
  async getRegistrationInfo(): Promise<RegistrationInfo> {
    const response = await fetch("/authentication/register", {
      method: "GET",
      credentials: "include",
    });
    if (!response.ok) {
      throw new Error(`Failed to get registration info: ${response.status}`);
    }
    return response.json();
  },

  async getLoginInfo(): Promise<LoginInfo> {
    const response = await fetch("/authentication/login", {
      method: "GET",
      credentials: "include",
    });
    if (!response.ok) {
      throw new Error(`Failed to get login info: ${response.status}`);
    }
    return response.json();
  },

  async login(credentials: LoginRequest): Promise<AuthResponse> {
    const response = await fetch("/authentication/login", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(credentials),
      credentials: "include", // Include cookies in request
    });
    if (!response.ok) {
      const errorMessage = await response.text();
      throw new ApiError(
        response.status,
        errorMessage || `Login failed: ${response.status}`,
        response,
      );
    }
    return response.json();
  },

  async register(credentials: RegisterRequest): Promise<AuthResponse> {
    const response = await fetch("/authentication/register", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(credentials),
      credentials: "include", // Include cookies in request
    });
    if (!response.ok) {
      const errorMessage = await response.text();
      throw new ApiError(
        response.status,
        errorMessage || `Registration failed: ${response.status}`,
        response,
      );
    }
    return response.json();
  },

  async logout(): Promise<AuthSuccessResponse> {
    const response = await fetch("/authentication/logout", {
      method: "POST",
      credentials: "include", // Include cookies in request
    });
    if (!response.ok) {
      throw new Error(`Logout failed: ${response.status}`);
    }
    return response.json();
  },

  async requestPasswordReset(
    request: PasswordResetRequest,
  ): Promise<AuthSuccessResponse> {
    const response = await fetch("/authentication/password-resets", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(request),
      credentials: "include",
    });
    if (!response.ok) {
      const errorMessage = await response.text();
      throw new ApiError(
        response.status,
        errorMessage || `Password reset request failed: ${response.status}`,
        response,
      );
    }
    return response.json();
  },

  async confirmPasswordReset(
    request: PasswordResetConfirmRequest,
  ): Promise<AuthSuccessResponse> {
    const response = await fetch(
      `/authentication/password-resets/${request.token_id}/confirm`,
      {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          token: request.token,
          new_password: request.new_password,
        }),
        credentials: "include",
      },
    );
    if (!response.ok) {
      const errorMessage = await response.text();
      throw new ApiError(
        response.status,
        errorMessage ||
          `Password reset confirmation failed: ${response.status}`,
        response,
      );
    }
    return response.json();
  },

  async changePassword(
    request: ChangePasswordRequest,
  ): Promise<AuthSuccessResponse> {
    const response = await fetch("/authentication/password-change", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(request),
      credentials: "include", // Include cookies for authentication
    });
    if (!response.ok) {
      const errorMessage = await response.text();
      throw new ApiError(
        response.status,
        errorMessage || `Password change failed: ${response.status}`,
        response,
      );
    }
    return response.json();
  },
};

// Cost management API
const costApi = {
  async listTransactions(
    query?: TransactionsQuery,
  ): Promise<TransactionsListResponse> {
    const params = new URLSearchParams();
    if (query?.limit) params.set("limit", query.limit.toString());
    if (query?.skip) params.set("skip", query.skip.toString());
    if (query?.userId) params.set("user_id", query.userId);
    if (query?.group_batches !== undefined) params.set("group_batches", query.group_batches.toString());
    if (query?.start_date) params.set("start_date", query.start_date);
    if (query?.end_date) params.set("end_date", query.end_date);

    const url = `/admin/api/v1/transactions${params.toString() ? "?" + params.toString() : ""}`;
    const response = await fetch(url);
    if (!response.ok) {
      throw new Error(`Failed to fetch transactions: ${response.status}`);
    }
    return response.json();
  },

  async addFunds(data: AddFundsRequest): Promise<AddFundsResponse> {
    const payload = {
      user_id: data.user_id,
      transaction_type: "admin_grant",
      amount: data.amount,
      source_id: data.source_id,
      description: data.description,
    };

    const response = await fetch("/admin/api/v1/transactions", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload),
    });
    if (!response.ok) {
      throw new Error(`Failed to add funds: ${response.status}`);
    }
    return response.json();
  },
};

// Payment processing API
const paymentsApi = {
  async create(crediteeId?: string): Promise<{ url: string }> {
    const params = new URLSearchParams();
    if (crediteeId) {
      params.set("creditee_id", crediteeId);
    }

    const url = `/admin/api/v1/payments${params.toString() ? "?" + params.toString() : ""}`;
    const response = await fetch(url, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
    });

    if (!response.ok) {
      const errorData = await response.json().catch(() => ({}));
      throw new Error(
        errorData.message || `Failed to create payment: ${response.status}`,
      );
    }

    return response.json();
  },

  async process(paymentId: string): Promise<void> {
    const response = await fetch(`/admin/api/v1/payments/${paymentId}`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
    });

    if (!response.ok) {
      if (response.status === 402) {
        throw new ApiError(
          402,
          "Payment is still processing. Please check back in a moment.",
          response,
        );
      }
      throw new Error(`Failed to process transaction: ${response.status}`);
    }

    // Explicitly return to ensure promise resolves
    return;
  },
};

// Probes API
const probesApi = {
  async list(status?: string): Promise<Probe[]> {
    const params = new URLSearchParams();
    if (status) params.set("status", status);

    const url = `/admin/api/v1/probes${params.toString() ? "?" + params.toString() : ""}`;
    const response = await fetch(url);
    if (!response.ok) {
      throw new Error(`Failed to fetch probes: ${response.status}`);
    }
    return response.json();
  },

  async get(id: string): Promise<Probe> {
    const response = await fetch(`/admin/api/v1/probes/${id}`);
    if (!response.ok) {
      throw new Error(`Failed to fetch probe: ${response.status}`);
    }
    return response.json();
  },

  async create(data: CreateProbeRequest): Promise<Probe> {
    const response = await fetch("/admin/api/v1/probes", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(data),
    });
    if (!response.ok) {
      throw new Error(`Failed to create probe: ${response.status}`);
    }
    return response.json();
  },

  async delete(id: string): Promise<void> {
    const response = await fetch(`/admin/api/v1/probes/${id}`, {
      method: "DELETE",
    });
    if (!response.ok) {
      throw new Error(`Failed to delete probe: ${response.status}`);
    }
  },

  async activate(id: string): Promise<Probe> {
    const response = await fetch(`/admin/api/v1/probes/${id}/activate`, {
      method: "PATCH",
    });
    if (!response.ok) {
      throw new Error(`Failed to activate probe: ${response.status}`);
    }
    return response.json();
  },

  async deactivate(id: string): Promise<Probe> {
    const response = await fetch(`/admin/api/v1/probes/${id}/deactivate`, {
      method: "PATCH",
    });
    if (!response.ok) {
      throw new Error(`Failed to deactivate probe: ${response.status}`);
    }
    return response.json();
  },

  async update(
    id: string,
    data: { interval_seconds?: number },
  ): Promise<Probe> {
    const response = await fetch(`/admin/api/v1/probes/${id}`, {
      method: "PATCH",
      headers: {
        "Content-Type": "application/json",
      },
      body: JSON.stringify(data),
    });
    if (!response.ok) {
      throw new Error(`Failed to update probe: ${response.status}`);
    }
    return response.json();
  },

  async execute(id: string): Promise<ProbeResult> {
    const response = await fetch(`/admin/api/v1/probes/${id}/execute`, {
      method: "POST",
    });
    if (!response.ok) {
      throw new Error(`Failed to execute probe: ${response.status}`);
    }
    return response.json();
  },

  async test(
    deploymentId: string,
    params?: {
      http_method?: string;
      request_path?: string;
      request_body?: Record<string, unknown>;
    },
  ): Promise<ProbeResult> {
    const response = await fetch(`/admin/api/v1/probes/test/${deploymentId}`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
      },
      body: JSON.stringify(params || null),
    });
    if (!response.ok) {
      throw new Error(`Failed to test probe: ${response.status}`);
    }
    return response.json();
  },

  async getResults(
    id: string,
    params?: { start_time?: string; end_time?: string; limit?: number },
  ): Promise<ProbeResult[]> {
    const queryParams = new URLSearchParams();
    if (params?.start_time) queryParams.set("start_time", params.start_time);
    if (params?.end_time) queryParams.set("end_time", params.end_time);
    if (params?.limit) queryParams.set("limit", params.limit.toString());

    const url = `/admin/api/v1/probes/${id}/results${queryParams.toString() ? "?" + queryParams.toString() : ""}`;
    const response = await fetch(url);
    if (!response.ok) {
      throw new Error(`Failed to fetch probe results: ${response.status}`);
    }
    return response.json();
  },

  async getStatistics(
    id: string,
    params?: { start_time?: string; end_time?: string },
  ): Promise<ProbeStatistics> {
    const queryParams = new URLSearchParams();
    if (params?.start_time) queryParams.set("start_time", params.start_time);
    if (params?.end_time) queryParams.set("end_time", params.end_time);

    const url = `/admin/api/v1/probes/${id}/statistics${queryParams.toString() ? "?" + queryParams.toString() : ""}`;
    const response = await fetch(url);
    if (!response.ok) {
      throw new Error(`Failed to fetch probe statistics: ${response.status}`);
    }
    return response.json();
  },
};

function isApiErrorObject(
  error: unknown,
): error is { status?: number; isConflict?: boolean } {
  return (
    typeof error === "object" &&
    error !== null &&
    ("status" in error || "isConflict" in error)
  );
}

// Add these new API sections to the dwctlApi object at the bottom

const filesApi = {
  async list(options?: FilesListQuery): Promise<FileListResponse> {
    const params = new URLSearchParams();
    if (options?.after) params.set("after", options.after);
    if (options?.limit) params.set("limit", options.limit.toString());
    if (options?.order) params.set("order", options.order);
    if (options?.purpose) params.set("purpose", options.purpose);
    if (options?.search) params.set("search", options.search);

    const response = await fetchAiApi(
      `/ai/v1/files${params.toString() ? "?" + params.toString() : ""}`,
    );
    if (!response.ok) {
      throw new Error(`Failed to fetch files: ${response.status}`);
    }
    return response.json();
  },

  async get(id: string): Promise<FileObject> {
    const response = await fetchAiApi(`/ai/v1/files/${id}`);
    if (!response.ok) {
      throw new Error(`Failed to fetch file: ${response.status}`);
    }
    return response.json();
  },

  async upload(data: FileUploadRequest): Promise<FileObject> {
    const formData = new FormData();
    formData.append("file", data.file);
    formData.append("purpose", data.purpose);

    if (data.expires_after) {
      formData.append("expires_after[anchor]", data.expires_after.anchor);
      formData.append(
        "expires_after[seconds]",
        data.expires_after.seconds.toString(),
      );
    }

    const response = await fetchAiApi("/ai/v1/files", {
      method: "POST",
      body: formData,
    });

    if (!response.ok) {
      const errorText = await response.text();
      throw new ApiError(
        response.status,
        errorText || `Failed to upload file: ${response.status}`,
        response,
      );
    }
    return response.json();
  },

  async uploadWithProgress(
    data: FileUploadRequest,
    onProgress?: (percent: number) => void,
  ): Promise<FileObject> {
    return new Promise((resolve, reject) => {
      const xhr = new XMLHttpRequest();
      const formData = new FormData();
      formData.append("file", data.file, data.filename || data.file.name);
      formData.append("purpose", data.purpose);

      if (data.expires_after) {
        formData.append("expires_after[anchor]", data.expires_after.anchor);
        formData.append(
          "expires_after[seconds]",
          data.expires_after.seconds.toString(),
        );
      }

      xhr.upload.onprogress = (event) => {
        if (event.lengthComputable && onProgress) {
          onProgress(Math.round((event.loaded / event.total) * 100));
        }
      };

      xhr.onload = () => {
        if (xhr.status >= 200 && xhr.status < 300) {
          try {
            resolve(JSON.parse(xhr.responseText));
          } catch {
            reject(new Error("Invalid JSON response from server"));
          }
        } else {
          reject(
            new ApiError(
              xhr.status,
              xhr.responseText || `Failed to upload file: ${xhr.status}`,
            ),
          );
        }
      };

      xhr.onerror = () => reject(new Error("Network error during upload"));
      xhr.onabort = () => reject(new Error("Upload aborted"));

      const url = getAiApiUrl("/ai/v1/files");
      xhr.open("POST", url);
      xhr.withCredentials = !!AI_API_BASE_URL;
      xhr.send(formData);
    });
  },

  async delete(id: string): Promise<FileDeleteResponse> {
    const response = await fetchAiApi(`/ai/v1/files/${id}`, {
      method: "DELETE",
    });
    if (!response.ok) {
      throw new Error(`Failed to delete file: ${response.status}`);
    }
    return response.json();
  },

  // Get file content as JSONL (supports limit/offset query params)
  // Returns content, whether there are more results, and the last line number
  async getFileContent(
    id: string,
    options?: { limit?: number; skip?: number; search?: string },
  ): Promise<{ content: string; incomplete: boolean; lastLine: number }> {
    const params = new URLSearchParams();
    if (options?.limit) params.set("limit", options.limit.toString());
    if (options?.skip) params.set("skip", options.skip.toString());
    if (options?.search) params.set("search", options.search);

    const response = await fetchAiApi(
      `/ai/v1/files/${id}/content${params.toString() ? "?" + params.toString() : ""}`,
    );
    if (!response.ok) {
      throw new Error(`Failed to fetch file content: ${response.status}`);
    }

    const content = await response.text();
    const incomplete = response.headers.get("X-Incomplete") === "true";
    const lastLine = parseInt(response.headers.get("X-Last-Line") || "0", 10);

    return { content, incomplete, lastLine };
  },

  async getCostEstimate(
    id: string,
    completionWindow?: string,
  ): Promise<FileCostEstimate> {
    const params = new URLSearchParams();
    if (completionWindow) {
      params.set("completion_window", completionWindow);
    }

    const response = await fetchAiApi(
      `/ai/v1/files/${id}/cost-estimate${params.toString() ? "?" + params.toString() : ""}`,
    );
    if (!response.ok) {
      throw new Error(`Failed to fetch file cost estimate: ${response.status}`);
    }
    return response.json();
  },
};

const batchesApi = {
  async list(options?: BatchesListQuery): Promise<BatchListResponse> {
    const params = new URLSearchParams();
    if (options?.after) params.set("after", options.after);
    if (options?.limit) params.set("limit", options.limit.toString());
    if (options?.search) params.set("search", options.search);

    const response = await fetchAiApi(
      `/ai/v1/batches${params.toString() ? "?" + params.toString() : ""}`,
    );
    if (!response.ok) {
      throw new Error(`Failed to fetch batches: ${response.status}`);
    }
    return response.json();
  },

  async get(id: string): Promise<Batch> {
    const response = await fetchAiApi(`/ai/v1/batches/${id}`);
    if (!response.ok) {
      throw new Error(`Failed to fetch batch: ${response.status}`);
    }
    return response.json();
  },

  async create(data: BatchCreateRequest): Promise<Batch> {
    const response = await fetchAiApi("/ai/v1/batches", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(data),
    });

    if (!response.ok) {
      const errorText = await response.text();
      throw new ApiError(
        response.status,
        errorText || `Failed to create batch: ${response.status}`,
        response,
      );
    }
    return response.json();
  },

  async cancel(id: string): Promise<Batch> {
    const response = await fetchAiApi(`/ai/v1/batches/${id}/cancel`, {
      method: "POST",
    });
    if (!response.ok) {
      throw new Error(`Failed to cancel batch: ${response.status}`);
    }
    return response.json();
  },

  async delete(id: string): Promise<void> {
    const response = await fetchAiApi(`/ai/v1/batches/${id}`, {
      method: "DELETE",
    });
    if (!response.ok) {
      throw new Error(`Failed to delete batch: ${response.status}`);
    }
  },

  async retry(id: string): Promise<Batch> {
    const response = await fetchAiApi(`/ai/v1/batches/${id}/retry`, {
      method: "POST",
    });
    if (!response.ok) {
      throw new Error(`Failed to retry batch: ${response.status}`);
    }
    return response.json();
  },

  async retryRequests(id: string, requestIds: string[]): Promise<Batch> {
    const response = await fetchAiApi(`/ai/v1/batches/${id}/retry-requests`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ request_ids: requestIds }),
    });
    if (!response.ok) {
      throw new Error(`Failed to retry requests: ${response.status}`);
    }
    return response.json();
  },

  async getAnalytics(id: string): Promise<BatchAnalytics> {
    const response = await fetchAiApi(`/ai/v1/batches/${id}/analytics`);
    if (!response.ok) {
      throw new Error(`Failed to fetch batch analytics: ${response.status}`);
    }
    return response.json();
  },

  async getBatchResults(
    id: string,
    options?: { limit?: number; skip?: number; search?: string },
  ): Promise<{ content: string; incomplete: boolean; lastLine: number }> {
    const params = new URLSearchParams();
    if (options?.limit) params.set("limit", options.limit.toString());
    if (options?.skip) params.set("skip", options.skip.toString());
    if (options?.search) params.set("search", options.search);

    const response = await fetchAiApi(
      `/ai/v1/batches/${id}/results${params.toString() ? "?" + params.toString() : ""}`,
    );
    if (!response.ok) {
      throw new Error(`Failed to fetch batch results: ${response.status}`);
    }

    const content = await response.text();
    const incomplete = response.headers.get("X-Incomplete") === "true";
    const lastLine = parseInt(response.headers.get("X-Last-Line") || "0", 10);

    return { content, incomplete, lastLine };
  },

  // Download batch results via the output file
  async downloadResults(id: string): Promise<Blob> {
    // First get the batch to find the output_file_id
    const batch = await this.get(id);

    if (!batch.output_file_id) {
      throw new Error(`Batch ${id} does not have output file yet`);
    }

    // Download the output file content
    const response = await fetchAiApi(
      `/ai/v1/files/${batch.output_file_id}/content`,
    );
    if (!response.ok) {
      throw new Error(`Failed to download batch results: ${response.status}`);
    }
    return response.blob();
  },
};

const daemonsApi = {
  async list(options?: DaemonsQuery): Promise<DaemonsListResponse> {
    const params = new URLSearchParams();
    if (options?.status) params.set("status", options.status);

    const response = await fetchAiApi(
      `/ai/v1/daemons${params.toString() ? "?" + params.toString() : ""}`,
    );
    if (!response.ok) {
      throw new Error(`Failed to fetch daemons: ${response.status}`);
    }
    return response.json();
  },
};

// Main nested API object
export const dwctlApi = {
  users: userApi,
  models: modelApi,
  endpoints: endpointApi,
  groups: groupApi,
  config: configApi,
  requests: requestsApi,
  auth: authApi,
  cost: costApi,
  payments: paymentsApi,
  probes: probesApi,
  files: filesApi,
  batches: batchesApi,
  daemons: daemonsApi,
};
