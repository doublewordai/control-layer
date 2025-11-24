import { useQuery, useMutation, useQueryClient } from "@tanstack/react-query";
import { dwctlApi } from "./client";
import { queryKeys } from "./keys";
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
  UsersQuery,
  ModelsQuery,
  GroupsQuery,
  ListRequestsQuery,
  TransactionsQuery,
  CreateProbeRequest,
  FilesListQuery,
  BatchCreateRequest,
  BatchesListQuery,
  Batch,
  FileUploadRequest,
  AddFundsRequest,
  DaemonsQuery,
} from "./types";

// Config hooks
export function useConfig() {
  return useQuery({
    queryKey: ["config"],
    queryFn: () => dwctlApi.config.get(),
    staleTime: 30 * 60 * 1000, // 30 minutes - config rarely changes
  });
}

// Users hooks
export function useUsers(options?: UsersQuery & { enabled?: boolean }) {
  const { enabled = true, ...queryOptions } = options || {};
  return useQuery({
    queryKey: queryKeys.users.query(queryOptions),
    queryFn: () => dwctlApi.users.list(queryOptions),
    enabled,
  });
}

export function useUser(id: string, options?: { include?: string }) {
  return useQuery({
    queryKey: queryKeys.users.byId(id, options?.include),
    queryFn: () => dwctlApi.users.get(id, options),
    staleTime: 30 * 1000, // 30 seconds - matches useTransactions to keep balance in sync
  });
}

export function useUserBalance(id: string) {
  // Reuse the useUser cache to avoid duplicate queries and ensure consistency
  // Explicitly request billing data for this hook
  const userQuery = useUser(id, { include: "billing" });

  return {
    data: userQuery.data?.credit_balance || 0,
    isLoading: userQuery.isLoading,
    isError: userQuery.isError,
    error: userQuery.error,
    refetch: userQuery.refetch,
  };
}

export function useCreateUser() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationKey: ["users", "create"],
    mutationFn: (data: UserCreateRequest) => dwctlApi.users.create(data),
    // Refetch queries after mutation completes (success or error)
    onSettled: () => {
      queryClient.invalidateQueries({ queryKey: queryKeys.users.all });
      queryClient.invalidateQueries({ queryKey: queryKeys.groups.all });
    },
  });
}

export function useUpdateUser() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationKey: ["users", "update"],
    mutationFn: ({ id, data }: { id: string; data: UserUpdateRequest }) =>
      dwctlApi.users.update(id, data),
    // Refetch queries after mutation completes (success or error)
    onSettled: () => {
      queryClient.invalidateQueries({ queryKey: queryKeys.users.all });
      queryClient.invalidateQueries({ queryKey: queryKeys.groups.all });
    },
  });
}

export function useDeleteUser() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationKey: ["users", "delete"],
    mutationFn: (id: string) => dwctlApi.users.delete(id),
    // Refetch queries after mutation completes (success or error)
    onSettled: () => {
      queryClient.invalidateQueries({ queryKey: queryKeys.users.all });
      queryClient.invalidateQueries({ queryKey: queryKeys.groups.all });
    },
  });
}

// Models hooks
export function useModels(options?: ModelsQuery) {
  return useQuery({
    queryKey: queryKeys.models.query(options),
    queryFn: () => dwctlApi.models.list(options),
  });
}

export function useModel(id: string) {
  return useQuery({
    queryKey: queryKeys.models.byId(id),
    queryFn: () => dwctlApi.models.get(id),
  });
}

export function useUpdateModel() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: ({ id, data }: { id: string; data: ModelUpdateRequest }) =>
      dwctlApi.models.update(id, data),
    onSuccess: (updatedModel) => {
      queryClient.invalidateQueries({ queryKey: queryKeys.models.all });
      queryClient.setQueryData(
        queryKeys.models.byId(updatedModel.id),
        updatedModel,
      );
    },
  });
}

// Groups hooks
export function useGroups(options?: GroupsQuery & { enabled?: boolean }) {
  const { enabled = true, ...queryOptions } = options || {};
  return useQuery({
    queryKey: queryKeys.groups.query(queryOptions),
    queryFn: () => dwctlApi.groups.list(queryOptions),
    enabled,
  });
}

export function useGroup(id: string) {
  return useQuery({
    queryKey: queryKeys.groups.byId(id),
    queryFn: () => dwctlApi.groups.get(id),
  });
}

export function useCreateGroup() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: (data: GroupCreateRequest) => dwctlApi.groups.create(data),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: queryKeys.groups.all });
    },
  });
}

export function useUpdateGroup() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: ({ id, data }: { id: string; data: GroupUpdateRequest }) =>
      dwctlApi.groups.update(id, data),
    onSuccess: (updatedGroup) => {
      queryClient.invalidateQueries({ queryKey: queryKeys.groups.all });
      queryClient.setQueryData(
        queryKeys.groups.byId(updatedGroup.id),
        updatedGroup,
      );
    },
  });
}

export function useDeleteGroup() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: (id: string) => dwctlApi.groups.delete(id),
    onSuccess: (_, deletedId) => {
      queryClient.invalidateQueries({ queryKey: queryKeys.groups.all });
      queryClient.removeQueries({ queryKey: queryKeys.groups.byId(deletedId) });
    },
  });
}

// Group relationship management hooks
export function useAddUserToGroup() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationKey: ["groups", "addUser"],
    mutationFn: ({ groupId, userId }: { groupId: string; userId: string }) =>
      dwctlApi.groups.addUser(groupId, userId),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: queryKeys.groups.all });
      queryClient.invalidateQueries({ queryKey: queryKeys.users.all });
    },
  });
}

export function useRemoveUserFromGroup() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationKey: ["groups", "removeUser"],
    mutationFn: ({ groupId, userId }: { groupId: string; userId: string }) =>
      dwctlApi.groups.removeUser(groupId, userId),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: queryKeys.groups.all });
      queryClient.invalidateQueries({ queryKey: queryKeys.users.all });
    },
  });
}

export function useAddModelToGroup() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: ({ groupId, modelId }: { groupId: string; modelId: string }) =>
      dwctlApi.groups.addModel(groupId, modelId),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: queryKeys.groups.all });
      queryClient.invalidateQueries({ queryKey: queryKeys.models.all });
    },
  });
}

export function useRemoveModelFromGroup() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: ({ groupId, modelId }: { groupId: string; modelId: string }) =>
      dwctlApi.groups.removeModel(groupId, modelId),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: queryKeys.groups.all });
      queryClient.invalidateQueries({ queryKey: queryKeys.models.all });
    },
  });
}

// Endpoints hooks
export function useEndpoints(options?: { enabled?: boolean }) {
  const { enabled = true } = options || {};
  return useQuery({
    queryKey: queryKeys.endpoints.all,
    queryFn: () => dwctlApi.endpoints.list(),
    enabled,
  });
}

export function useEndpoint(id: string) {
  return useQuery({
    queryKey: queryKeys.endpoints.byId(id),
    queryFn: () => dwctlApi.endpoints.get(id),
  });
}

export function useValidateEndpoint() {
  return useMutation({
    mutationKey: ["endpoints", "validate"],
    mutationFn: (data: EndpointValidateRequest) =>
      dwctlApi.endpoints.validate(data),
  });
}

export function useCreateEndpoint() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationKey: ["endpoints", "create"],
    mutationFn: (data: EndpointCreateRequest) =>
      dwctlApi.endpoints.create(data),
    onSettled: () => {
      queryClient.invalidateQueries({ queryKey: queryKeys.endpoints.all });
      queryClient.invalidateQueries({ queryKey: queryKeys.models.all });
    },
  });
}

export function useUpdateEndpoint() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationKey: ["endpoints", "update"],
    mutationFn: ({ id, data }: { id: string; data: EndpointUpdateRequest }) =>
      dwctlApi.endpoints.update(id, data),
    onSettled: (_, __, variables) => {
      queryClient.invalidateQueries({ queryKey: queryKeys.endpoints.all });
      queryClient.invalidateQueries({ queryKey: queryKeys.models.all });
      if (variables?.id) {
        queryClient.invalidateQueries({
          queryKey: queryKeys.endpoints.byId(variables.id),
        });
      }
    },
  });
}

export function useDeleteEndpoint() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationKey: ["endpoints", "delete"],
    mutationFn: (id: string) => dwctlApi.endpoints.delete(id),
    onSettled: () => {
      queryClient.invalidateQueries({ queryKey: queryKeys.endpoints.all });
      queryClient.invalidateQueries({ queryKey: queryKeys.models.all });
    },
  });
}

export function useSynchronizeEndpoint() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationKey: ["endpoints", "synchronize"],
    mutationFn: (id: string) => dwctlApi.endpoints.synchronize(id),
    onSettled: () => {
      // Refetch models since synchronization affects deployments
      queryClient.invalidateQueries({ queryKey: queryKeys.models.all });
    },
  });
}

// API Keys hooks
export function useApiKeys(userId = "current") {
  return useQuery({
    queryKey: queryKeys.apiKeys.query(userId),
    queryFn: () => dwctlApi.users.apiKeys.getAll(userId),
  });
}

export function useApiKey(id: string, userId = "current") {
  return useQuery({
    queryKey: queryKeys.apiKeys.byId(id, userId),
    queryFn: () => dwctlApi.users.apiKeys.get(id, userId),
  });
}

export function useCreateApiKey() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: ({
      data,
      userId = "current",
    }: {
      data: ApiKeyCreateRequest;
      userId?: string;
    }) => dwctlApi.users.apiKeys.create(data, userId),
    onSuccess: (_, { userId = "current" }) => {
      queryClient.invalidateQueries({
        queryKey: queryKeys.apiKeys.query(userId),
      });
    },
  });
}

export function useDeleteApiKey() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: ({
      keyId,
      userId = "current",
    }: {
      keyId: string;
      userId?: string;
    }) => dwctlApi.users.apiKeys.delete(keyId, userId),
    onSuccess: (_, { keyId, userId = "current" }) => {
      queryClient.invalidateQueries({
        queryKey: queryKeys.apiKeys.query(userId),
      });
      queryClient.removeQueries({
        queryKey: queryKeys.apiKeys.byId(keyId, userId),
      });
    },
  });
}

// Requests hooks
export function useRequests(
  options?: ListRequestsQuery,
  queryOptions?: { enabled?: boolean },
  dateRange?: { from: Date; to: Date },
) {
  const optionsWithDate = {
    ...options,
    ...(dateRange && {
      timestamp_after: dateRange.from.toISOString(),
      timestamp_before: dateRange.to.toISOString(),
    }),
  };

  return useQuery({
    queryKey: queryKeys.requests.query(optionsWithDate),
    queryFn: () => dwctlApi.requests.list(optionsWithDate),
    enabled: queryOptions?.enabled ?? true,
  });
}

export function useRequestsAggregate(
  model?: string,
  dateRange?: { from: Date; to: Date },
) {
  const timestampAfter = dateRange?.from?.toISOString();
  const timestampBefore = dateRange?.to?.toISOString();

  return useQuery({
    queryKey: queryKeys.requests.aggregate(
      model,
      timestampAfter,
      timestampBefore,
    ),
    queryFn: () =>
      dwctlApi.requests.aggregate(model, timestampAfter, timestampBefore),
  });
}

export function useRequestsAggregateByUser(
  model?: string,
  startDate?: string,
  endDate?: string,
) {
  return useQuery({
    queryKey: queryKeys.requests.aggregateByUser(model, startDate, endDate),
    queryFn: () => dwctlApi.requests.aggregateByUser(model, startDate, endDate),
    enabled: !!model,
  });
}

// Authentication hooks
export function useRegistrationInfo() {
  return useQuery({
    queryKey: ["registration-info"],
    queryFn: () => dwctlApi.auth.getRegistrationInfo(),
    staleTime: 5 * 60 * 1000, // 5 minutes
  });
}

export function useLoginInfo() {
  return useQuery({
    queryKey: ["login-info"],
    queryFn: () => dwctlApi.auth.getLoginInfo(),
    staleTime: 5 * 60 * 1000, // 5 minutes
  });
}

export function useRequestPasswordReset() {
  return useMutation({
    mutationKey: ["password-reset", "request"],
    mutationFn: (email: string) =>
      dwctlApi.auth.requestPasswordReset({ email }),
  });
}

export function useConfirmPasswordReset() {
  return useMutation({
    mutationKey: ["password-reset", "confirm"],
    mutationFn: (data: {
      token_id: string;
      token: string;
      new_password: string;
    }) => dwctlApi.auth.confirmPasswordReset(data),
  });
}

// Probes hooks
export function useProbes(status?: string) {
  return useQuery({
    queryKey: ["probes", status],
    queryFn: () => dwctlApi.probes.list(status),
    refetchInterval: 10000, // Refetch every 10 seconds for live updates
  });
}

export function useProbe(id: string) {
  return useQuery({
    queryKey: ["probes", id],
    queryFn: () => dwctlApi.probes.get(id),
  });
}

export function useProbeResults(
  id: string,
  params?: { start_time?: string; end_time?: string; limit?: number },
  options?: { enabled?: boolean },
) {
  return useQuery({
    queryKey: ["probes", id, "results", params],
    queryFn: () => dwctlApi.probes.getResults(id, params),
    refetchInterval: 5000, // Refetch every 5 seconds for live updates
    enabled: options?.enabled ?? true,
  });
}

export function useProbeStatistics(
  id: string,
  params?: { start_time?: string; end_time?: string },
  options?: { enabled?: boolean },
) {
  return useQuery({
    queryKey: ["probes", id, "statistics", params],
    queryFn: () => dwctlApi.probes.getStatistics(id, params),
    refetchInterval: 10000, // Refetch every 10 seconds
    enabled: options?.enabled ?? true,
  });
}

export function useCreateProbe() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationKey: ["probes", "create"],
    mutationFn: (data: CreateProbeRequest) => dwctlApi.probes.create(data),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["probes"] });
      queryClient.invalidateQueries({ queryKey: queryKeys.models.all });
    },
  });
}

export function useDeleteProbe() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationKey: ["probes", "delete"],
    mutationFn: (id: string) => dwctlApi.probes.delete(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["probes"] });
      queryClient.invalidateQueries({ queryKey: queryKeys.models.all });
    },
  });
}

export function useActivateProbe() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationKey: ["probes", "activate"],
    mutationFn: (id: string) => dwctlApi.probes.activate(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["probes"] });
      queryClient.invalidateQueries({ queryKey: queryKeys.models.all });
    },
  });
}

export function useDeactivateProbe() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationKey: ["probes", "deactivate"],
    mutationFn: (id: string) => dwctlApi.probes.deactivate(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ["probes"] });
      queryClient.invalidateQueries({ queryKey: queryKeys.models.all });
    },
  });
}

export function useExecuteProbe() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationKey: ["probes", "execute"],
    mutationFn: (id: string) => dwctlApi.probes.execute(id),
    onSuccess: (_, id) => {
      queryClient.invalidateQueries({ queryKey: ["probes", id, "results"] });
      queryClient.invalidateQueries({ queryKey: ["probes", id, "statistics"] });
    },
  });
}

export function useTestProbe() {
  return useMutation({
    mutationKey: ["probes", "test"],
    mutationFn: ({
      deploymentId,
      params,
    }: {
      deploymentId: string;
      params?: {
        http_method?: string;
        request_path?: string;
        request_body?: Record<string, unknown>;
      };
    }) => dwctlApi.probes.test(deploymentId, params),
  });
}

export function useUpdateProbe() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationKey: ["probes", "update"],
    mutationFn: ({
      id,
      data,
    }: {
      id: string;
      data: {
        interval_seconds?: number;
        http_method?: string;
        request_path?: string | null;
        request_body?: Record<string, any> | null;
      };
    }) => dwctlApi.probes.update(id, data),
    onSuccess: (_, variables) => {
      queryClient.invalidateQueries({ queryKey: ["probes"] });
      queryClient.invalidateQueries({ queryKey: ["probes", variables.id] });
      queryClient.invalidateQueries({ queryKey: queryKeys.models.all });
    },
  });
}

// ===== FILES HOOKS =====

export function useFiles(options?: FilesListQuery & { enabled?: boolean }) {
  const { enabled, ...queryOptions } = options || {};
  return useQuery({
    queryKey: queryKeys.files.list(queryOptions),
    queryFn: () => dwctlApi.files.list(queryOptions),
    enabled,
  });
}

export function useFile(id: string) {
  return useQuery({
    queryKey: queryKeys.files.detail(id),
    queryFn: () => dwctlApi.files.get(id),
    enabled: !!id,
  });
}

// Deprecated: Use dwctlApi.files.getContent() with useQuery directly instead
// export function useFileRequests(id: string, options?: FileRequestsListQuery) {
//   return useQuery({
//     queryKey: queryKeys.files.requestsList(id, options || {}),
//     queryFn: () => dwctlApi.files.getContent(id, {limit: options?.limit, offset: options?.skip}),
//     enabled: !!id,
//   });
// }

export function useUploadFile() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: (data: FileUploadRequest) => dwctlApi.files.upload(data),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: queryKeys.files.lists() });
    },
  });
}

export function useDeleteFile() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: (id: string) => dwctlApi.files.delete(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: queryKeys.files.lists() });
    },
  });
}

// ===== BATCHES HOOKS =====

export function useBatches(options?: BatchesListQuery & { enabled?: boolean }) {
  const queryClient = useQueryClient();
  const { enabled, ...queryOptions } = options || {};

  return useQuery({
    queryKey: queryKeys.batches.list(queryOptions),
    queryFn: async () => {
      const response = await dwctlApi.batches.list(queryOptions);

      // Check if any batches just completed by comparing with cached data
      const previousData = queryClient.getQueryData<{ data: Batch[] }>(
        queryKeys.batches.list(queryOptions),
      );

      if (previousData && response.data) {
        const hasNewlyCompletedBatch = response.data.some((newBatch) => {
          const oldBatch = previousData.data.find((b) => b.id === newBatch.id);
          return (
            oldBatch &&
            ["validating", "in_progress", "finalizing"].includes(
              oldBatch.status,
            ) &&
            ["completed", "failed", "expired", "cancelled"].includes(
              newBatch.status,
            )
          );
        });

        // If a batch just completed, invalidate files to fetch new output/error files
        if (hasNewlyCompletedBatch) {
          queryClient.invalidateQueries({ queryKey: queryKeys.files.lists() });
        }
      }

      return response;
    },
    enabled,
    refetchOnMount: "always",
    refetchInterval: (query) => {
      const batches = query.state.data?.data;
      // Refetch every 2 seconds if any batch is in progress or cancelling
      if (
        batches &&
        batches.some((batch: Batch) =>
          ["validating", "in_progress", "finalizing", "cancelling"].includes(
            batch.status,
          ),
        )
      ) {
        return 2000;
      }
      return false;
    },
  });
}

export function useBatch(id: string) {
  return useQuery({
    queryKey: queryKeys.batches.detail(id),
    queryFn: () => dwctlApi.batches.get(id),
    enabled: !!id,
    refetchInterval: (query) => {
      const batch = query.state.data as Batch | undefined;
      // Refetch every 2 seconds if batch is in progress
      if (
        batch &&
        ["validating", "in_progress", "finalizing", "cancelling"].includes(
          batch.status,
        )
      ) {
        return 2000;
      }
      return false;
    },
  });
}

// Deprecated: Batch request viewing disabled - would need to fetch/merge output and error files
// export function useBatchRequests(
//   id: string,
//   options?: BatchRequestsListQuery,
// ) {
//   return useQuery({
//     queryKey: queryKeys.batches.requestsList(id, options || {}),
//     queryFn: () => dwctlApi.batches.getRequests(id, options),
//     enabled: !!id,
//     refetchInterval: (query) => {
//       // Refetch requests periodically while batch is processing
//       const parent = query.state.data as BatchRequestsListResponse | undefined;
//       if (parent) {
//         const hasInProgress = parent.data.some(
//           (r) => r.status === "pending" || r.status === "in_progress",
//         );
//         if (hasInProgress) {
//           return 3000; // Refetch every 3 seconds
//         }
//       }
//       return false;
//     },
//   });
// }

export function useCreateBatch() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: (data: BatchCreateRequest) => dwctlApi.batches.create(data),
    onSuccess: async () => {
      // Invalidate and refetch batches queries
      await queryClient.invalidateQueries({
        queryKey: queryKeys.batches.lists(),
        refetchType: "all",
      });
      // Also refetch immediately
      await queryClient.refetchQueries({
        queryKey: queryKeys.batches.lists(),
      });
    },
  });
}

export function useCancelBatch() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationFn: (id: string) => dwctlApi.batches.cancel(id),
    onSuccess: (data) => {
      queryClient.invalidateQueries({
        queryKey: queryKeys.batches.detail(data.id),
      });
      queryClient.invalidateQueries({ queryKey: queryKeys.batches.lists() });
    },
  });
}

export function useDownloadBatchResults() {
  return useMutation({
    mutationFn: async (id: string) => {
      const blob = await dwctlApi.batches.downloadResults(id);
      // Create download link
      const url = window.URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = `batch-${id}-results.jsonl`;
      document.body.appendChild(a);
      a.click();
      window.URL.revokeObjectURL(url);
      document.body.removeChild(a);
    },
  });
}

// Cost management hooks

export function useTransactions(query?: TransactionsQuery) {
  return useQuery({
    queryKey: ["cost", "transactions", query],
    queryFn: () => dwctlApi.cost.listTransactions(query),
    staleTime: 30 * 1000, // 30 seconds
  });
}

export function useAddFunds() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationKey: ["cost", "add-funds"],
    mutationFn: (data: AddFundsRequest) => dwctlApi.cost.addFunds(data),
    onSuccess: async (_, variables) => {
      // Refetch user balance and transactions from server
      await Promise.all([
        queryClient.refetchQueries({
          queryKey: queryKeys.users.byId(variables.user_id, "billing"),
        }),
        queryClient.invalidateQueries({
          queryKey: ["cost", "transactions"],
          exact: false,
        }),
      ]);
    },
  });
}

// Payment hooks

export function useCreatePayment() {
  return useMutation({
    mutationKey: queryKeys.payments.create(),
    mutationFn: () => dwctlApi.payments.create(),
  });
}

export function useProcessPayment() {
  const queryClient = useQueryClient();

  return useMutation({
    mutationKey: ["payments", "process"],
    mutationFn: (sessionId: string) => dwctlApi.payments.process(sessionId),
    onSuccess: () => {
      // Refetch user data to update balance after successful payment
      queryClient.invalidateQueries({
        queryKey: queryKeys.users.all,
      });
      queryClient.invalidateQueries({
        queryKey: ["cost", "transactions"],
        exact: false,
      });
    },
  });
}

// ===== DAEMONS HOOKS =====

export function useDaemons(options?: DaemonsQuery) {
  return useQuery({
    queryKey: ["daemons", "list", options],
    queryFn: () => dwctlApi.daemons.list(options),
    refetchInterval: 5000, // Refetch every 5 seconds to show live daemon status
  });
}
