import React, { useState } from "react";
import {
  Loader2,
  Plus,
  Pencil,
  Trash2,
  RotateCw,
  AlertTriangle,
  Copy,
  Check,
  Globe,
  Mail,
} from "lucide-react";
import { useNavigate } from "react-router-dom";
import {
  useUser,
  useUpdateUser,
  useUpdateOrganization,
  useWebhooks,
  useCreateWebhook,
  useUpdateWebhook,
  useDeleteWebhook,
  useRotateWebhookSecret,
} from "../../../api/control-layer/hooks";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
  HoverCard,
  HoverCardContent,
  HoverCardTrigger,
} from "../../ui";
import { Input } from "../../ui/input";
import { Button } from "../../ui/button";
import { Switch } from "../../ui/switch";
import { Label } from "../../ui/label";
import { Badge } from "../../ui/badge";
import { Checkbox } from "../../ui/checkbox";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
  DialogFooter,
} from "../../ui/dialog";
import { Tabs, TabsList, TabsTrigger } from "../../ui/tabs";
import type { Webhook, WebhookScope } from "../../../api/control-layer/types";

const OWN_EVENT_TYPE_OPTIONS = [
  {
    value: "batch.completed",
    label: "Batch completed",
    example: `{
  "type": "batch.completed",
  "timestamp": "2025-01-15T10:30:00Z",
  "data": {
    "batch_id": "batch_abc123",
    "status": "completed",
    "request_counts": {
      "total": 150,
      "completed": 150,
      "failed": 0,
      "cancelled": 0
    },
    "output_file_id": "file_def456",
    "created_at": "2025-01-15T09:00:00Z",
    "finished_at": "2025-01-15T10:30:00Z"
  }
}`,
  },
  {
    value: "batch.failed",
    label: "Batch failed",
    example: `{
  "type": "batch.failed",
  "timestamp": "2025-01-15T10:30:00Z",
  "data": {
    "batch_id": "batch_abc123",
    "status": "failed",
    "request_counts": {
      "total": 150,
      "completed": 80,
      "failed": 70,
      "cancelled": 0
    },
    "error_file_id": "file_ghi789",
    "created_at": "2025-01-15T09:00:00Z",
    "finished_at": "2025-01-15T10:30:00Z"
  }
}`,
  },
];

const PLATFORM_EVENT_TYPE_OPTIONS = [
  {
    value: "user.created",
    label: "User created",
    example: `{
  "type": "user.created",
  "timestamp": "2025-01-15T10:30:00Z",
  "data": {
    "user_id": "usr_abc123",
    "email": "jane@example.com",
    "auth_source": "native"
  }
}`,
  },
  {
    value: "batch.created",
    label: "Batch created",
    example: `{
  "type": "batch.created",
  "timestamp": "2025-01-15T10:30:00Z",
  "data": {
    "batch_id": "batch_abc123",
    "user_id": "usr_abc123",
    "endpoint": "/v1/chat/completions"
  }
}`,
  },
  {
    value: "api_key.created",
    label: "API key created",
    example: `{
  "type": "api_key.created",
  "timestamp": "2025-01-15T10:30:00Z",
  "data": {
    "api_key_id": "key_abc123",
    "user_id": "usr_abc123",
    "created_by": "usr_def456",
    "name": "My API Key"
  }
}`,
  },
];

function eventTypeOptionsForScope(scope: WebhookScope) {
  return scope === "platform"
    ? PLATFORM_EVENT_TYPE_OPTIONS
    : OWN_EVENT_TYPE_OPTIONS;
}

interface NotificationSettingsProps {
  /** User or organization ID to manage notifications for */
  userId: string;
  /** Whether to show platform webhook scope option */
  showPlatformScope?: boolean;
  /** Whether the userId refers to an organization */
  isOrganization?: boolean;
}

export const NotificationSettings: React.FC<NotificationSettingsProps> = ({
  userId,
  showPlatformScope = false,
  isOrganization = false,
}) => {
  const { data: user, refetch: refetchUser } = useUser(userId);
  const updateUserMutation = useUpdateUser();
  const updateOrgMutation = useUpdateOrganization();
  const navigate = useNavigate();

  const { data: webhooks } = useWebhooks(userId);
  const createWebhookMutation = useCreateWebhook();
  const updateWebhookMutation = useUpdateWebhook();
  const deleteWebhookMutation = useDeleteWebhook();
  const rotateSecretMutation = useRotateWebhookSecret();

  const [webhookDialogOpen, setWebhookDialogOpen] = useState(false);
  const [editingWebhook, setEditingWebhook] = useState<Webhook | null>(null);
  const [webhookUrl, setWebhookUrl] = useState("");
  const [webhookDescription, setWebhookDescription] = useState("");
  const [webhookEventTypes, setWebhookEventTypes] = useState<string[]>([]);
  const [webhookSecret, setWebhookSecret] = useState<string | null>(null);
  const [webhookError, setWebhookError] = useState("");
  const [deletingWebhookId, setDeletingWebhookId] = useState<string | null>(
    null,
  );
  const [copiedSecret, setCopiedSecret] = useState(false);
  const [webhookScope, setWebhookScope] = useState<WebhookScope>("own");

  const updateSettings = async (data: Record<string, unknown>) => {
    if (!user) return;
    if (isOrganization) {
      await updateOrgMutation.mutateAsync({ id: user.id, data });
    } else {
      await updateUserMutation.mutateAsync({ id: user.id, data });
    }
    await refetchUser();
  };

  const handleEmailNotificationsToggle = async (enabled: boolean) => {
    if (!user) return;
    try {
      await updateSettings({ batch_notifications_enabled: enabled });
    } catch {
      // Revert will happen via refetchUser
    }
  };

  const handleLowBalanceToggle = async (enabled: boolean) => {
    if (!user) return;
    try {
      await updateSettings({ low_balance_threshold: enabled ? 2.0 : null });
    } catch {
      // Revert will happen via refetchUser
    }
  };

  const handleLowBalanceThresholdChange = async (value: string) => {
    if (!user) return;
    const num = parseFloat(value);
    if (isNaN(num) || num <= 0) return;
    try {
      await updateSettings({ low_balance_threshold: num });
    } catch {
      // Revert will happen via refetchUser
    }
  };

  const openCreateWebhookDialog = () => {
    setEditingWebhook(null);
    setWebhookScope("own");
    setWebhookUrl("");
    setWebhookDescription("");
    setWebhookEventTypes(OWN_EVENT_TYPE_OPTIONS.map((o) => o.value));
    setWebhookSecret(null);
    setWebhookError("");
    setWebhookDialogOpen(true);
  };

  const openEditWebhookDialog = (webhook: Webhook) => {
    const scope = webhook.scope || "own";
    setEditingWebhook(webhook);
    setWebhookScope(scope);
    setWebhookUrl(webhook.url);
    setWebhookDescription(webhook.description || "");
    const options = eventTypeOptionsForScope(scope);
    setWebhookEventTypes(
      webhook.event_types && webhook.event_types.length > 0
        ? webhook.event_types
        : options.map((o) => o.value),
    );
    setWebhookSecret(null);
    setWebhookError("");
    setWebhookDialogOpen(true);
  };

  const handleWebhookEventTypeToggle = (eventType: string) => {
    setWebhookEventTypes((prev) =>
      prev.includes(eventType)
        ? prev.filter((t) => t !== eventType)
        : [...prev, eventType],
    );
  };

  const handleScopeChange = (scope: WebhookScope) => {
    setWebhookScope(scope);
    const options = eventTypeOptionsForScope(scope);
    setWebhookEventTypes(options.map((o) => o.value));
  };

  const handleWebhookSave = async () => {
    setWebhookError("");

    if (!webhookUrl.trim()) {
      setWebhookError("URL is required");
      return;
    }

    try {
      new URL(webhookUrl);
    } catch {
      setWebhookError("Please enter a valid URL");
      return;
    }

    if (!webhookUrl.startsWith("https://")) {
      setWebhookError("URL must use HTTPS");
      return;
    }

    try {
      if (editingWebhook) {
        await updateWebhookMutation.mutateAsync({
          userId,
          webhookId: editingWebhook.id,
          data: {
            url: webhookUrl,
            description: webhookDescription.trim() || null,
            event_types: webhookEventTypes,
          },
        });
        setWebhookDialogOpen(false);
      } else {
        const result = await createWebhookMutation.mutateAsync({
          userId,
          data: {
            url: webhookUrl,
            description: webhookDescription.trim() || undefined,
            event_types: webhookEventTypes,
            scope: webhookScope,
          },
        });
        setWebhookSecret(result.secret);
      }
    } catch {
      setWebhookError(
        editingWebhook
          ? "Failed to update webhook"
          : "Failed to create webhook",
      );
    }
  };

  const handleWebhookDelete = async (webhookId: string) => {
    try {
      await deleteWebhookMutation.mutateAsync({ userId, webhookId });
      setDeletingWebhookId(null);
    } catch {
      // Error handled by mutation
    }
  };

  const handleWebhookToggle = async (webhook: Webhook) => {
    try {
      await updateWebhookMutation.mutateAsync({
        userId,
        webhookId: webhook.id,
        data: { enabled: !webhook.enabled },
      });
    } catch {
      // Error handled by mutation
    }
  };

  const handleRotateSecret = async (webhookId: string) => {
    try {
      const result = await rotateSecretMutation.mutateAsync({ userId, webhookId });
      setWebhookSecret(result.secret);
    } catch {
      // Error handled by mutation
    }
  };

  const handleCopySecret = async (secret: string) => {
    await navigator.clipboard.writeText(secret);
    setCopiedSecret(true);
    setTimeout(() => setCopiedSecret(false), 2000);
  };

  return (
    <>
      <div className="bg-white rounded-lg shadow-sm border border-gray-200 p-6">
        <h4 className="text-lg font-medium text-gray-900 mb-4">
          Notifications
        </h4>

        {/* Email Section */}
        <h5 className="text-sm font-medium text-gray-500 uppercase tracking-wide mb-3">
          Email
        </h5>

        {/* Batch Notifications Toggle */}
        <div className="flex items-center justify-between pb-4 border-b border-gray-200">
          <div className="flex items-center gap-3">
            <div className="p-2 bg-gray-100 rounded-lg">
              <Mail className="w-4 h-4 text-gray-600" />
            </div>
            <div>
              <Label
                htmlFor="emailNotifications"
                className="text-sm font-medium text-gray-900"
              >
                Batch Notifications
              </Label>
              <p className="text-xs text-gray-500 mt-0.5">
                Receive email when a batch completes or fails
              </p>
            </div>
          </div>
          <div className="flex items-center gap-2 shrink-0">
            <Switch
              id="emailNotifications"
              checked={user?.batch_notifications_enabled ?? false}
              onCheckedChange={handleEmailNotificationsToggle}
              disabled={updateUserMutation.isPending || updateOrgMutation.isPending}
              aria-label="Email notifications"
            />
          </div>
        </div>

        {/* Low Balance Alerts Toggle */}
        <div className="flex items-center justify-between py-4 border-b border-gray-200">
          <div className="flex items-center gap-3">
            <div className="p-2 bg-gray-100 rounded-lg">
              <AlertTriangle className="w-4 h-4 text-gray-600" />
            </div>
            <div>
              <Label
                htmlFor="lowBalanceNotifications"
                className="text-sm font-medium text-gray-900"
              >
                Low Balance Alerts
              </Label>
              <p className="text-xs text-gray-500 mt-0.5">
                Receive email when your balance drops below the specified threshold.{" "}
                <button
                  type="button"
                  className="text-blue-600 hover:text-blue-700 underline"
                  onClick={() => navigate("/cost-management")}
                >
                  Configure auto top-up
                </button>
              </p>
            </div>
          </div>
          <div className="flex items-center gap-2 shrink-0">
            {user?.low_balance_threshold != null && (
              <div className="flex items-center gap-1">
                <span className="text-xs text-gray-500">$</span>
                <Input
                  key={user.low_balance_threshold}
                  type="number"
                  min="0.01"
                  step="0.50"
                  className="w-20 h-7 text-xs [appearance:textfield] [&::-webkit-outer-spin-button]:appearance-none [&::-webkit-inner-spin-button]:appearance-none"
                  defaultValue={user.low_balance_threshold}
                  onBlur={(e) =>
                    handleLowBalanceThresholdChange(e.target.value)
                  }
                  disabled={updateUserMutation.isPending || updateOrgMutation.isPending}
                  aria-label="Low balance threshold"
                />
              </div>
            )}
            <Switch
              id="lowBalanceNotifications"
              checked={user?.low_balance_threshold != null}
              onCheckedChange={handleLowBalanceToggle}
              disabled={updateUserMutation.isPending || updateOrgMutation.isPending}
              aria-label="Low balance notifications"
            />
          </div>
        </div>

        {/* Webhooks Section */}
        <h5 className="text-sm font-medium text-gray-500 uppercase tracking-wide mt-6 mb-3">
          Webhooks
        </h5>
        <div>
          <div className="flex items-center justify-between mb-3">
            <div className="flex items-center gap-3">
              <div className="p-2 bg-gray-100 rounded-lg">
                <Globe className="w-4 h-4 text-gray-600" />
              </div>
              <div>
                <p className="text-xs text-gray-500">
                  Receive HTTP callbacks when events occur
                </p>
              </div>
            </div>
            <Button
              variant="outline"
              size="sm"
              onClick={openCreateWebhookDialog}
              aria-label="Add webhook"
            >
              <Plus className="w-3.5 h-3.5" />
              Add Webhook
            </Button>
          </div>

          {/* Webhook List */}
          {webhooks && webhooks.length > 0 ? (
            <div className="space-y-2">
              {webhooks.map((webhook) => (
                <div
                  key={webhook.id}
                  className="border border-gray-200 rounded-lg p-3"
                >
                  <div className="flex items-start justify-between gap-3">
                    <div className="min-w-0 flex-1">
                      <div className="flex items-center gap-2">
                        <code className="text-sm text-gray-900 truncate block">
                          {webhook.url}
                        </code>
                        {webhook.scope === "platform" && (
                          <Badge
                            variant="outline"
                            className="text-[10px] px-1.5 py-0 border-blue-200 text-blue-700 bg-blue-50"
                          >
                            Platform
                          </Badge>
                        )}
                        {!webhook.enabled && (
                          <Badge variant="secondary">Disabled</Badge>
                        )}
                        {webhook.disabled_at && (
                          <Tooltip>
                            <TooltipTrigger asChild>
                              <span
                                className="inline-flex items-center gap-1"
                                aria-label="Auto-disabled due to repeated delivery failures"
                              >
                                <AlertTriangle className="w-3.5 h-3.5 text-amber-500" />
                              </span>
                            </TooltipTrigger>
                            <TooltipContent>
                              <p>
                                Auto-disabled due to repeated delivery
                                failures.
                              </p>
                            </TooltipContent>
                          </Tooltip>
                        )}
                      </div>
                      {webhook.description && (
                        <p className="text-xs text-gray-500 mt-0.5">
                          {webhook.description}
                        </p>
                      )}
                      <div className="flex items-center gap-1.5 mt-1.5">
                        {webhook.event_types &&
                        webhook.event_types.length > 0 ? (
                          webhook.event_types.map((type) => (
                            <Badge
                              key={type}
                              variant="outline"
                              className="text-[10px] px-1.5 py-0"
                            >
                              {type}
                            </Badge>
                          ))
                        ) : (
                          <span className="text-[10px] text-gray-400">
                            All events
                          </span>
                        )}
                      </div>
                    </div>
                    <div className="flex items-center gap-1 shrink-0">
                      <Switch
                        checked={webhook.enabled}
                        onCheckedChange={() =>
                          handleWebhookToggle(webhook)
                        }
                        aria-label={`Toggle webhook ${webhook.url}`}
                      />
                      <Tooltip>
                        <TooltipTrigger asChild>
                          <Button
                            variant="ghost"
                            size="icon"
                            className="h-7 w-7"
                            onClick={() => openEditWebhookDialog(webhook)}
                            aria-label={`Edit webhook ${webhook.url}`}
                          >
                            <Pencil className="w-3.5 h-3.5" />
                          </Button>
                        </TooltipTrigger>
                        <TooltipContent>Edit webhook</TooltipContent>
                      </Tooltip>
                      <Tooltip>
                        <TooltipTrigger asChild>
                          <Button
                            variant="ghost"
                            size="icon"
                            className="h-7 w-7 text-red-500 hover:text-red-700 hover:bg-red-50"
                            onClick={() =>
                              setDeletingWebhookId(webhook.id)
                            }
                            aria-label={`Delete webhook ${webhook.url}`}
                          >
                            <Trash2 className="w-3.5 h-3.5" />
                          </Button>
                        </TooltipTrigger>
                        <TooltipContent>Delete webhook</TooltipContent>
                      </Tooltip>
                    </div>
                  </div>
                </div>
              ))}
            </div>
          ) : (
            <div className="text-sm text-gray-500 py-4 text-center border border-dashed border-gray-200 rounded-lg">
              No webhooks configured. Add one to receive HTTP
              notifications.
            </div>
          )}
        </div>
      </div>

      {/* Webhook Create/Edit Dialog */}
      <Dialog
        open={webhookDialogOpen}
        onOpenChange={(open) => {
          if (!open) {
            setWebhookDialogOpen(false);
            setWebhookSecret(null);
          }
        }}
      >
        <DialogContent>
          {webhookSecret ? (
            <>
              <DialogHeader>
                <DialogTitle>
                  {editingWebhook ? "Secret Rotated" : "Webhook Created"}
                </DialogTitle>
                <DialogDescription>
                  Copy this secret now. It will not be shown again.
                </DialogDescription>
              </DialogHeader>
              <div className="space-y-3">
                <div className="flex items-center gap-2 p-3 bg-gray-50 rounded-lg border border-gray-200">
                  <code className="text-sm flex-1 break-all font-mono">
                    {webhookSecret}
                  </code>
                  <Button
                    variant="ghost"
                    size="icon"
                    className="shrink-0 h-8 w-8"
                    onClick={() => handleCopySecret(webhookSecret)}
                    aria-label="Copy secret"
                  >
                    {copiedSecret ? (
                      <Check className="w-4 h-4 text-green-600" />
                    ) : (
                      <Copy className="w-4 h-4" />
                    )}
                  </Button>
                </div>
                <p className="text-xs text-amber-600 flex items-center gap-1">
                  <AlertTriangle className="w-3 h-3 shrink-0" />
                  Use this secret to verify webhook signatures. Store it
                  securely.
                </p>
              </div>
              <DialogFooter>
                <Button onClick={() => setWebhookDialogOpen(false)}>
                  Done
                </Button>
              </DialogFooter>
            </>
          ) : (
            <>
              <DialogHeader>
                <DialogTitle>
                  {editingWebhook ? "Edit Webhook" : "Add Webhook"}
                </DialogTitle>
                <DialogDescription>
                  {editingWebhook
                    ? "Update your webhook configuration."
                    : "Configure a URL to receive HTTP POST notifications."}
                </DialogDescription>
              </DialogHeader>
              <div className="space-y-4">
                {webhookError && (
                  <div className="text-sm text-red-600 bg-red-50 border border-red-200 p-2 rounded">
                    {webhookError}
                  </div>
                )}
                <div>
                  <Label htmlFor="webhookUrl" className="text-sm font-medium">
                    Endpoint URL
                  </Label>
                  <p className="text-xs text-gray-500 mt-0.5 mb-1">
                    We'll send a POST request with a JSON body to this URL when
                    events occur.
                  </p>
                  <Input
                    id="webhookUrl"
                    type="url"
                    value={webhookUrl}
                    onChange={(e) => setWebhookUrl(e.target.value)}
                    placeholder="https://example.com/webhooks/doubleword"
                  />
                </div>
                <div>
                  <Label
                    htmlFor="webhookDescription"
                    className="text-sm font-medium"
                  >
                    Description (optional)
                  </Label>
                  <Input
                    id="webhookDescription"
                    type="text"
                    value={webhookDescription}
                    onChange={(e) => setWebhookDescription(e.target.value)}
                    placeholder="My notification webhook"
                    className="mt-1"
                  />
                </div>
                {showPlatformScope && !editingWebhook && (
                  <div>
                    <Label className="text-sm font-medium">Scope</Label>
                    <p className="text-xs text-gray-500 mt-0.5 mb-2">
                      Personal webhooks receive events for your own activity.
                      Platform webhooks receive events across all users.
                    </p>
                    <Tabs
                      value={webhookScope}
                      onValueChange={(v) =>
                        handleScopeChange(v as WebhookScope)
                      }
                    >
                      <TabsList className="w-full">
                        <TabsTrigger value="own" className="flex-1">
                          Personal
                        </TabsTrigger>
                        <TabsTrigger value="platform" className="flex-1">
                          Platform
                        </TabsTrigger>
                      </TabsList>
                    </Tabs>
                  </div>
                )}
                {editingWebhook && editingWebhook.scope === "platform" && (
                  <div className="flex items-center gap-2">
                    <Label className="text-sm font-medium">Scope</Label>
                    <Badge
                      variant="outline"
                      className="border-blue-200 text-blue-700 bg-blue-50"
                    >
                      Platform
                    </Badge>
                  </div>
                )}
                <div>
                  <Label className="text-sm font-medium">Event Types</Label>
                  <p className="text-xs text-gray-500 mt-0.5 mb-2">
                    Select at least one event to listen for.
                  </p>
                  <div className="space-y-2">
                    {eventTypeOptionsForScope(webhookScope).map((option) => (
                      <label
                        key={option.value}
                        className="flex items-center gap-2 cursor-pointer"
                      >
                        <Checkbox
                          checked={webhookEventTypes.includes(option.value)}
                          onCheckedChange={() =>
                            handleWebhookEventTypeToggle(option.value)
                          }
                        />
                        <span className="text-sm text-gray-700">
                          {option.label}
                        </span>
                        <HoverCard openDelay={200} closeDelay={100}>
                          <HoverCardTrigger asChild>
                            <code className="text-xs text-gray-400 ml-auto cursor-help border-b border-dashed border-gray-300">
                              {option.value}
                            </code>
                          </HoverCardTrigger>
                          <HoverCardContent
                            side="left"
                            align="start"
                            className="w-[22rem]"
                          >
                            <p className="text-xs font-medium text-gray-700 mb-1">
                              Example payload
                            </p>
                            <pre className="text-[11px] text-gray-500 bg-gray-50 rounded p-2 overflow-x-auto whitespace-pre">
                              {option.example}
                            </pre>
                          </HoverCardContent>
                        </HoverCard>
                      </label>
                    ))}
                  </div>
                </div>
                {editingWebhook && (
                  <div className="pt-2 border-t border-gray-200">
                    <Button
                      variant="outline"
                      size="sm"
                      onClick={() => handleRotateSecret(editingWebhook.id)}
                      disabled={rotateSecretMutation.isPending}
                    >
                      {rotateSecretMutation.isPending ? (
                        <Loader2 className="w-3.5 h-3.5 animate-spin" />
                      ) : (
                        <RotateCw className="w-3.5 h-3.5" />
                      )}
                      Rotate Secret
                    </Button>
                    <p className="text-xs text-gray-500 mt-1">
                      Generate a new signing secret. The old secret will stop
                      working immediately.
                    </p>
                  </div>
                )}
              </div>
              <DialogFooter>
                <Button
                  variant="outline"
                  onClick={() => setWebhookDialogOpen(false)}
                >
                  Cancel
                </Button>
                <Button
                  onClick={handleWebhookSave}
                  disabled={
                    createWebhookMutation.isPending ||
                    updateWebhookMutation.isPending ||
                    webhookEventTypes.length === 0
                  }
                >
                  {createWebhookMutation.isPending ||
                  updateWebhookMutation.isPending ? (
                    <Loader2 className="w-4 h-4 animate-spin" />
                  ) : null}
                  {editingWebhook ? "Save Changes" : "Create Webhook"}
                </Button>
              </DialogFooter>
            </>
          )}
        </DialogContent>
      </Dialog>

      {/* Delete Confirmation Dialog */}
      <Dialog
        open={!!deletingWebhookId}
        onOpenChange={(open) => !open && setDeletingWebhookId(null)}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Delete Webhook</DialogTitle>
            <DialogDescription>
              Are you sure you want to delete this webhook? This action cannot
              be undone.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => setDeletingWebhookId(null)}
            >
              Cancel
            </Button>
            <Button
              variant="destructive"
              onClick={() =>
                deletingWebhookId && handleWebhookDelete(deletingWebhookId)
              }
              disabled={deleteWebhookMutation.isPending}
            >
              {deleteWebhookMutation.isPending ? (
                <Loader2 className="w-4 h-4 animate-spin" />
              ) : null}
              Delete
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
};
