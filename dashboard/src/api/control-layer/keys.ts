// Query key factory for consistent caching
export const queryKeys = {
  // Users
  users: {
    all: ["users"] as const,
    query: (options?: { include?: string; skip?: number; limit?: number; search?: string }) =>
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
    byId: (id: string, include?: string) =>
      ["models", "byId", id, include] as const,
    // Composite model components
    components: (modelId: string) =>
      ["models", "byId", modelId, "components"] as const,
    // Separate metrics query for lazy loading
    metrics: (options?: {
      skip?: number;
      limit?: number;
      endpoint?: string;
      accessible?: boolean;
      search?: string;
      is_composite?: boolean;
      group?: string;
    }) => ["models", "metrics", options] as const,
  },

  providerDisplayConfigs: {
    all: ["providerDisplayConfigs"] as const,
    query: () => ["providerDisplayConfigs", "query"] as const,
    byKey: (providerKey: string) =>
      ["providerDisplayConfigs", "byKey", providerKey] as const,
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
    query: (options?: { enabled?: boolean; skip?: number; limit?: number }) =>
      ["endpoints", "query", options] as const,
    byId: (id: string) => ["endpoints", "byId", id] as const,
  },

  // API Keys
  apiKeys: {
    all: ["apiKeys"] as const,
    query: (
      userId: string = "current",
      options?: { skip?: number; limit?: number },
    ) => ["apiKeys", "query", userId, options] as const,
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

  // Monitoring
  monitoring: {
    all: ["monitoring"] as const,
    pendingRequestCounts: () => ["monitoring", "pendingRequestCounts"] as const,
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
    analytics: (id: string) =>
      [...queryKeys.batches.detail(id), "analytics"] as const,
    results: (id: string) =>
      [...queryKeys.batches.detail(id), "results"] as const,
    resultsList: (id: string, filters: any) =>
      [...queryKeys.batches.results(id), filters] as const,
  },

  // Payments
  payments: {
    all: ["payments"] as const,
    create: () => [...queryKeys.payments.all, "create"] as const,
    process: (sessionId: string) =>
      [...queryKeys.payments.all, "process", sessionId] as const,
  },

  // Usage
  usage: {
    all: ["usage"] as const,
    forRange: (startDate?: string, endDate?: string) =>
      ["usage", "range", startDate, endDate] as const,
  },

  // Webhooks
  webhooks: {
    all: ["webhooks"] as const,
    byUser: (userId: string) => ["webhooks", "byUser", userId] as const,
  },

  // Organizations
  organizations: {
    all: ["organizations"] as const,
    query: (options?: { skip?: number; limit?: number; search?: string; include?: string }) =>
      ["organizations", "query", options] as const,
    byId: (id: string) => ["organizations", "byId", id] as const,
    members: (orgId: string) =>
      ["organizations", "members", orgId] as const,
  },

  // Connections
  connections: {
    all: ["connections"] as const,
    list: (kind?: string) => ["connections", "list", kind] as const,
    byId: (id: string) => ["connections", "byId", id] as const,
    syncs: (connectionId: string) => ["connections", connectionId, "syncs"] as const,
    sync: (connectionId: string, syncId: string) => ["connections", connectionId, "syncs", syncId] as const,
    syncEntries: (connectionId: string, syncId: string) =>
      ["connections", connectionId, "syncs", syncId, "entries"] as const,
    files: (connectionId: string, options?: { search?: string; cursor?: string }) =>
      ["connections", connectionId, "files", options] as const,
  },
} as const;
