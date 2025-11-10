// Query key factory for consistent caching
export const queryKeys = {
  // Users
  users: {
    all: ["users"] as const,
    query: (options?: { include?: string }) =>
      ["users", "query", options] as const,
    byId: (id: string, include?: string) =>
      ["users", "byId", id, include] as const,
  },

  // Models
  models: {
    all: ["models"] as const,
    query: (options?: { endpoint?: string }) =>
      ["models", "query", options] as const,
    byId: (id: string) => ["models", "byId", id] as const,
  },

  // Groups
  groups: {
    all: ["groups"] as const,
    query: (options?: { include?: string }) =>
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
} as const;
