/// A client implementation for the `clay` backend API.
import type {
  Model,
  Endpoint,
  Group,
  User,
  ApiKey,
  ApiKeyCreateResponse,
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
  ListRequestsResponse,
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
} from "./types";
import { ApiError } from "./errors";

// Resource APIs
const userApi = {
  async list(options?: UsersQuery): Promise<User[]> {
    const params = new URLSearchParams();
    if (options?.include) params.set("include", options.include);

    const url = `/admin/api/v1/users${params.toString() ? "?" + params.toString() : ""}`;
    const response = await fetch(url);
    if (!response.ok) {
      throw new Error(`Failed to fetch users: ${response.status}`);
    }
    return response.json();
  },

  async get(id: string): Promise<User> {
    const response = await fetch(`/admin/api/v1/users/${id}`);
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
    async getAll(userId: string = "current"): Promise<ApiKey[]> {
      const response = await fetch(`/admin/api/v1/users/${userId}/api-keys`);
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
  async list(options?: ModelsQuery): Promise<Model[]> {
    const params = new URLSearchParams();
    if (options?.endpoint) params.set("endpoint", options.endpoint);
    if (options?.include) params.set("include", options.include);
    if (options?.accessible !== undefined)
      params.set("accessible", options.accessible.toString());

    const url = `/admin/api/v1/models${params.toString() ? "?" + params.toString() : ""}`;
    const response = await fetch(url);
    if (!response.ok) {
      throw new Error(`Failed to fetch models: ${response.status}`);
    }
    return response.json();
  },

  async get(id: string): Promise<Model> {
    const response = await fetch(`/admin/api/v1/models/${id}`);
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
        throw new Error(errorText || `Failed to hello model: ${response.status}`);
      }
      
      // Generic error for all other cases
      throw new Error(`Failed to update model: ${response.status}`);
    }
    
    return response.json();
  },
};

const endpointApi = {
  async list(): Promise<Endpoint[]> {
    const response = await fetch("/admin/api/v1/endpoints");
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
      console.log(`❌ Error response (${response.status}):`, response.statusText);
      
      try {
        // Always try to get the response body first
        const responseText = await response.text();
        
        // Try to parse as JSON
        let responseData;
        try {
          responseData = JSON.parse(responseText);
        } catch (parseError) {
          throw new Error(responseText || `Failed to create endpoint: ${response.status}`);
        }
        
        // Handle different status codes
        if (response.status === 409) {
          if (responseData.conflicts) {
            const conflictError = new Error(responseData.message || "Alias conflicts detected");
            (conflictError as any).isConflict = true;
            (conflictError as any).conflicts = responseData.conflicts;
            throw conflictError;
          }
        }
        
        // Generic error with JSON message
        throw new Error(responseData.message || responseText || `Failed to create endpoint: ${response.status}`);
        
      } catch (error) {
        
        // If it's already our custom error, re-throw it
        if (error && typeof error === 'object' && 'isConflict' in error) {
          throw error;
        }
        
        // Otherwise throw the error we caught
        throw error;
      }
    }
    
    console.log("✅ Success response, parsing JSON...");
    return response.json();
  },

  async update(id: string, data: EndpointUpdateRequest): Promise<Endpoint> {
    const response = await fetch(`/admin/api/v1/endpoints/${id}`, {
      method: "PATCH",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(data),
    });
    if (!response.ok) {
      throw new Error(`Failed to update endpoint: ${response.status}`);
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
  async synchronize(id: string): Promise<EndpointSyncResponse> {
    const response = await fetch(`/admin/api/v1/endpoints/${id}/synchronize`, {
      method: "POST",
    });
    if (!response.ok) {
      throw new Error(`Failed to synchronize endpoint: ${response.status}`);
    }
    return response.json();
  },
};

const groupApi = {
  async list(options?: GroupsQuery): Promise<Group[]> {
    const params = new URLSearchParams();
    if (options?.include) params.set("include", options.include);

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
  async list(options?: ListRequestsQuery): Promise<ListRequestsResponse> {
    const params = new URLSearchParams();
    if (options?.limit !== undefined)
      params.set("limit", options.limit.toString());
    if (options?.offset !== undefined)
      params.set("offset", options.offset.toString());
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
};

// Main nested API object
export const clayApi = {
  users: userApi,
  models: modelApi,
  endpoints: endpointApi,
  groups: groupApi,
  config: configApi,
  requests: requestsApi,
  auth: authApi,
};
