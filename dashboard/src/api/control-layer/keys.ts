// Query key factory for consistent caching
export const queryKeys = {
  // Users
  users: {
    all: ["users"] as const,
    query: (options?: { include?: string; skip?: number; limit?: number }) =>
      ["users", "query", options] as const,
    byId: (id: string, include?: string) =>
      ["users", "byId", id, include] as const,
  },

  // Models
  models: {
    all: ["models"] as const,
    query: (options?: {
      skip?: number;
      limit?: number;
      endpoint?: string;
      include?: string;
      accessible?: boolean;
      search?: string;
    }) => ["models", "query", options] as const,
    byId: (id: string) => ["models", "byId", id] as const,
  },

  // Groups
  groups: {
    all: ["groups"] as const,
    query: (options?: { include?: string; skip?: number; limit?: number }) =>
      ["groups", "query", options] as const,
    byId: (id: string) => ["groups", "byId", id] as const,
  },

  // Endpoints
  endpoints: {
    all: ["endpoints"] as const,
    byId: (id: string) => ["endpoints", "byId", id] as const,
  },

  // API Keys
  apiKeys: {
    all: ["apiKeys"] as const,
    query: (userId?: string) => ["apiKeys", "query", userId] as const,
    byId: (id: string, userId?: string) =>
      ["apiKeys", "byId", id, userId] as const,
  },

  // Requests
  requests: {
    all: ["requests"] as const,
    query: (options?: any) => ["requests", "query", options] as const,
    aggregate: (
      model?: string,
      timestampAfter?: string,
      timestampBefore?: string,
    ) =>
      [
        "requests",
        "aggregate",
        model,
        timestampAfter,
        timestampBefore,
      ] as const,
    aggregateByUser: (model?: string, startDate?: string, endDate?: string) =>
      ["requests", "aggregateByUser", model, startDate, endDate] as const,
  },

  // Files
  files: {
    all: ["files"] as const,
    lists: () => [...queryKeys.files.all, "list"] as const,
    list: (filters: any) => [...queryKeys.files.lists(), filters] as const,
    details: () => [...queryKeys.files.all, "detail"] as const,
    detail: (id: string) => [...queryKeys.files.details(), id] as const,
    requests: (id: string) =>
      [...queryKeys.files.detail(id), "requests"] as const,
    requestsList: (id: string, filters: any) =>
      [...queryKeys.files.requests(id), filters] as const,
  },

  // Batches
  batches: {
    all: ["batches"] as const,
    lists: () => [...queryKeys.batches.all, "list"] as const,
    list: (filters: any) => [...queryKeys.batches.lists(), filters] as const,
    details: () => [...queryKeys.batches.all, "detail"] as const,
    detail: (id: string) => [...queryKeys.batches.details(), id] as const,
    requests: (id: string) =>
      [...queryKeys.batches.detail(id), "requests"] as const,
    requestsList: (id: string, filters: any) =>
      [...queryKeys.batches.requests(id), filters] as const,
  },

  // Payments
  payments: {
    all: ["payments"] as const,
    create: () => [...queryKeys.payments.all, "create"] as const,
    process: (sessionId: string) =>
      [...queryKeys.payments.all, "process", sessionId] as const,
  },
} as const;
