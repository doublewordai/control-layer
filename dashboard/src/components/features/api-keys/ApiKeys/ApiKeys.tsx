import React, { useState } from "react";
import {
  Key,
  Plus,
  Trash2,
  Copy,
  Loader2,
  Check,
  ChevronDown,
  Info,
  RefreshCw,
} from "lucide-react";
import { toast } from "sonner";
import {
  useApiKeys,
  useCreateApiKey,
  useDeleteApiKey,
  useUpdateApiKey,
  type ApiKey,
  type ApiKeyCreateResponse,
  type ApiKeyPurpose,
  type SpendLimitInterval,
} from "../../../../api/control-layer";
import {
  formatCredits,
  formatResetInstant,
  resetPreviewLine,
} from "./spendCap";
import {
  useUser,
  useRotateApiKey,
  useOrganizationMembers,
} from "../../../../api/control-layer/hooks";
import { DataTable } from "../../../ui/data-table";
import { createColumns } from "./columns";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from "../../../ui/dialog";
import { Button } from "../../../ui/button";
import { Input } from "../../../ui/input";
import { Textarea } from "../../../ui/textarea";
import { Label } from "../../../ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../../ui/select";
import {
  Collapsible,
  CollapsibleContent,
  CollapsibleTrigger,
} from "../../../ui/collapsible";
import {
  HoverCard,
  HoverCardContent,
  HoverCardTrigger,
} from "../../../ui/hover-card";
import { Tabs, TabsList, TabsTrigger } from "../../../ui/tabs";
import { useServerPagination } from "@/hooks/useServerPagination";
import { useOrganizationContext } from "@/contexts";

export const ApiKeys: React.FC = () => {
  const { data: user } = useUser("current");
  const { activeOrganizationId, activeOrganization } = useOrganizationContext();
  // In org context, use org ID for API key operations (orgs are virtual users)
  const targetUserId = activeOrganizationId || user?.id || "current";
  const isOrgContext = !!activeOrganizationId;
  // Org "managers" (owner/admin) see and manage all org keys; the role comes
  // from the current user's membership summary on the org context.
  const isOrgManager =
    activeOrganization?.role === "owner" || activeOrganization?.role === "admin";
  const { data: membersData } = useOrganizationMembers(
    activeOrganizationId ?? "",
  );
  const [showCreateForm, setShowCreateForm] = useState(false);
  const [newKeyName, setNewKeyName] = useState("");
  const [newKeyDescription, setNewKeyDescription] = useState("");
  const [newKeyPurpose, setNewKeyPurpose] = useState<ApiKeyPurpose>("realtime");
  const [newKeyAssignee, setNewKeyAssignee] = useState<string>("self");
  const [newKeyRequestsPerSecond, setNewKeyRequestsPerSecond] = useState<
    number | ""
  >("");
  const [newKeyBurstSize, setNewKeyBurstSize] = useState<number | "">("");
  const [newKeyCapAmount, setNewKeyCapAmount] = useState("");
  const [newKeyCapInterval, setNewKeyCapInterval] = useState<
    SpendLimitInterval | "none"
  >("none");
  const [newKeyResponse, setNewKeyResponse] =
    useState<ApiKeyCreateResponse | null>(null);
  const [editModal, setEditModal] = useState<ApiKey | null>(null);
  const [editCapAmount, setEditCapAmount] = useState("");
  const [editCapInterval, setEditCapInterval] = useState<
    SpendLimitInterval | "none"
  >("none");
  const [deleteModal, setDeleteModal] = useState<{
    keyId: string;
    keyName: string;
  } | null>(null);
  const [copiedKey, setCopiedKey] = useState<string | null>(null);
  const [selectedKeys, setSelectedKeys] = useState<any[]>([]);
  const [showBulkDeleteModal, setShowBulkDeleteModal] = useState(false);
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [rotateModal, setRotateModal] = useState<ApiKey | null>(null);
  const [rotatedKey, setRotatedKey] = useState<{
    name: string;
    key: string;
  } | null>(null);
  // Manager-only view scoping (org context): tab + per-member filter,
  // applied client-side since the API already returns all org keys to
  // managers.
  const [keyScope, setKeyScope] = useState<"all" | "mine">("all");
  const [memberFilter, setMemberFilter] = useState<string>("all");

  // Check if user is a platform manager
  const isPlatformManager = user?.roles?.includes("PlatformManager") || false;
  // Per-member key governance: can the current user create/manage their own
  // keys in this context? Personal context is always true; in org context the
  // server-computed can_manage_keys flag decides (owners/admins are always
  // true, plain members only when granted the additive role).
  const canSelfManage = !isOrgContext || !!activeOrganization?.can_manage_keys;
  const showScopingControls = isOrgContext && isOrgManager;
  const pagination = useServerPagination();
  const {
    data: apiKeysData,
    isLoading,
    error,
  } = useApiKeys(targetUserId, {
    ...pagination.queryParams,
  });

  const apiKeys = apiKeysData?.data || [];

  const orgMembers = (membersData ?? []).filter(
    (m) => m.status === "active" && m.user,
  );
  const memberLabel = (memberUser: { display_name?: string; email: string }) =>
    memberUser.display_name || memberUser.email;
  const resolveAssignee = (createdBy: string) => {
    const member = orgMembers.find((m) => m.user!.id === createdBy);
    if (member) return memberLabel(member.user!);
    // Holder is no longer an active member (or list still loading): fall
    // back to a shortened UUID.
    return `${createdBy.slice(0, 8)}…`;
  };

  const displayedKeys = showScopingControls
    ? apiKeys.filter((key) => {
        if (keyScope === "mine") return key.created_by === user?.id;
        return memberFilter === "all" || key.created_by === memberFilter;
      })
    : apiKeys;

  const createApiKeyMutation = useCreateApiKey();
  const deleteApiKeyMutation = useDeleteApiKey();
  const updateApiKeyMutation = useUpdateApiKey();
  const rotateApiKeyMutation = useRotateApiKey();

  const resetCreateForm = () => {
    setShowCreateForm(false);
    setNewKeyName("");
    setNewKeyDescription("");
    setNewKeyPurpose("realtime");
    setNewKeyAssignee("self");
    setNewKeyRequestsPerSecond("");
    setNewKeyBurstSize("");
    setNewKeyCapAmount("");
    setNewKeyCapInterval("none");
    setNewKeyResponse(null);
    setAdvancedOpen(false);
  };

  const handleCreateApiKey = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!newKeyName.trim()) return;

    // Managers can issue an org key to another member; the key lands in
    // that member's view (created_by = member). Only send member_id when a
    // member other than self is selected.
    const assignedMemberId =
      isOrgContext &&
      isOrgManager &&
      newKeyAssignee !== "self" &&
      newKeyAssignee !== user?.id
        ? newKeyAssignee
        : undefined;

    const newKey = await createApiKeyMutation.mutateAsync({
      data: {
        name: newKeyName.trim(),
        description: newKeyDescription.trim() || undefined,
        purpose: newKeyPurpose,
        member_id: assignedMemberId,
        requests_per_second:
          newKeyRequestsPerSecond === ""
            ? null
            : Number(newKeyRequestsPerSecond),
        burst_size: newKeyBurstSize === "" ? null : Number(newKeyBurstSize),
        spend_limit: newKeyCapAmount.trim() === "" ? null : newKeyCapAmount.trim(),
        spend_limit_interval:
          newKeyCapAmount.trim() === "" || newKeyCapInterval === "none"
            ? null
            : newKeyCapInterval,
      },
      userId: targetUserId,
    });

    setNewKeyResponse(newKey);
    // Don't close the form - show success state instead
  };

  const openEditModal = (apiKey: ApiKey) => {
    setEditModal(apiKey);
    setEditCapAmount(apiKey.spend_limit ?? "");
    setEditCapInterval(apiKey.spend_limit_interval ?? "none");
  };

  const handleSaveCap = async () => {
    if (!editModal) return;
    try {
      // Tri-state PATCH: both cap fields are always sent explicitly so removal
      // (null) actually clears server-side, matching the API semantics.
      await updateApiKeyMutation.mutateAsync({
        keyId: editModal.id,
        data: {
          spend_limit: editCapAmount.trim() === "" ? null : editCapAmount.trim(),
          spend_limit_interval:
            editCapAmount.trim() === "" || editCapInterval === "none"
              ? null
              : editCapInterval,
        },
        userId: targetUserId,
      });
      toast.success("Usage limit updated");
      setEditModal(null);
    } catch (e) {
      console.error("Failed to update usage limit:", e);
      toast.error((e as Error)?.message ?? "Failed to update usage limit");
    }
  };

  const handleResetWindow = async () => {
    if (!editModal) return;
    try {
      await updateApiKeyMutation.mutateAsync({
        keyId: editModal.id,
        data: { reset_window: true },
        userId: targetUserId,
      });
      toast.success("Spend window reset");
      setEditModal(null);
    } catch (e) {
      console.error("Failed to reset spend window:", e);
      toast.error((e as Error)?.message ?? "Failed to reset spend window");
    }
  };

  const handleDeleteApiKey = (keyId: string) => {
    deleteApiKeyMutation.mutate(
      {
        keyId,
        userId: targetUserId,
      },
      {
        onSuccess: () => setDeleteModal(null),
      },
    );
  };

  const handleDeleteFromTable = (apiKey: any) => {
    setDeleteModal({
      keyId: apiKey.id,
      keyName: apiKey.name,
    });
  };

  const handleBulkDelete = async () => {
    try {
      // Delete keys one by one
      for (const key of selectedKeys) {
        await deleteApiKeyMutation.mutateAsync({
          keyId: key.id,
          userId: targetUserId,
        });
      }
      setSelectedKeys([]);
      setShowBulkDeleteModal(false);
    } catch (error) {
      console.error("Error deleting API keys:", error);
    }
  };

  const copyToClipboard = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text);
      setCopiedKey(text);
      toast.success("API key copied to clipboard");

      // Reset the copied state after 2 seconds
      setTimeout(() => {
        setCopiedKey(null);
      }, 2000);
    } catch (err) {
      console.error("Failed to copy to clipboard:", err);
      toast.error("Failed to copy API key");
    }
  };

  const handleRotateKey = async () => {
    if (!rotateModal) return;
    try {
      const { key } = await rotateApiKeyMutation.mutateAsync({
        keyId: rotateModal.id,
        userId: targetUserId,
      });
      // One-time secret display, same pattern as creation.
      setRotatedKey({ name: rotateModal.name, key });
      setRotateModal(null);
    } catch (e) {
      console.error("Failed to rotate API key:", e);
      toast.error((e as Error)?.message ?? "Failed to rotate API key");
    }
  };

  // Mirrors what the server permits for edit/rename/delete: a
  // PlatformManager, an org owner/admin acting on any key of the active org,
  // or the key's creator when they hold self-manage rights. Keep this gate in
  // lockstep with the server.
  const canManageKey = (apiKey: ApiKey) =>
    isPlatformManager ||
    (isOrgContext && isOrgManager) ||
    (!!apiKey.created_by && apiKey.created_by === user?.id && canSelfManage);

  // Rotation is the secret-recovery path: members without creation rights can
  // still rotate keys they hold to get a fresh secret (shown once).
  const canRotateKey = (apiKey: ApiKey) =>
    canManageKey(apiKey) ||
    (!!apiKey.created_by && apiKey.created_by === user?.id);

  const columns = createColumns({
    onDelete: handleDeleteFromTable,
    onEdit: openEditModal,
    onRotate: setRotateModal,
    canManage: canManageKey,
    canRotate: canRotateKey,
    isPlatformManager,
    showAssignee: isOrgContext,
    resolveAssignee: isOrgContext ? resolveAssignee : undefined,
    showSelect: canSelfManage,
  });

  if (isLoading) {
    return (
      <div className="p-6">
        <div className="animate-pulse">
          <div className="h-8 bg-gray-200 rounded w-48 mb-6"></div>
          <div className="bg-white rounded-lg shadow-sm border border-gray-200 p-6">
            <div className="space-y-4">
              {[...Array(3)].map((_, i) => (
                <div key={i} className="h-16 bg-gray-200 rounded"></div>
              ))}
            </div>
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="p-4 md:p-6">
      <div className="mb-8">
        <div className="flex flex-col sm:flex-row items-start sm:items-center justify-between gap-4">
          <div>
            <h1 className="text-2xl md:text-3xl font-bold text-doubleword-neutral-900 mb-2">
              API Keys
            </h1>
            <p className="text-sm md:text-base text-doubleword-neutral-600">
              Manage your API keys for programmatic access
            </p>
          </div>
          {apiKeys.length > 0 && canSelfManage && (
            <Button
              onClick={() => setShowCreateForm(true)}
              className="bg-doubleword-background-dark hover:bg-doubleword-neutral-900 w-full sm:w-auto"
              aria-label="Create new API key"
            >
              <Plus className="w-4 h-4 mr-2" />
              Create API Key
            </Button>
          )}
        </div>
      </div>

      {!canSelfManage && (
        <div
          className="mb-6 flex items-center gap-2 rounded-lg border border-doubleword-neutral-200 bg-doubleword-neutral-50 px-4 py-3 text-sm text-doubleword-neutral-600"
          role="status"
        >
          <Info className="h-4 w-4 shrink-0" />
          <span>
            API keys in this organization are issued by its admins. You can
            rotate a key you hold to get a fresh secret.
          </span>
        </div>
      )}

      {error && (
        <div className="mb-6 bg-red-50 border border-red-200 text-red-700 px-4 py-3 rounded-lg">
          {(error as Error)?.message || "An error occurred"}
        </div>
      )}

      {(createApiKeyMutation.isSuccess || deleteApiKeyMutation.isSuccess) &&
        !createApiKeyMutation.isPending &&
        !deleteApiKeyMutation.isPending && (
          <div className="mb-6 bg-green-50 border border-green-200 text-green-700 px-4 py-3 rounded-lg">
            {createApiKeyMutation.isSuccess
              ? "API key created successfully!"
              : "API key deleted successfully"}
          </div>
        )}

      {(createApiKeyMutation.error || deleteApiKeyMutation.error) && (
        <div className="mb-6 bg-red-50 border border-red-200 text-red-700 px-4 py-3 rounded-lg">
          {(createApiKeyMutation.error as Error)?.message ||
            (deleteApiKeyMutation.error as Error)?.message ||
            "An error occurred"}
        </div>
      )}

      {showScopingControls && apiKeys.length > 0 && (
        <div className="mb-4 flex flex-wrap items-center gap-3">
          <Tabs
            value={keyScope}
            onValueChange={(value) => setKeyScope(value as "all" | "mine")}
          >
            <TabsList aria-label="Key scope">
              <TabsTrigger value="all">All keys</TabsTrigger>
              <TabsTrigger value="mine">My keys</TabsTrigger>
            </TabsList>
          </Tabs>
          {keyScope === "all" && (
            <Select value={memberFilter} onValueChange={setMemberFilter}>
              <SelectTrigger className="w-56" aria-label="Filter by member">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="all">All members</SelectItem>
                {orgMembers.map((member) => (
                  <SelectItem key={member.user!.id} value={member.user!.id}>
                    {memberLabel(member.user!)}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          )}
        </div>
      )}

      {apiKeys.length > 0 ? (
        <DataTable
          columns={columns}
          data={displayedKeys}
          searchPlaceholder="Search API keys..."
          searchColumn="name"
          onSelectionChange={setSelectedKeys}
          actionBar={
            <div className="bg-blue-50 border border-blue-200 rounded-lg p-3 mb-4 flex items-center justify-between">
              <div className="flex items-center gap-2">
                <span className="text-sm font-medium text-blue-900">
                  {selectedKeys.length} key
                  {selectedKeys.length !== 1 ? "s" : ""} selected
                </span>
              </div>
              <div className="flex items-center gap-2">
                <button
                  onClick={() => setShowBulkDeleteModal(true)}
                  className="flex items-center gap-1 px-3 py-1.5 bg-red-600 text-white text-sm rounded-md hover:bg-red-700 transition-colors"
                  aria-label={`Delete ${selectedKeys.length} selected API key${selectedKeys.length !== 1 ? "s" : ""}`}
                >
                  <Trash2 className="w-4 h-4" />
                  Delete Selected
                </button>
              </div>
            </div>
          }
          paginationMode="server"
          serverPagination={{
            page: pagination.page,
            pageSize: pagination.pageSize,
            totalItems: apiKeysData?.total_count || 0,
            onPageChange: (page: number) => pagination.handlePageChange(page),
            onPageSizeChange: (pageSize: number) =>
              pagination.handlePageSizeChange(pageSize),
          }}
        />
      ) : (
        <div
          className="text-center py-12"
          role="status"
          aria-label="No API keys"
        >
          <div className="p-4 bg-doubleword-neutral-100 rounded-full w-16 h-16 mx-auto mb-4 flex items-center justify-center">
            <Key className="w-8 h-8 text-doubleword-neutral-600" />
          </div>
          <h3
            className="text-lg font-medium text-doubleword-neutral-900 mb-2"
            role="heading"
            aria-level={3}
          >
            No API keys configured
          </h3>
          <p className="text-doubleword-neutral-600 mb-6">
            {canSelfManage
              ? "Create your first API key to start using the API"
              : "An org admin can issue an API key for you"}
          </p>
          {canSelfManage && (
            <Button
              onClick={() => setShowCreateForm(true)}
              className="bg-doubleword-background-dark hover:bg-doubleword-neutral-900"
              aria-label="Create first API key"
            >
              <Plus className="w-4 h-4 mr-2" />
              Create API Key
            </Button>
          )}
        </div>
      )}

      {/* Create/Success Modal */}
      <Dialog
        open={showCreateForm}
        onOpenChange={(open) => {
          if (!open) {
            resetCreateForm();
          } else {
            setShowCreateForm(true);
          }
        }}
      >
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>
              {newKeyResponse ? "API Key Created Successfully" : "Create API key"}
            </DialogTitle>
            <DialogDescription>
              Give the key a recognizable name. You'll see the secret value
              once on the next screen.
            </DialogDescription>
          </DialogHeader>

          {newKeyResponse ? (
            <>
              <div className="space-y-4">
                <div
                  className="p-3 bg-green-50 border border-green-200 rounded-lg"
                  role="alert"
                >
                  <div className="flex items-center gap-2">
                    <Key className="w-4 h-4 text-green-600" />
                    <p className="text-sm text-green-800 font-medium">
                      Save this key - it won't be shown again
                    </p>
                  </div>
                </div>

                <div className="space-y-2">
                  <Label>Key Name</Label>
                  <p className="text-sm text-gray-900">{newKeyResponse.name}</p>
                </div>

                <div className="space-y-2">
                  <Label>API Key</Label>
                  <div className="flex items-center gap-2">
                    <div className="flex-1 overflow-hidden rounded border bg-gray-50">
                      <code className="block text-xs font-mono px-3 py-2 overflow-x-auto whitespace-nowrap">
                        {newKeyResponse.key}
                      </code>
                    </div>
                    <Button
                      type="button"
                      variant="outline"
                      size="icon"
                      onClick={() => copyToClipboard(newKeyResponse.key)}
                      aria-label={
                        copiedKey === newKeyResponse.key
                          ? "API key copied"
                          : "Copy API key"
                      }
                    >
                      {copiedKey === newKeyResponse.key ? (
                        <Check className="h-4 w-4 text-green-600" />
                      ) : (
                        <Copy className="h-4 w-4" />
                      )}
                    </Button>
                  </div>
                </div>
              </div>

              <DialogFooter>
                <Button onClick={resetCreateForm} className="w-full sm:w-auto">
                  Done
                </Button>
              </DialogFooter>
            </>
          ) : (
            <>
              <form
                id="create-key-form"
                onSubmit={handleCreateApiKey}
                className="space-y-4"
              >
                <div className="space-y-2">
                  <Label htmlFor="keyName">Name</Label>
                  <Input
                    id="keyName"
                    type="text"
                    value={newKeyName}
                    onChange={(e) => setNewKeyName(e.target.value)}
                    placeholder="Production worker"
                    required
                  />
                </div>

                <div className="space-y-2">
                  <Label htmlFor="keyDescription">
                    Description{" "}
                    <span className="font-normal text-doubleword-neutral-400">
                      (optional)
                    </span>
                  </Label>
                  <Textarea
                    id="keyDescription"
                    value={newKeyDescription}
                    onChange={(e) => setNewKeyDescription(e.target.value)}
                    placeholder="Where will this key be used?"
                    rows={2}
                    className="resize-none"
                  />
                </div>

                {/* Issue-to-member — managers only, org context only */}
                {isOrgContext && isOrgManager && (
                  <div className="space-y-2">
                    <Label>
                      Assign to member{" "}
                      <span className="font-normal text-doubleword-neutral-400">
                        (optional)
                      </span>
                    </Label>
                    <Select
                      value={newKeyAssignee}
                      onValueChange={setNewKeyAssignee}
                    >
                      <SelectTrigger
                        className="w-full"
                        aria-label="Assign to member"
                      >
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectItem value="self">Myself</SelectItem>
                        {orgMembers
                          .filter((member) => member.user!.id !== user?.id)
                          .map((member) => (
                            <SelectItem
                              key={member.user!.id}
                              value={member.user!.id}
                            >
                              {memberLabel(member.user!)}
                            </SelectItem>
                          ))}
                      </SelectContent>
                    </Select>
                    <p className="text-xs text-doubleword-neutral-500">
                      The key appears in the assigned member's view, and they
                      can rotate it from their API Keys page to get a secret
                      of their own.
                    </p>
                  </div>
                )}

                {/* Key type — card-style choice, matching the design */}
                <div className="space-y-2">
                  <Label>Key type</Label>
                  <div role="radiogroup" aria-label="Key type" className="space-y-2">
                    {(
                      [
                        {
                          value: "realtime",
                          title: "Inference",
                          desc: "Calls chat completions, embeddings, responses, and batches.",
                        },
                        {
                          value: "platform",
                          title: "Platform",
                          desc: "Hits the management API. Required by dw-cli and other tools that read or change account settings.",
                        },
                      ] as const
                    ).map((opt) => (
                      <button
                        key={opt.value}
                        type="button"
                        role="radio"
                        aria-checked={newKeyPurpose === opt.value}
                        aria-label={opt.title}
                        onClick={() =>
                          setNewKeyPurpose(opt.value as ApiKeyPurpose)
                        }
                        className={`w-full rounded-lg border p-3 text-left transition-colors ${
                          newKeyPurpose === opt.value
                            ? "border-doubleword-neutral-900 ring-1 ring-doubleword-neutral-900 bg-doubleword-neutral-50"
                            : "border-doubleword-neutral-200 hover:border-doubleword-neutral-300"
                        }`}
                      >
                        <div className="font-medium text-doubleword-neutral-900">
                          {opt.title}
                        </div>
                        <div className="text-sm text-doubleword-neutral-600">
                          {opt.desc}
                        </div>
                      </button>
                    ))}
                  </div>
                </div>

                {/* Usage Limit */}
                <div className="space-y-2">
                  <Label htmlFor="usageLimit">
                    Usage Limit{" "}
                    <span className="font-normal text-doubleword-neutral-400">
                      (optional)
                    </span>
                  </Label>
                  <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
                    <div className="relative">
                      <span className="absolute left-3 top-1/2 -translate-y-1/2 text-sm text-doubleword-neutral-400">
                        $
                      </span>
                      <Input
                        id="usageLimit"
                        type="number"
                        min="0.01"
                        step="any"
                        className="pl-7"
                        value={newKeyCapAmount}
                        onChange={(e) => setNewKeyCapAmount(e.target.value)}
                        placeholder="Amount"
                        aria-label="Usage limit amount"
                      />
                    </div>
                    <Select
                      value={newKeyCapInterval}
                      onValueChange={(value) =>
                        setNewKeyCapInterval(value as SpendLimitInterval | "none")
                      }
                      disabled={newKeyCapAmount.trim() === ""}
                    >
                      <SelectTrigger
                        className="w-full"
                        aria-label="Usage limit reset period"
                      >
                        <SelectValue placeholder="No reset (N/A)" />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectItem value="none">No reset (N/A)</SelectItem>
                        <SelectItem value="daily">Daily</SelectItem>
                        <SelectItem value="weekly">Weekly</SelectItem>
                        <SelectItem value="monthly">Monthly</SelectItem>
                      </SelectContent>
                    </Select>
                  </div>
                  <p className="text-xs text-doubleword-neutral-500">
                    Restrict how much API credit this specific key can consume.
                    Leave amount blank for no limit.
                    {newKeyCapAmount.trim() !== "" &&
                      ` ${resetPreviewLine(
                        newKeyCapInterval === "none" ? null : newKeyCapInterval,
                      )}`}
                  </p>
                </div>

                {/* Advanced Settings (Rate Limiting, PM-only) - Collapsible */}
                {isPlatformManager && (
                  <Collapsible
                    open={advancedOpen}
                    onOpenChange={setAdvancedOpen}
                  >
                    <CollapsibleTrigger asChild>
                      <button
                        type="button"
                        className="flex items-center gap-2 w-full text-sm font-medium text-gray-700 hover:text-gray-900 transition-colors group"
                      >
                        <ChevronDown
                          className={`h-4 w-4 text-gray-400 transition-transform duration-200 ${
                            advancedOpen ? "transform rotate-180" : ""
                          }`}
                        />
                        <span>Advanced Settings</span>
                        <div className="flex-1 h-px bg-gray-200 group-hover:bg-gray-300 transition-colors" />
                      </button>
                    </CollapsibleTrigger>
                    <CollapsibleContent className="space-y-3 pt-4">
                      {/* Rate Limiting */}
                      <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
                        <div className="space-y-2">
                          <div className="flex items-center gap-1">
                            <Label htmlFor="requestsPerSecond">
                              Requests/Second
                            </Label>
                            <HoverCard openDelay={200} closeDelay={100}>
                              <HoverCardTrigger asChild>
                                <button
                                  type="button"
                                  className="text-gray-400 hover:text-gray-600 transition-colors"
                                  onFocus={(e) => e.preventDefault()}
                                  tabIndex={-1}
                                >
                                  <Info className="h-4 w-4" />
                                  <span className="sr-only">
                                    Requests per second information
                                  </span>
                                </button>
                              </HoverCardTrigger>
                              <HoverCardContent className="w-80" sideOffset={5}>
                                <p className="text-sm text-muted-foreground">
                                  Maximum number of requests allowed per second
                                  for this API key. Leave blank for no limit.
                                </p>
                              </HoverCardContent>
                            </HoverCard>
                          </div>
                          <Input
                            id="requestsPerSecond"
                            type="number"
                            min="1"
                            max="10000"
                            step="1"
                            value={newKeyRequestsPerSecond}
                            onChange={(e) =>
                              setNewKeyRequestsPerSecond(
                                e.target.value === ""
                                  ? ""
                                  : Number(e.target.value),
                              )
                            }
                            placeholder="None"
                          />
                        </div>

                        <div className="space-y-2">
                          <div className="flex items-center gap-1">
                            <Label htmlFor="burstSize">Burst Size</Label>
                            <HoverCard openDelay={200} closeDelay={100}>
                              <HoverCardTrigger asChild>
                                <button
                                  type="button"
                                  className="text-gray-400 hover:text-gray-600 transition-colors"
                                  onFocus={(e) => e.preventDefault()}
                                  tabIndex={-1}
                                >
                                  <Info className="h-4 w-4" />
                                  <span className="sr-only">
                                    Burst size information
                                  </span>
                                </button>
                              </HoverCardTrigger>
                              <HoverCardContent className="w-80" sideOffset={5}>
                                <p className="text-sm text-muted-foreground">
                                  Maximum burst capacity for rate limiting. This
                                  allows temporary spikes above the per-second
                                  rate. Leave blank for no limit.
                                </p>
                              </HoverCardContent>
                            </HoverCard>
                          </div>
                          <Input
                            id="burstSize"
                            type="number"
                            min="1"
                            max="50000"
                            step="1"
                            value={newKeyBurstSize}
                            onChange={(e) =>
                              setNewKeyBurstSize(
                                e.target.value === ""
                                  ? ""
                                  : Number(e.target.value),
                              )
                            }
                            placeholder="None"
                          />
                        </div>
                      </div>
                    </CollapsibleContent>
                  </Collapsible>
                )}
              </form>

              <DialogFooter>
                <Button type="button" variant="outline" onClick={resetCreateForm}>
                  Cancel
                </Button>
                <Button
                  type="submit"
                  form="create-key-form"
                  disabled={
                    createApiKeyMutation.isPending || !newKeyName.trim()
                  }
                >
                  {createApiKeyMutation.isPending && (
                    <Loader2 className="w-4 h-4 mr-2 animate-spin" />
                  )}
                  Create Key
                </Button>
              </DialogFooter>
            </>
          )}
        </DialogContent>
      </Dialog>

      {/* Delete Confirmation Modal */}
      <Dialog open={!!deleteModal} onOpenChange={() => setDeleteModal(null)}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <div className="flex items-center gap-3">
              <div className="w-10 h-10 bg-red-100 rounded-full flex items-center justify-center">
                <Trash2 className="w-5 h-5 text-red-600" />
              </div>
              <div>
                <DialogTitle>Delete API Key</DialogTitle>
                <DialogDescription>
                  This action cannot be undone
                </DialogDescription>
              </div>
            </div>
          </DialogHeader>

          <div className="py-4">
            <p className="text-sm text-gray-700">
              Are you sure you want to delete the API key{" "}
              <strong>"{deleteModal?.keyName}"</strong>? Any applications using
              this key will lose access immediately.
            </p>
          </div>

          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => setDeleteModal(null)}
            >
              Cancel
            </Button>
            <Button
              onClick={() =>
                deleteModal && handleDeleteApiKey(deleteModal.keyId)
              }
              disabled={deleteApiKeyMutation.isPending}
              variant="destructive"
            >
              {deleteApiKeyMutation.isPending && (
                <Loader2 className="w-4 h-4 mr-2 animate-spin" />
              )}
              Delete API Key
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* Edit Usage Limit Modal */}
      <Dialog
        open={!!editModal}
        onOpenChange={(open) => {
          if (!open) setEditModal(null);
        }}
      >
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>Edit Usage Limit</DialogTitle>
            <DialogDescription>
              Update limits for <strong>{editModal?.name}</strong>
            </DialogDescription>
          </DialogHeader>

          {editModal?.spend_limit != null && (
            <div className="rounded-lg bg-doubleword-neutral-50 border border-doubleword-neutral-200 p-3 text-sm text-doubleword-neutral-700">
              Spent {formatCredits(editModal.spend ?? "0")} of{" "}
              {formatCredits(editModal.spend_limit)} this window
              {editModal.resets_at
                ? ` · resets ${formatResetInstant(editModal.resets_at)}`
                : " · no automatic reset"}
            </div>
          )}

          <div className="space-y-2">
            <Label htmlFor="editUsageLimit">
              Usage Limit{" "}
              <span className="font-normal text-doubleword-neutral-400">
                (optional)
              </span>
            </Label>
            <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
              <div className="relative">
                <span className="absolute left-3 top-1/2 -translate-y-1/2 text-sm text-doubleword-neutral-400">
                  $
                </span>
                <Input
                  id="editUsageLimit"
                  type="number"
                  min="0.01"
                  step="any"
                  className="pl-7"
                  value={editCapAmount}
                  onChange={(e) => setEditCapAmount(e.target.value)}
                  placeholder="Amount"
                  aria-label="Usage limit amount"
                />
              </div>
              <Select
                value={editCapInterval}
                onValueChange={(value) =>
                  setEditCapInterval(value as SpendLimitInterval | "none")
                }
                disabled={editCapAmount.trim() === ""}
              >
                <SelectTrigger
                  className="w-full"
                  aria-label="Usage limit reset period"
                >
                  <SelectValue placeholder="No reset (N/A)" />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="none">No reset (N/A)</SelectItem>
                  <SelectItem value="daily">Daily</SelectItem>
                  <SelectItem value="weekly">Weekly</SelectItem>
                  <SelectItem value="monthly">Monthly</SelectItem>
                </SelectContent>
              </Select>
            </div>
            <p className="text-xs text-doubleword-neutral-500">
              Restrict how much API credit this specific key can consume. Leave
              amount blank for no limit.
              {editCapAmount.trim() !== "" &&
                ` ${resetPreviewLine(
                  editCapInterval === "none" ? null : editCapInterval,
                )}`}
            </p>
          </div>

          <DialogFooter className="gap-2 sm:justify-between">
            {editModal?.spend_limit != null ? (
              <Button
                type="button"
                variant="outline"
                onClick={handleResetWindow}
                disabled={updateApiKeyMutation.isPending}
                aria-label="Reset spend window now"
              >
                Reset window now
              </Button>
            ) : (
              <span />
            )}
            <div className="flex gap-2">
              <Button
                type="button"
                variant="outline"
                onClick={() => setEditModal(null)}
                disabled={updateApiKeyMutation.isPending}
              >
                Cancel
              </Button>
              <Button
                type="button"
                onClick={handleSaveCap}
                disabled={updateApiKeyMutation.isPending}
              >
                {updateApiKeyMutation.isPending && (
                  <Loader2 className="w-4 h-4 mr-2 animate-spin" />
                )}
                Save changes
              </Button>
            </div>
          </DialogFooter>
        </DialogContent>
      </Dialog>


      {/* Rotate Confirmation Modal */}
      <Dialog
        open={!!rotateModal}
        onOpenChange={(open) => {
          if (!open) setRotateModal(null);
        }}
      >
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <div className="flex items-center gap-3">
              <div className="w-10 h-10 bg-doubleword-neutral-100 rounded-full flex items-center justify-center">
                <RefreshCw className="w-5 h-5 text-doubleword-neutral-700" />
              </div>
              <div>
                <DialogTitle>Rotate API Key</DialogTitle>
                <DialogDescription>
                  Generate a new secret for{" "}
                  <strong>{rotateModal?.name}</strong>
                </DialogDescription>
              </div>
            </div>
          </DialogHeader>

          <div className="space-y-4 py-2">
            <p className="text-sm text-gray-700">
              The new secret is live immediately, and the old secret stops
              working for new requests within about a second. The key itself
              is unchanged — its usage limit, counted spend, and attribution
              all carry over.
            </p>
            <div className="p-3 bg-yellow-50 border border-yellow-200 rounded-lg">
              <p className="text-sm text-yellow-800">
                <strong>Note:</strong> batches already submitted with this key
                will keep running — they execute on an internal system key. If
                this key was compromised, cancel any in-flight batches
                separately.
              </p>
            </div>
          </div>

          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => setRotateModal(null)}
              disabled={rotateApiKeyMutation.isPending}
            >
              Cancel
            </Button>
            <Button
              type="button"
              onClick={handleRotateKey}
              disabled={rotateApiKeyMutation.isPending}
            >
              {rotateApiKeyMutation.isPending && (
                <Loader2 className="w-4 h-4 mr-2 animate-spin" />
              )}
              Rotate Key
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* Rotated Secret Modal — one-time display, same pattern as creation */}
      <Dialog
        open={!!rotatedKey}
        onOpenChange={(open) => {
          if (!open) setRotatedKey(null);
        }}
      >
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <DialogTitle>API Key Rotated</DialogTitle>
            <DialogDescription>
              <strong>{rotatedKey?.name}</strong> has a new secret. The old
              secret no longer works for new requests.
            </DialogDescription>
          </DialogHeader>

          <div className="space-y-4">
            <div
              className="p-3 bg-green-50 border border-green-200 rounded-lg"
              role="alert"
            >
              <div className="flex items-center gap-2">
                <Key className="w-4 h-4 text-green-600" />
                <p className="text-sm text-green-800 font-medium">
                  Save this key now - it won't be shown again
                </p>
              </div>
            </div>

            <div className="space-y-2">
              <Label>New API Key</Label>
              <div className="flex items-center gap-2">
                <div className="flex-1 overflow-hidden rounded border bg-gray-50">
                  <code className="block text-xs font-mono px-3 py-2 overflow-x-auto whitespace-nowrap">
                    {rotatedKey?.key}
                  </code>
                </div>
                <Button
                  type="button"
                  variant="outline"
                  size="icon"
                  onClick={() => rotatedKey && copyToClipboard(rotatedKey.key)}
                  aria-label={
                    rotatedKey && copiedKey === rotatedKey.key
                      ? "API key copied"
                      : "Copy API key"
                  }
                >
                  {rotatedKey && copiedKey === rotatedKey.key ? (
                    <Check className="h-4 w-4 text-green-600" />
                  ) : (
                    <Copy className="h-4 w-4" />
                  )}
                </Button>
              </div>
            </div>
          </div>

          <DialogFooter>
            <Button
              onClick={() => setRotatedKey(null)}
              className="w-full sm:w-auto"
            >
              Done
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* Bulk Delete Confirmation Modal */}
      <Dialog open={showBulkDeleteModal} onOpenChange={setShowBulkDeleteModal}>
        <DialogContent className="sm:max-w-md">
          <DialogHeader>
            <div className="flex items-center gap-3">
              <div className="w-10 h-10 bg-red-100 rounded-full flex items-center justify-center">
                <Trash2 className="w-5 h-5 text-red-600" />
              </div>
              <div>
                <DialogTitle>Delete API Keys</DialogTitle>
                <DialogDescription>
                  This action cannot be undone
                </DialogDescription>
              </div>
            </div>
          </DialogHeader>

          <div className="space-y-4">
            <p className="text-gray-700">
              Are you sure you want to delete{" "}
              <strong>{selectedKeys.length}</strong> API key
              {selectedKeys.length !== 1 ? "s" : ""}?
            </p>

            <div className="bg-gray-50 rounded-lg p-3 max-h-32 overflow-y-auto">
              <p className="text-sm font-medium text-gray-600 mb-2">
                Keys to be deleted:
              </p>
              <ul className="text-sm text-gray-700 space-y-1">
                {selectedKeys.map((key) => (
                  <li key={key.id} className="flex justify-between">
                    <span>{key.name}</span>
                    <span className="text-gray-500">
                      {key.description || "No description"}
                    </span>
                  </li>
                ))}
              </ul>
            </div>

            <div className="p-3 bg-yellow-50 border border-yellow-200 rounded-lg">
              <p className="text-sm text-yellow-800">
                <strong>Warning:</strong> This will permanently delete{" "}
                {selectedKeys.length > 1 ? "these API keys" : "this API key"}{" "}
                and any applications using{" "}
                {selectedKeys.length > 1 ? "them" : "it"} will lose access
                immediately.
              </p>
            </div>
          </div>

          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              onClick={() => setShowBulkDeleteModal(false)}
              disabled={deleteApiKeyMutation.isPending}
            >
              Cancel
            </Button>
            <Button
              type="button"
              variant="destructive"
              onClick={handleBulkDelete}
              disabled={deleteApiKeyMutation.isPending}
            >
              {deleteApiKeyMutation.isPending ? (
                <>
                  <Loader2 className="w-4 h-4 mr-2 animate-spin" />
                  Deleting...
                </>
              ) : (
                <>
                  <Trash2 className="w-4 h-4 mr-2" />
                  Delete {selectedKeys.length} Key
                  {selectedKeys.length !== 1 ? "s" : ""}
                </>
              )}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
};
