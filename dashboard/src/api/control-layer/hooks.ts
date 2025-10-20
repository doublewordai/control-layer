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

export function useUser(id: string) {
  return useQuery({
    queryKey: queryKeys.users.byId(id),
    queryFn: () => dwctlApi.users.get(id),
  });
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
    queryFn: () =>
      dwctlApi.requests.aggregateByUser(model, startDate, endDate),
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
