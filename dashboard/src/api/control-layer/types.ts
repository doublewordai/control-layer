import { z } from "zod";

export type ModelType = "CHAT" | "EMBEDDINGS" | "RERANKER";
export type AuthSource = "vouch" | "native" | "system" | "proxy-header";
export type Role = "PlatformManager" | "RequestViewer" | "StandardUser";

// Config/Metadata types
export interface ConfigResponse {
  region: string;
  organization: string;
}

// Model metrics time series point
export interface ModelTimeSeriesPoint {
  timestamp: string;
  requests: number;
}

// Model metrics (only present when include=metrics)
export interface ModelMetrics {
  avg_latency_ms?: number;
  total_requests: number;
  total_input_tokens: number;
  total_output_tokens: number;
  last_active_at?: string; // ISO 8601 timestamp
  time_series?: ModelTimeSeriesPoint[]; // Recent activity for sparklines
}

// Model probe status (only present when include=status)
export interface ModelProbeStatus {
  probe_id?: string;
  active: boolean;
  interval_seconds?: number;
  last_check?: string; // ISO 8601 timestamp
  last_success?: boolean;
  uptime_percentage?: number; // Last 24h uptime
}

// Base model types
export interface Model {
  id: string;
  alias: string;
  model_name: string;
  description?: string | null;
  model_type?: ModelType | null;
  capabilities?: string[] | null;
  hosted_on: string; // endpoint ID (UUID)
  requests_per_second?: number | null; // Global rate limiting: requests per second
  burst_size?: number | null; // Global rate limiting: burst capacity
  groups?: Group[]; // array of group IDs - only present when include=groups
  metrics?: ModelMetrics; // only present when include=metrics
  status?: ModelProbeStatus; // only present when include=status
}

export interface Endpoint {
  id: string; // UUID
  name: string;
  description?: string;
  url: string;
  created_by: string;
  created_at: string; // ISO 8601 timestamp
  updated_at: string; // ISO 8601 timestamp
  requires_api_key: boolean; // Whether this endpoint requires an API key
  model_filter?: string[] | null; // Optional list of models to sync
}

export interface EndpointSyncResponse {
  endpoint_id: string; // UUID
  changes_made: number;
  new_models_created: number;
  models_reactivated: number;
  models_deactivated: number;
  models_deleted: number;
  total_models_fetched: number;
  filtered_models_count: number;
  synced_at: string; // ISO 8601 timestamp
}

export interface Group {
  id: string;
  name: string;
  description?: string;
  created_by?: string;
  created_at?: string; // ISO 8601 timestamp
  updated_at?: string; // ISO 8601 timestamp
  users?: User[]; // List of IDs, only present when include contains 'users'
  models?: Model[]; // List of IDs, only present when include contains 'models'
  source: string;
}

export interface User {
  id: string;
  username: string;
  email: string;
  display_name?: string;
  avatar_url?: string;
  is_admin?: boolean;
  roles: Role[];
  groups?: Group[]; // only present when include=groups
  created_at: string; // ISO 8601 timestamp
  updated_at: string; // ISO 8601 timestamp
  auth_source: AuthSource;
}

export interface ApiKey {
  id: string;
  name: string;
  description?: string;
  created_at: string; // ISO 8601 timestamp
  last_used?: string; // ISO 8601 timestamp
  requests_per_second?: number | null; // Rate limiting: requests per second
  burst_size?: number | null; // Rate limiting: burst capacity
  // Note: actual key value only returned on creation
}

// Response type for API key creation (includes the actual key)
export interface ApiKeyCreateResponse extends ApiKey {
  key: string; // The actual API key - only returned on creation
}

// Request payload types for CRUD operations Certain endpoints can have query
// parameters that trigger additional data returns. For example, GET
// /admin/api/v1/groups?include=users,models will return user ids and model ids
// in each element of the groups response. Note that this is only the id; and
// we need to make another query for the actual data.
export type ModelsInclude =
  | "groups"
  | "metrics"
  | "status"
  | "groups,metrics"
  | "groups,status"
  | "metrics,status"
  | "groups,metrics,status";
export type GroupsInclude = "users" | "models" | "users,models";
export type UsersInclude = "groups";

// List endpoint query parameters
export interface ModelsQuery {
  endpoint?: string;
  include?: ModelsInclude;
  accessible?: boolean; // Filter to only models the current user can access
}

export interface EndpointsQuery {
  skip?: number;
  limit?: number;
}

export interface GroupsQuery {
  skip?: number;
  limit?: number;
  include?: GroupsInclude;
}

export interface UsersQuery {
  skip?: number;
  limit?: number;
  include?: UsersInclude;
}

// Create endpoint bodies
// Missing model & endpoint, since both of those are created by the system for now
export interface UserCreateRequest {
  username: string;
  email: string;
  display_name?: string;
  avatar_url?: string;
  roles: Role[];
}

export interface GroupCreateRequest {
  name: string;
  description?: string;
}

export interface ApiKeyCreateRequest {
  name: string;
  description?: string;
  requests_per_second?: number | null;
  burst_size?: number | null;
}

// Update endpoint bodies
export interface UserUpdateRequest {
  display_name?: string;
  avatar_url?: string;
  roles?: Role[];
}

export interface GroupUpdateRequest {
  name?: string;
  description?: string;
}

export interface ModelUpdateRequest {
  alias?: string;
  description?: string | null;
  model_type?: ModelType | null;
  capabilities?: string[] | null;
  requests_per_second?: number | null;
  burst_size?: number | null;
}

// Endpoint-specific types
export interface EndpointCreateRequest {
  name: string;
  description?: string;
  url: string;
  api_key?: string;
  model_filter?: string[]; // Array of model IDs to sync, or null for all models
  alias_mapping?: Record<string, string>; // model_name -> custom_alias
  auth_header_name?: string; // Header name for authorization (defaults to "Authorization")
  auth_header_prefix?: string; // Prefix for authorization header value (defaults to "Bearer ")
  sync?: boolean; // Whether to sync models during creation (defaults to true)
  skip_fetch?: boolean; // Create deployments directly from model_filter without fetching (defaults to false)
}

export interface EndpointUpdateRequest {
  name?: string;
  description?: string;
  url?: string;
  api_key?: string | null;
  model_filter?: string[] | null;
  alias_mapping?: Record<string, string>;
  auth_header_name?: string;
  auth_header_prefix?: string;
}

export type EndpointValidateRequest =
  | {
      type: "new";
      url: string;
      api_key?: string;
      auth_header_name?: string;
      auth_header_prefix?: string;
    }
  | {
      type: "existing";
      endpoint_id: string; // UUID
    };

export interface AvailableModel {
  id: string;
  created: number; // Unix timestamp
  object: "model"; // Literal type matching OpenAI API
  owned_by: string;
}

export interface AvailableModelsResponse {
  object: "list";
  data: AvailableModel[];
}

export interface EndpointValidateResponse {
  status: "success" | "error";
  models?: AvailableModelsResponse;
  error?: string;
}

// ===== REQUESTS/TRAFFIC MONITORING TYPES =====

// Backend HTTP request/response types matching Control Layer API
export interface HttpRequest {
  id: number;
  timestamp: string;
  method: string;
  uri: string;
  headers: Record<string, any>;
  body?: AiRequest;
  created_at: string;
}

export interface HttpResponse {
  id: number;
  timestamp: string;
  status_code: number;
  headers: Record<string, any>;
  body?: AiResponse;
  duration_ms: number;
  created_at: string;
}

export interface RequestResponsePair {
  request: HttpRequest;
  response?: HttpResponse;
}

export interface ListRequestsResponse {
  requests: RequestResponsePair[];
}

// AI request/response types (matching Control Layer's tagged ApiAiRequest/ApiAiResponse enums)
// Now properly tagged for easy discrimination
export type AiRequest =
  | { type: "chat_completions"; data: ChatCompletionRequest }
  | { type: "completions"; data: CompletionRequest }
  | { type: "embeddings"; data: EmbeddingRequest }
  | { type: "rerank"; data: RerankRequest }
  | { type: "other"; data: any };

export type AiResponse =
  | { type: "chat_completions"; data: ChatCompletionResponse }
  | { type: "chat_completions_stream"; data: ChatCompletionChunk[] }
  | { type: "completions"; data: CompletionResponse }
  | { type: "embeddings"; data: EmbeddingResponse }
  | { type: "rerank"; data: RerankResponse }
  | { type: "other"; data: any };

// OpenAI-compatible request/response types
export interface ChatCompletionMessage {
  role: "system" | "user" | "assistant";
  content: string;
}

export interface ChatCompletionRequest {
  model: string;
  messages: ChatCompletionMessage[];
  temperature?: number;
  max_completion_tokens?: number;
  stream?: boolean;
}

export interface ChatCompletionResponse {
  id: string;
  object: string;
  created: number;
  model: string;
  choices: {
    index: number;
    message: ChatCompletionMessage;
    finish_reason: string;
  }[];
  usage?: {
    prompt_tokens: number;
    completion_tokens: number;
    total_tokens: number;
  };
}

export interface ChatCompletionChunk {
  id: string;
  object: string;
  created: number;
  model: string;
  choices: {
    index: number;
    delta: Partial<ChatCompletionMessage>;
    finish_reason?: string;
  }[];
  usage?: {
    prompt_tokens: number;
    completion_tokens: number;
    total_tokens: number;
  };
}

export interface CompletionRequest {
  model: string;
  prompt: string;
  temperature?: number;
  max_tokens?: number;
}

export interface CompletionResponse {
  id: string;
  object: string;
  created: number;
  model: string;
  choices: {
    index: number;
    text: string;
    finish_reason: string;
  }[];
  usage?: {
    prompt_tokens: number;
    completion_tokens: number;
    total_tokens: number;
  };
}

export interface EmbeddingRequest {
  model: string;
  input: string | string[];
}

export interface EmbeddingResponse {
  object: string;
  data: {
    index: number;
    embedding: number[];
  }[];
  model: string;
  usage: {
    prompt_tokens: number;
    total_tokens: number;
  };
}

export interface RerankRequest {
  model: string;
  query: string;
  documents: string[];
}

export interface RerankResponse {
  id: string;
  model: string;
  usage: {
    total_tokens: number;
  };
  results: {
    index: number;
    document: {
      text: string;
      multi_modal: any | null;
    };
    relevance_score: number;
  }[];
}

// Query parameters for backend API
export interface ListRequestsQuery {
  limit?: number;
  offset?: number;
  method?: string;
  uri_pattern?: string;
  status_code?: number;
  status_code_min?: number;
  status_code_max?: number;
  min_duration_ms?: number;
  max_duration_ms?: number;
  timestamp_after?: string;
  timestamp_before?: string;
  order_desc?: boolean;
}

// Validation schemas
export const listRequestsQuerySchema = z.object({
  limit: z.number().min(1).max(1000).optional(),
  offset: z.number().min(0).optional(),
  method: z.string().optional(),
  uri_pattern: z.string().optional(),
  status_code: z.number().optional(),
  status_code_min: z.number().optional(),
  status_code_max: z.number().optional(),
  min_duration_ms: z.number().min(0).optional(),
  max_duration_ms: z.number().min(0).optional(),
  timestamp_after: z.string().optional(),
  timestamp_before: z.string().optional(),
  order_desc: z.boolean().optional(),
});

export type ListRequestsQueryValidated = z.infer<
  typeof listRequestsQuerySchema
>;

// Analytics/aggregate response types
export interface StatusCodeBreakdown {
  status: string;
  count: number;
  percentage: number;
}

export interface ModelUsage {
  model: string;
  count: number;
  percentage: number;
  avg_latency_ms: number;
}

export interface TimeSeriesPoint {
  timestamp: string;
  duration_minutes?: number; // Present in backend response
  requests: number;
  input_tokens: number;
  output_tokens: number;
  avg_latency_ms?: number | null;
  p95_latency_ms?: number | null;
  p99_latency_ms?: number | null;
}

export interface RequestsAggregateResponse {
  total_requests: number;
  model?: string; // Present when filtering by specific model
  status_codes: StatusCodeBreakdown[];
  models?: ModelUsage[]; // Only present in "all models" view
  time_series: TimeSeriesPoint[];
}

// User usage statistics for a specific model
export interface UserUsage {
  user_id?: string;
  user_email?: string;
  request_count: number;
  total_tokens: number;
  input_tokens: number;
  output_tokens: number;
  total_cost?: number;
  last_active_at?: string;
}

// Response for model usage grouped by user
export interface ModelUserUsageResponse {
  model: string;
  start_date: string;
  end_date: string;
  total_requests: number;
  total_tokens: number;
  total_cost?: number;
  users: UserUsage[];
}

// Authentication types
export interface LoginRequest {
  email: string;
  password: string;
}

export interface RegisterRequest {
  username: string;
  email: string;
  password: string;
  display_name?: string;
}

export interface AuthResponse {
  user: UserResponse;
  message: string;
}

export interface AuthSuccessResponse {
  message: string;
}

export interface RegistrationInfo {
  enabled: boolean;
  message: string;
}

export interface LoginInfo {
  enabled: boolean;
  message: string;
}

export interface PasswordResetRequest {
  email: string;
}

export interface PasswordResetConfirmRequest {
  token_id: string;
  token: string;
  new_password: string;
}

export interface ChangePasswordRequest {
  current_password: string;
  new_password: string;
}

// User response type alias for auth responses
export type UserResponse = User;

// ===== COST MANAGEMENT TYPES =====

export interface CreditTransaction {
  id: string;
  type: "credit" | "debit";
  amount: number;
  description: string;
  timestamp: string; // ISO 8601 timestamp
  balance_after: number;
  model?: string; // Optional model name for debit transactions
}

export interface CreditBalanceResponse {
  balance: number;
  currency: string; // e.g., "credits"
}

export interface TransactionsListResponse {
  transactions: CreditTransaction[];
  total: number;
  limit: number;
  offset: number;
}

export interface TransactionsQuery {
  limit?: number;
  offset?: number;
  type?: "credit" | "debit";
  model?: string;
  start_date?: string; // ISO 8601 timestamp
  end_date?: string; // ISO 8601 timestamp
  userId?: string; // Filter transactions by user
}

export interface AddCreditsRequest {
  amount: number;
  description?: string;
}

export interface AddCreditsResponse {
  transaction: CreditTransaction;
  new_balance: number;
}

// Probe types
export interface Probe {
  id: string;
  name: string;
  deployment_id: string;
  interval_seconds: number;
  active: boolean;
  http_method: string;
  request_path?: string | null;
  request_body?: Record<string, any> | null;
  created_at: string;
  updated_at: string;
}

export interface CreateProbeRequest {
  name: string;
  deployment_id: string;
  interval_seconds: number;
  http_method?: string;
  request_path?: string | null;
  request_body?: Record<string, any> | null;
}

export interface ProbeResult {
  id: string;
  probe_id: string;
  executed_at: string;
  success: boolean;
  response_time_ms: number | null;
  status_code: number | null;
  error_message: string | null;
  response_data: any | null;
  metadata: any | null;
}

export interface ProbeStatistics {
  total_executions: number;
  successful_executions: number;
  failed_executions: number;
  success_rate: number;
  avg_response_time_ms: number | null;
  min_response_time_ms: number | null;
  max_response_time_ms: number | null;
  p50_response_time_ms: number | null;
  p95_response_time_ms: number | null;
  p99_response_time_ms: number | null;
  last_execution: string | null;
  last_success: string | null;
  last_failure: string | null;
}
