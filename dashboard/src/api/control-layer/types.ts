import { z } from "zod";

// Generic paginated response wrapper
export interface PaginatedResponse<T> {
  data: T[];
  total_count: number;
  skip: number;
  limit: number;
}

export type ModelType = "CHAT" | "EMBEDDINGS" | "RERANKER";
export type AuthSource = "vouch" | "native" | "system" | "proxy-header";
export type Role =
  | "PlatformManager"
  | "RequestViewer"
  | "StandardUser"
  | "BillingManager"
  | "BatchAPIUser";
export type ApiKeyPurpose = "platform" | "realtime" | "batch" | "playground";
export type TariffApiKeyPurpose = "realtime" | "batch" | "playground";

// Config/Metadata types
export interface ConfigResponse {
  region: string;
  organization: string;
  payment_enabled: boolean;

  docs_jsonl_url?: string;
  docs_url: string;
  batches?: {
    enabled: boolean;
    allowed_completion_windows: string[]; // Available SLAs like ["24h", "1h", "12h"]
  };
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

// Tariff types (read-only from API)
export interface ModelTariff {
  id: string;
  deployed_model_id: string;
  name: string;
  input_price_per_token: string; // Decimal string to preserve precision
  output_price_per_token: string; // Decimal string to preserve precision
  valid_from: string; // ISO 8601 timestamp
  valid_until?: string | null; // ISO 8601 timestamp, null means currently active
  api_key_purpose?: TariffApiKeyPurpose | null;
  completion_window?: string | null; // SLA like "24h", "1h" - required for batch tariffs
  is_active: boolean;
}

// Tariff definition for model create/update
export interface TariffDefinition {
  name: string;
  input_price_per_token: string; // Decimal string to preserve precision
  output_price_per_token: string; // Decimal string to preserve precision
  api_key_purpose?: TariffApiKeyPurpose | null;
  completion_window?: string | null; // SLA like "24h", "1h" - required for batch tariffs
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
  capacity?: number | null; // Maximum concurrent requests allowed
  batch_capacity?: number | null; // Maximum concurrent batch requests allowed
  groups?: Group[]; // array of group IDs - only present when include=groups
  metrics?: ModelMetrics; // only present when include=metrics
  status?: ModelProbeStatus; // only present when include=status
  tariffs?: ModelTariff[]; // only present when include=pricing
  endpoint?: Endpoint; // only present when include=endpoints
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
  auth_header_name: string;
  auth_header_prefix: string;
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
  external_user_id: string;
  email: string;
  display_name?: string;
  avatar_url?: string;
  is_admin?: boolean;
  roles: Role[];
  groups?: Group[]; // only present when include=groups
  created_at: string; // ISO 8601 timestamp
  updated_at: string; // ISO 8601 timestamp
  auth_source: AuthSource;
  credit_balance?: number; // User's balance in dollars (backend field name is credit_balance)
}

export interface ApiKey {
  id: string;
  name: string;
  description?: string;
  purpose: ApiKeyPurpose; // Purpose of the key: platform (for /admin/api/*) or inference (for /ai/*)
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
  | "endpoints"
  | "pricing"
  | "groups,metrics"
  | "groups,status"
  | "groups,endpoints"
  | "groups,pricing"
  | "metrics,status"
  | "metrics,endpoints"
  | "metrics,pricing"
  | "status,endpoints"
  | "status,pricing"
  | "endpoints,pricing"
  | "groups,metrics,status"
  | "groups,metrics,endpoints"
  | "groups,metrics,pricing"
  | "groups,status,endpoints"
  | "groups,status,pricing"
  | "groups,endpoints,pricing"
  | "metrics,status,endpoints"
  | "metrics,status,pricing"
  | "metrics,endpoints,pricing"
  | "status,endpoints,pricing"
  | "groups,metrics,status,endpoints"
  | "groups,metrics,status,pricing"
  | "groups,metrics,endpoints,pricing"
  | "groups,status,endpoints,pricing"
  | "metrics,status,endpoints,pricing"
  | "groups,metrics,status,endpoints,pricing";
export type GroupsInclude = "users" | "models" | "users,models";
export type UsersInclude = "groups";

// List endpoint query parameters
export interface ModelsQuery {
  skip?: number;
  limit?: number;
  endpoint?: string;
  include?: ModelsInclude;
  accessible?: boolean; // Filter to only models the current user can access
  search?: string; // Search query to filter models by alias or model_name
}

export interface EndpointsQuery {
  skip?: number;
  limit?: number;
  enabled?: boolean;
}

export interface GroupsQuery {
  skip?: number;
  limit?: number;
  include?: GroupsInclude;
  search?: string;
}

export interface UsersQuery {
  skip?: number;
  limit?: number;
  include?: UsersInclude;
  search?: string;
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
  purpose: ApiKeyPurpose; // Required: purpose of the key
  requests_per_second?: number | null;
  burst_size?: number | null;
}

export interface ApiKeysQuery {
  skip?: number;
  limit?: number;
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
  capacity?: number | null;
  batch_capacity?: number | null;
  tariffs?: TariffDefinition[];
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

// New simplified analytics entry (from http_analytics table)
export interface AnalyticsEntry {
  id: number;
  timestamp: string;
  method: string;
  uri: string;
  model?: string;
  status_code?: number;
  duration_ms?: number;
  prompt_tokens?: number;
  completion_tokens?: number;
  total_tokens?: number;
  response_type?: string;
  user_email?: string;
  fusillade_batch_id?: string;
  input_price_per_token?: string;
  output_price_per_token?: string;
  custom_id?: string;
}

export interface ListAnalyticsResponse {
  entries: AnalyticsEntry[];
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
  skip?: number;
  limit?: number;
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
  model?: string;
  fusillade_batch_id?: string;
  custom_id?: string;
}

// Validation schemas
export const listRequestsQuerySchema = z.object({
  skip: z.number().min(0).optional(),
  limit: z.number().min(1).max(1000).optional(),
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

// Backend transaction type enum
export type TransactionType =
  | "admin_grant"
  | "admin_removal"
  | "usage"
  | "purchase";

export interface Transaction {
  id: string;
  user_id: string; // UUID
  transaction_type: TransactionType;
  batch_id?: string; // Batch ID (present when this is a grouped batch of multiple usage transactions)
  amount: number; // Amount in dollars
  source_id: string;
  description?: string;
  created_at: string; // ISO 8601 timestamp
}

export interface BalanceResponse {
  balance: number; // Balance in dollars
  currency: string; // e.g., "USD"
}

export interface TransactionsListResponse {
  data: Transaction[];
  total_count: number;
  limit: number;
  skip: number;
  /** Current user balance when skip=0, or balance at the pagination point when skip>0.
   * Frontend can compute each row's balance by subtracting signed amounts from this value. */
  page_start_balance: number;
}

export interface TransactionsQuery {
  limit?: number;
  skip?: number;
  userId?: string; // Filter transactions by user (UUID)
  group_batches?: boolean; // Group transactions by batch (merges batch requests into single entries)
  search?: string; // Search term for description (case-insensitive)
  transaction_types?: string; // Comma-separated transaction types (e.g., "admin_grant,purchase" or "usage,admin_removal")
  start_date?: string; // Filter transactions created on or after this date/time (ISO 8601 format)
  end_date?: string; // Filter transactions created on or before this date/time (ISO 8601 format)
}

export interface AddFundsRequest {
  user_id: string; // UUID of the user to add funds to
  source_id: string; // UUID of the user providing the funds
  amount: number; // Amount in dollars
  description?: string;
}

export type AddFundsResponse = Transaction;

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

export interface FileObject {
  id: string;
  object: "file";
  bytes: number;
  created_at: number; // Unix timestamp
  expires_at?: number; // Unix timestamp
  filename: string;
  purpose:
    | "batch"
    | "batch_output"
    | "batch_error"
    | "fine-tune"
    | "assistants"
    | "vision"
    | "user_data"
    | "evals";
}

export interface FileListResponse {
  object: "list";
  data: FileObject[];
  first_id: string;
  last_id: string;
  has_more: boolean;
}

export interface FileUploadRequest {
  file: File;
  purpose: string;
  filename?: string;
  expires_after?: {
    anchor: "created_at";
    seconds: number;
  };
}

export interface FileDeleteResponse {
  id: string;
  object: "file";
  deleted: boolean;
}

export interface FilesListQuery {
  after?: string;
  limit?: number;
  order?: "asc" | "desc";
  purpose?: string;
  search?: string;
}

export interface ModelCostBreakdown {
  model: string;
  request_count: number;
  estimated_input_tokens: number;
  estimated_output_tokens: number;
  estimated_cost: string;
}

export interface FileCostEstimate {
  file_id: string;
  total_requests: number;
  total_estimated_input_tokens: number;
  total_estimated_output_tokens: number;
  total_estimated_cost: string;
  models: ModelCostBreakdown[];
}

export interface BatchRequestCounts {
  total: number;
  completed: number;
  failed: number;
}

export interface BatchUsage {
  input_tokens: number;
  input_tokens_details?: {
    cached_tokens?: number;
    text_tokens?: number;
    audio_tokens?: number;
  };
  output_tokens: number;
  output_tokens_details?: {
    text_tokens?: number;
    audio_tokens?: number;
    reasoning_tokens?: number;
  };
  total_tokens: number;
}

export interface BatchError {
  code: string;
  line?: number;
  message: string;
  param?: string;
}

export interface BatchErrors {
  object: "list";
  data: BatchError[];
}

export type BatchStatus =
  | "validating"
  | "failed"
  | "in_progress"
  | "finalizing"
  | "completed"
  | "expired"
  | "cancelling"
  | "cancelled";

export interface Batch {
  id: string;
  object: "batch";
  endpoint: string;
  errors?: BatchErrors | null;
  input_file_id: string;
  completion_window: string; // SLA like "24h", "1h", "12h", "48h"
  status: BatchStatus;
  output_file_id?: string | null;
  error_file_id?: string | null;
  created_at: number; // Unix timestamp
  in_progress_at?: number | null;
  expires_at?: number | null;
  finalizing_at?: number | null;
  completed_at?: number | null;
  failed_at?: number | null;
  expired_at?: number | null;
  cancelling_at?: number | null;
  cancelled_at?: number | null;
  request_counts: BatchRequestCounts;
  metadata?: Record<string, string>;
  usage?: BatchUsage;
}

export interface BatchListResponse {
  object: "list";
  data: Batch[];
  first_id?: string;
  last_id?: string;
  has_more: boolean;
}

export interface BatchCreateRequest {
  input_file_id: string;
  endpoint: string;
  completion_window: string; // SLA like "24h", "1h", "12h", "48h"
  metadata?: Record<string, string>;
  output_expires_after?: {
    anchor: "created_at";
    seconds: number;
  };
}

export interface BatchesListQuery {
  after?: string;
  limit?: number;
  search?: string;
}

// ===== BATCH REQUESTS (Custom endpoints beyond OpenAI spec) =====

export type RequestStatus =
  | "pending"
  | "in_progress"
  | "completed"
  | "failed"
  | "cancelled";

export interface BatchRequest {
  id: string;
  batch_id: string;
  custom_id: string;
  status: RequestStatus;
  request: {
    method: string;
    url: string;
    body: any;
  };
  response?: {
    status_code: number;
    body: any;
  } | null;
  error?: {
    code: string;
    message: string;
  } | null;
  created_at: number;
  started_at?: number | null;
  completed_at?: number | null;
  usage?: {
    prompt_tokens: number;
    completion_tokens: number;
    total_tokens: number;
  };
}

export interface BatchRequestsListResponse {
  object: "list";
  data: BatchRequest[];
  has_more: boolean;
  total: number;
}

export interface BatchRequestsListQuery {
  limit?: number;
  skip?: number;
  status?: RequestStatus;
}

// File requests (templates in a file, before batch creation)
export interface FileRequest {
  custom_id: string;
  method: string;
  url: string;
  body: any;
}

export interface FileRequestsListResponse {
  object: "list";
  data: FileRequest[];
  has_more: boolean;
  total: number;
}

export interface FileRequestsListQuery {
  limit?: number;
  skip?: number;
}

// Batch Analytics (Custom endpoint beyond OpenAI spec)
export interface BatchAnalytics {
  total_requests: number;
  total_prompt_tokens: number;
  total_completion_tokens: number;
  total_tokens: number;
  avg_duration_ms?: number | null;
  avg_ttfb_ms?: number | null;
  total_cost?: string | null;
}

// Batch Result Item (merged input/output for Results view)
export type BatchResultStatus =
  | "pending"
  | "in_progress"
  | "completed"
  | "failed"
  | "cancelled";

export interface BatchResultItem {
  /** Fusillade request ID (unique identifier) */
  id: string;
  /** User-provided identifier (NOT unique - may be duplicated) */
  custom_id: string | null;
  /** Model used for this request */
  model: string;
  /** Original request body from the input template */
  input_body: Record<string, unknown>;
  /** Full response object (choices, usage, etc.) for completed requests */
  response_body: Record<string, unknown> | null;
  /** Error message for failed requests */
  error: string | null;
  /** Current status of the request */
  status: BatchResultStatus;
}

// ===== DAEMON MONITORING TYPES =====

export type DaemonStatus = "initializing" | "running" | "dead";

export interface DaemonStats {
  requests_processed: number;
  requests_failed: number;
  requests_in_flight: number;
}

export interface DaemonConfig {
  claim_batch_size: number;
  default_model_concurrency: number;
  model_concurrency_limits: Record<string, number>;
  claim_interval_ms: number;
  min_retries?: number | null;
  stop_before_deadline_ms?: number | null;
  max_retries?: number | null;
  backoff_ms: number;
  backoff_factor: number;
  max_backoff_ms: number;
  timeout_ms: number;
  status_log_interval_ms?: number | null;
  heartbeat_interval_ms: number;
  claim_timeout_ms: number;
  processing_timeout_ms: number;
}

export interface Daemon {
  id: string;
  status: DaemonStatus;
  hostname: string;
  pid: number;
  version: string;
  started_at: number; // Unix timestamp
  last_heartbeat?: number | null; // Unix timestamp
  stopped_at?: number | null; // Unix timestamp
  stats: DaemonStats;
  config: DaemonConfig;
}

export interface DaemonsListResponse {
  daemons: Daemon[];
}

export interface DaemonsQuery {
  status?: DaemonStatus;
}
