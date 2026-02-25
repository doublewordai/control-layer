import React, { useState, useEffect, useMemo, useCallback } from "react";
import { useParams, useNavigate, useSearchParams } from "react-router-dom";
import {
  ArrowLeft,
  Code,
  Users,
  BarChart3,
  Play,
  Activity,
  X,
  Info,
  Edit,
  Check,
  Copy,
  GitMerge,
} from "lucide-react";
import {
  useModel,
  useEndpoint,
  useUpdateModel,
  useProbes,
  useModelComponents,
  useDaemons,
  useConfig,
} from "../../../../api/control-layer";
import type {
  ApiKeyPurpose,
  TrafficRoutingRule,
} from "../../../../api/control-layer";
import { useAuthorization } from "../../../../utils";
import {
  ApiExamples,
  AccessManagementModal,
  UpdateModelPricingModal,
} from "../../../modals";
import UserUsageTable from "./UserUsageTable";
import ModelProbes from "./ModelProbes";
import ProvidersTab from "./ProvidersTab";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "../../../ui/card";
import { Badge } from "../../../ui/badge";
import { Button } from "../../../ui/button";
import { Input } from "../../../ui/input";
import { Textarea } from "../../../ui/textarea";
import { InfoTip } from "../../../ui/info-tip";
import { Checkbox } from "../../../ui/checkbox";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "../../../ui/tabs";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../../ui/select";
import { Form, FormControl, FormField, FormItem } from "../../../ui/form";
import { useForm } from "react-hook-form";
import { zodResolver } from "@hookform/resolvers/zod";
import * as z from "zod";
import { Sparkline } from "../../../ui/sparkline";
import { Markdown } from "../../../ui/markdown";
import { ModelCombobox } from "../../../ui/model-combobox";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "../../../ui/popover";

// Form schema for alias editing
const aliasFormSchema = z.object({
  alias: z
    .string()
    .min(1, "Alias is required")
    .max(100, "Alias must be 100 characters or less"),
});

const TRAFFIC_PURPOSE_OPTIONS: ApiKeyPurpose[] = [
  "realtime",
  "batch",
  "playground",
];

const ModelInfo: React.FC = () => {
  const { modelId } = useParams<{ modelId: string }>();
  const navigate = useNavigate();
  const [searchParams] = useSearchParams();
  const { hasPermission } = useAuthorization();
  const canManageGroups = hasPermission("manage-groups");
  const canViewAnalytics = hasPermission("analytics");
  const canViewEndpoints = hasPermission("endpoints");

  const fromUrl = searchParams.get("from");

  // Get tab from URL or default to "overview"
  const tabFromUrl = searchParams.get("tab");
  const [activeTab, setActiveTab] = useState<string>(() => {
    // Only allow usage tab if user has permission
    if (tabFromUrl === "usage" && canManageGroups) return "usage";
    if (tabFromUrl === "probes" && canManageGroups) return "probes";
    if (tabFromUrl === "providers" && canManageGroups) return "providers";
    return "overview";
  });

  // Update activeTab when URL changes
  useEffect(() => {
    const tabFromUrl = searchParams.get("tab");
    if (
      tabFromUrl === "overview" ||
      (tabFromUrl === "usage" && canManageGroups) ||
      (tabFromUrl === "probes" && canManageGroups) ||
      (tabFromUrl === "providers" && canManageGroups)
    ) {
      setActiveTab(tabFromUrl);
    }
  }, [searchParams, canManageGroups]);

  // Handle tab change
  const handleTabChange = (value: string) => {
    setActiveTab(value);
    const newParams = new URLSearchParams(searchParams);
    newParams.set("tab", value);
    navigate(`/models/${modelId}?${newParams.toString()}`, { replace: true });
  };

  // Settings form state
  const [updateData, setUpdateData] = useState({
    alias: "",
    description: "",
    model_type: "" as "CHAT" | "EMBEDDINGS" | "",
    capabilities: [] as string[],
    sanitize_responses: false,
    trusted: false,
    open_responses_adapter: true,
    requests_per_second: null as number | null,
    burst_size: null as number | null,
    capacity: null as number | null,
    batch_capacity: null as number | null,
    throughput: null as number | null,
    allowed_batch_completion_windows: null as string[] | null,
    traffic_routing_rules: [] as TrafficRoutingRule[],
  });
  const [settingsError, setSettingsError] = useState<string | null>(null);
  const [showApiExamples, setShowApiExamples] = useState(false);
  const [isEditingAlias, setIsEditingAlias] = useState(false);
  const [aliasTruncated, setAliasTruncated] = useState(false);
  const [aliasPopoverOpen, setAliasPopoverOpen] = useState(false);
  const aliasRef = useCallback((el: HTMLHeadingElement | null) => {
    if (el) setAliasTruncated(el.scrollWidth > el.clientWidth);
  }, []);
  const [showAccessModal, setShowAccessModal] = useState(false);
  const [isEditingModelDetails, setIsEditingModelDetails] = useState(false);
  const [showPricingModal, setShowPricingModal] = useState(false);
  const [aliasCopied, setAliasCopied] = useState(false);

  // Alias form
  const aliasForm = useForm<z.infer<typeof aliasFormSchema>>({
    resolver: zodResolver(aliasFormSchema),
    defaultValues: {
      alias: "",
    },
  });

  const updateModelMutation = useUpdateModel();
  const { data: config } = useConfig();
  const availableCompletionWindows = useMemo(
    () => config?.batches?.allowed_completion_windows ?? ["24h"],
    [config?.batches?.allowed_completion_windows],
  );

  // Get config to check for strict mode
  const strictModeEnabled = config?.onwards?.strict_mode ?? false;

  // Build include parameter based on permissions
  const includeParam = useMemo(() => {
    const parts: string[] = ["status", "pricing"];
    if (canManageGroups) parts.push("groups");
    if (canViewAnalytics) parts.push("metrics");
    return parts.join(",");
  }, [canManageGroups, canViewAnalytics]);

  const {
    data: model,
    isLoading: modelLoading,
    error: modelError,
  } = useModel(modelId!, { include: includeParam });

  const {
    data: endpoint,
    isLoading: endpointLoading,
    error: endpointError,
  } = useEndpoint(model?.hosted_on || "", {
    enabled: !!model?.hosted_on && canViewEndpoints,
  });

  // Fetch probes to show status indicator
  const { data: probes } = useProbes();
  const modelProbe = probes?.find((p) => p.deployment_id === model?.id);

  // Fetch components (hosted models) for virtual models
  const { data: components } = useModelComponents(modelId!, {
    enabled: !!model?.is_composite,
  });

  // Fetch running daemon count for batch capacity context (requires System::ReadAll)
  const { data: daemonsData } = useDaemons(
    { status: "running" },
    { enabled: canManageGroups },
  );
  const runningDaemonCount =
    daemonsData?.daemons.filter((d) => {
      if (d.status !== "running" || !d.last_heartbeat) return false;
      const timeSinceHeartbeat = Date.now() - d.last_heartbeat * 1000;
      return timeSinceHeartbeat <= d.config.heartbeat_interval_ms * 2;
    }).length ?? 0;

  const loading = modelLoading || endpointLoading;
  const error = modelError
    ? (modelError as Error).message
    : endpointError
      ? (endpointError as Error).message
      : null;

  // Initialize form data when model is loaded
  useEffect(() => {
    if (model) {
      const effectiveType = model.model_type || "CHAT";

      setUpdateData({
        alias: model.alias,
        description: model.description || "",
        model_type: effectiveType as "CHAT" | "EMBEDDINGS",
        capabilities: model.capabilities || [],
        sanitize_responses: model.sanitize_responses ?? false,
        trusted: model.trusted ?? false,
        open_responses_adapter: model.open_responses_adapter ?? true,
        requests_per_second: model.requests_per_second || null,
        burst_size: model.burst_size || null,
        capacity: model.capacity || null,
        batch_capacity: model.batch_capacity || null,
        throughput: model.throughput || null,
        allowed_batch_completion_windows:
          model.allowed_batch_completion_windows ?? null,
        traffic_routing_rules: model.traffic_routing_rules || [],
      });
      aliasForm.reset({
        alias: model.alias,
      });
      setIsEditingAlias(false);
      setIsEditingModelDetails(false);
      setSettingsError(null);
    }
  }, [model, aliasForm]);

  // Settings form handlers
  const handleSave = async () => {
    if (!model) return;
    setSettingsError(null);

    const normalizedTrafficRules = updateData.traffic_routing_rules.map((rule) => {
      if (rule.action.type === "redirect") {
        return {
          ...rule,
          action: {
            type: "redirect" as const,
            target: rule.action.target.trim(),
          },
        };
      }
      return rule;
    });

    if (
      normalizedTrafficRules.some(
        (rule) =>
          rule.action.type === "redirect" && rule.action.target.length === 0,
      )
    ) {
      setSettingsError("Redirect rules must include a target model alias");
      return;
    }

    try {
      await updateModelMutation.mutateAsync({
        id: model.id,
        data: {
          alias: updateData.alias,
          description: updateData.description,
          model_type:
            updateData.model_type === ""
              ? null
              : (updateData.model_type as "CHAT" | "EMBEDDINGS"),
          capabilities: updateData.capabilities,
          sanitize_responses: updateData.sanitize_responses,
          trusted: updateData.trusted,
          open_responses_adapter: updateData.open_responses_adapter,
          // Always include rate limiting and capacity fields to handle clearing properly
          // Send null as the actual value when clearing (not undefined)
          requests_per_second: updateData.requests_per_second,
          burst_size: updateData.burst_size,
          capacity: updateData.capacity,
          batch_capacity: updateData.batch_capacity,
          throughput: updateData.throughput,
          allowed_batch_completion_windows:
            updateData.allowed_batch_completion_windows,
          traffic_routing_rules: normalizedTrafficRules,
        },
      });
      setIsEditingModelDetails(false);
    } catch (error) {
      setSettingsError(
        error instanceof Error
          ? error.message
          : "Failed to update model settings",
      );
    }
  };

  // Model details form handlers
  const handleModelDetailsCancel = () => {
    if (model) {
      const effectiveType = model.model_type || "CHAT";
      setUpdateData({
        alias: model.alias,
        description: model.description || "",
        model_type: effectiveType as "CHAT" | "EMBEDDINGS",
        capabilities: model.capabilities || [],
        sanitize_responses: model.sanitize_responses ?? false,
        trusted: model.trusted ?? false,
        open_responses_adapter: model.open_responses_adapter ?? true,
        requests_per_second: model.requests_per_second || null,
        burst_size: model.burst_size || null,
        capacity: model.capacity || null,
        batch_capacity: model.batch_capacity || null,
        throughput: model.throughput || null,
        allowed_batch_completion_windows:
          model.allowed_batch_completion_windows ?? null,
        traffic_routing_rules: model.traffic_routing_rules || [],
      });
    }
    setIsEditingModelDetails(false);
    setSettingsError(null);
  };

  // Alias inline editing handlers
  const onAliasSubmit = async (values: z.infer<typeof aliasFormSchema>) => {
    if (!model) return;
    setSettingsError(null);

    try {
      await updateModelMutation.mutateAsync({
        id: model.id,
        data: { alias: values.alias },
      });
      setIsEditingAlias(false);
    } catch (error) {
      setSettingsError(
        error instanceof Error ? error.message : "Failed to update alias",
      );
    }
  };

  const handleAliasCancel = () => {
    aliasForm.reset({
      alias: model?.alias || "",
    });
    setIsEditingAlias(false);
    setSettingsError(null);
  };

  if (loading) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center">
          <div
            className="animate-spin rounded-full h-12 w-12 border-b-2 border-doubleword-accent-blue mx-auto mb-4"
            aria-label="Loading"
          ></div>
          <p className="text-doubleword-neutral-600">
            Loading model details...
          </p>
        </div>
      </div>
    );
  }

  if (error) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center">
          <div className="text-red-500 mb-4">
            <ArrowLeft className="h-12 w-12 mx-auto" />
          </div>
          <p className="text-red-600 font-semibold">Error: {error}</p>
          <Button
            variant="outline"
            onClick={() => navigate(fromUrl || "/models")}
            className="mt-4"
          >
            <ArrowLeft className="mr-2 h-4 w-4" />
            {fromUrl ? "Go Back" : "Back to Models"}
          </Button>
        </div>
      </div>
    );
  }

  if (!model) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center">
          <p className="text-gray-600 font-semibold">Model not found</p>
          <Button
            variant="outline"
            onClick={() => navigate(fromUrl || "/models")}
            className="mt-4"
          >
            <ArrowLeft className="mr-2 h-4 w-4" />
            {fromUrl ? "Go Back" : "Back to Models"}
          </Button>
        </div>
      </div>
    );
  }

  return (
    <div className="p-6">
      <Tabs
        value={activeTab}
        onValueChange={handleTabChange}
        className="space-y-4"
      >
        {/* Header */}
        <div className="mb-6">
          <div className="flex items-center gap-4 mb-4">
            <button
              onClick={() => navigate(fromUrl || "/models")}
              className="p-2 text-gray-500 hover:bg-gray-100 rounded-lg transition-colors"
              aria-label={fromUrl ? "Go back" : "Back to Models"}
              title={fromUrl ? "Go back" : "Back to Models"}
            >
              <ArrowLeft className="w-5 h-5" />
            </button>
            <div className="flex-1 min-w-0">
              <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-4">
                <div className="min-w-0">
                  {isEditingAlias ? (
                    <div className="space-y-2">
                      <Form {...aliasForm}>
                        <form
                          onSubmit={aliasForm.handleSubmit(onAliasSubmit)}
                          className="flex items-center gap-2"
                        >
                          <FormField
                            control={aliasForm.control}
                            name="alias"
                            render={({ field }) => (
                              <FormItem>
                                <FormControl>
                                  <Input
                                    className="text-3xl font-bold h-12 text-doubleword-neutral-900"
                                    placeholder="Model alias"
                                    {...field}
                                  />
                                </FormControl>
                              </FormItem>
                            )}
                          />
                          <Button
                            type="submit"
                            size="sm"
                            disabled={updateModelMutation.isPending}
                          >
                            {updateModelMutation.isPending ? (
                              <div className="w-4 h-4 border-2 border-white border-t-transparent rounded-full animate-spin" />
                            ) : (
                              <Check className="h-4 w-4" />
                            )}
                          </Button>
                          <Button
                            type="button"
                            variant="outline"
                            size="sm"
                            onClick={handleAliasCancel}
                            disabled={updateModelMutation.isPending}
                          >
                            <X className="h-4 w-4" />
                          </Button>
                        </form>
                      </Form>
                      {aliasForm.formState.errors.alias && (
                        <p className="text-sm text-red-600">
                          {aliasForm.formState.errors.alias.message}
                        </p>
                      )}
                      {settingsError && (
                        <p className="text-sm text-red-600">{settingsError}</p>
                      )}
                    </div>
                  ) : (
                    <div className="flex items-center gap-3 min-w-0">
                      {aliasTruncated ? (
                        <Popover open={aliasPopoverOpen} onOpenChange={setAliasPopoverOpen}>
                          <PopoverTrigger asChild>
                            <h1
                              ref={aliasRef}
                              className="text-3xl font-bold text-doubleword-neutral-900 truncate min-w-0 cursor-default"
                              onMouseEnter={() => setAliasPopoverOpen(true)}
                              onMouseLeave={() => setAliasPopoverOpen(false)}
                            >
                              {model.alias}
                            </h1>
                          </PopoverTrigger>
                          <PopoverContent
                            side="bottom"
                            align="start"
                            className="w-auto max-w-sm px-3 py-2 pointer-events-none"
                            onOpenAutoFocus={(e) => e.preventDefault()}
                          >
                            <p className="text-sm break-all font-medium">{model.alias}</p>
                          </PopoverContent>
                        </Popover>
                      ) : (
                        <h1
                          ref={aliasRef}
                          className="text-3xl font-bold text-doubleword-neutral-900 truncate min-w-0"
                        >
                          {model.alias}
                        </h1>
                      )}
                      <button
                        type="button"
                        className="shrink-0 p-1 text-gray-400 hover:text-gray-600 transition-colors"
                        aria-label="Copy model alias"
                        onClick={() => {
                          navigator.clipboard.writeText(model.alias).then(() => {
                            setAliasCopied(true);
                            setTimeout(() => setAliasCopied(false), 1500);
                          });
                        }}
                      >
                        {aliasCopied ? (
                          <Check className="h-4 w-4 text-green-600" />
                        ) : (
                          <Copy className="h-4 w-4" />
                        )}
                      </button>
                      {/* Status indicator */}
                      {modelProbe && (
                        <div className="flex items-center gap-2">
                          <div
                            className={`h-3 w-3 rounded-full ${
                              model.status?.last_success === true
                                ? "bg-green-500"
                                : model.status?.last_success === false
                                  ? "bg-red-500"
                                  : "bg-gray-400"
                            }`}
                          />
                          {model.status?.uptime_percentage !== undefined &&
                            model.status?.uptime_percentage !== null && (
                              <span className="text-sm text-doubleword-neutral-600">
                                {model.status.uptime_percentage.toFixed(2)}%
                                uptime (24h)
                              </span>
                            )}
                        </div>
                      )}
                    </div>
                  )}
                  {canManageGroups && (
                    <p className="text-doubleword-neutral-600 mt-1">
                      {model.model_name}
                      {model.is_composite ? (
                        <>
                          {" "}
                          •{" "}
                          <span className="inline-flex items-center gap-1">
                            <GitMerge className="h-3 w-3" />
                            Virtual ({components?.length || 0} hosted models)
                          </span>
                        </>
                      ) : (
                        canViewEndpoints && (
                          <> • {endpoint?.name || "Unknown endpoint"}</>
                        )
                      )}
                    </p>
                  )}
                  {!canManageGroups &&
                    !model.is_composite &&
                    canViewEndpoints && (
                      <p className="text-doubleword-neutral-600 mt-1">
                        {endpoint?.name || "Unknown endpoint"}
                      </p>
                    )}
                </div>
                {canManageGroups && (
                  <div className="flex items-center justify-center sm:justify-start gap-3">
                    <TabsList className="w-full sm:w-auto">
                      <TabsTrigger
                        value="overview"
                        className="flex items-center gap-2"
                      >
                        <Info className="h-4 w-4" />
                        Overview
                      </TabsTrigger>
                      <TabsTrigger
                        value="usage"
                        className="flex items-center gap-2"
                      >
                        <Users className="h-4 w-4" />
                        Usage
                      </TabsTrigger>
                      <TabsTrigger
                        value="probes"
                        className="flex items-center gap-2"
                      >
                        <Activity className="h-4 w-4" />
                        Uptime
                      </TabsTrigger>
                      {model.is_composite && (
                        <TabsTrigger
                          value="providers"
                          className="flex items-center gap-2"
                        >
                          <GitMerge className="h-4 w-4" />
                          Hosted Models
                        </TabsTrigger>
                      )}
                    </TabsList>
                  </div>
                )}
              </div>
            </div>
          </div>
        </div>
        <TabsContent value="overview">
          <div className="grid grid-cols-1 lg:grid-cols-3 gap-6">
            {/* Main Content */}
            <div className="lg:col-span-2 space-y-6">
              {/* Model Details */}
              <Card className="p-0 gap-0 rounded-lg">
                <CardHeader className="px-6 pt-5 pb-4">
                  <div className="flex items-center justify-between">
                    <CardTitle>Model Details</CardTitle>
                    {canManageGroups && !isEditingModelDetails && (
                      <Button
                        variant="outline"
                        size="sm"
                        onClick={() => setIsEditingModelDetails(true)}
                        className="h-8 w-8 p-0"
                      >
                        <Edit className="h-4 w-4" />
                      </Button>
                    )}
                  </div>
                </CardHeader>
                <CardContent className="px-6 pb-6 pt-0">
                  {isEditingModelDetails ? (
                    <div className="space-y-4">
                      <div className="grid grid-cols-1 md:grid-cols-3 gap-6">
                        <div>
                          <label className="text-sm text-gray-600 mb-2 block">
                            Full Name
                          </label>
                          <p className="font-medium p-2 bg-gray-50 rounded text-gray-500">
                            {model.model_name}
                          </p>
                          <p className="text-xs text-gray-400 mt-1">
                            Read-only
                          </p>
                        </div>
                        <div>
                          <label className="text-sm text-gray-600 mb-2 block">
                            Alias
                          </label>
                          <Input
                            value={updateData.alias}
                            onChange={(e) =>
                              setUpdateData((prev) => ({
                                ...prev,
                                alias: e.target.value,
                              }))
                            }
                            className="font-medium"
                            placeholder="Model alias"
                          />
                        </div>
                        <div>
                          <label className="text-sm text-gray-600 mb-2 block">
                            Type
                          </label>
                          <Select
                            value={updateData.model_type}
                            onValueChange={(value) =>
                              setUpdateData((prev) => ({
                                ...prev,
                                model_type: value as "CHAT" | "EMBEDDINGS",
                              }))
                            }
                          >
                            <SelectTrigger>
                              <SelectValue />
                            </SelectTrigger>
                            <SelectContent>
                              <SelectItem value="CHAT">Chat</SelectItem>
                              <SelectItem value="EMBEDDINGS">
                                Embeddings
                              </SelectItem>
                            </SelectContent>
                          </Select>
                        </div>
                      </div>
                      <div>
                        <div className="flex items-center gap-1 mb-2">
                          <label className="text-sm text-gray-600">
                            Description
                          </label>
                          <InfoTip>
                            <p className="text-sm text-muted-foreground">
                              User provided description for the model.
                              Displayed to all users when viewing the model on
                              the overview page.
                            </p>
                          </InfoTip>
                        </div>
                        <Textarea
                          value={updateData.description}
                          onChange={(e) =>
                            setUpdateData((prev) => ({
                              ...prev,
                              description: e.target.value,
                            }))
                          }
                          placeholder="Enter model description..."
                          rows={3}
                          className="resize-y min-h-[72px]"
                        />
                      </div>

                      {/* Capabilities Section */}
                      {updateData.model_type === "CHAT" && (
                        <div className="border-t pt-4">
                          <div className="flex items-center gap-1 mb-3">
                            <label className="text-sm text-gray-600 font-medium">
                              Capabilities
                            </label>
                          </div>
                          <div className="space-y-2">
                            <div className="flex items-center space-x-2">
                              <input
                                type="checkbox"
                                id="vision-capability"
                                checked={
                                  updateData.capabilities?.includes("vision") ??
                                  false
                                }
                                onChange={(e) => {
                                  const newCapabilities = e.target.checked
                                    ? [
                                        ...(updateData.capabilities || []),
                                        "vision",
                                      ]
                                    : (updateData.capabilities || []).filter(
                                        (c) => c !== "vision",
                                      );
                                  setUpdateData((prev) => ({
                                    ...prev,
                                    capabilities: newCapabilities,
                                  }));
                                }}
                                className="h-4 w-4 rounded border-gray-300 text-blue-600 focus:ring-blue-500"
                              />
                              <label
                                htmlFor="vision-capability"
                                className="text-sm font-medium leading-none peer-disabled:cursor-not-allowed peer-disabled:opacity-70 flex items-center gap-1"
                              >
                                Vision
                                <InfoTip>
                                  <p className="text-sm text-muted-foreground">
                                    Enables image upload in the playground.
                                  </p>
                                </InfoTip>
                              </label>
                            </div>
                          </div>
                        </div>
                      )}

                      {/* Response Configuration Section */}
                      <div className="border-t pt-4">
                        <div className="flex items-center gap-1 mb-3">
                          <label className="text-sm text-gray-600 font-medium">
                            Response Configuration
                          </label>
                          <InfoTip>
                            <p className="text-sm text-muted-foreground">
                              Configure how responses from this model are
                              processed before being returned to clients.
                            </p>
                          </InfoTip>
                        </div>

                        <div className="space-y-2">
                          {/* Show sanitize_responses when strict mode is OFF */}
                          {!strictModeEnabled && (
                            <div className="flex items-center space-x-2">
                              <input
                                type="checkbox"
                                id="sanitize-responses"
                                checked={updateData.sanitize_responses ?? false}
                                onChange={(e) => {
                                  setUpdateData((prev) => ({
                                    ...prev,
                                    sanitize_responses: e.target.checked,
                                  }));
                                }}
                                className="h-4 w-4 rounded border-gray-300 text-blue-600 focus:ring-blue-500"
                              />
                              <label
                                htmlFor="sanitize-responses"
                                className="text-sm font-medium leading-none peer-disabled:cursor-not-allowed peer-disabled:opacity-70 flex items-center gap-1"
                              >
                                Sanitize Responses
                                <InfoTip>
                                  <p className="text-sm text-muted-foreground">
                                    Filter out third-party provider fields from
                                    OpenAI compatible responses to ensure clean,
                                    standardized API responses.
                                  </p>
                                </InfoTip>
                              </label>
                            </div>
                          )}

                          {/* Show trusted and responses adapter for standard models when strict mode is ON */}
                          {!model.is_composite && strictModeEnabled && (
                            <>
                              <div className="flex items-center space-x-2">
                                <input
                                  type="checkbox"
                                  id="trusted-provider"
                                  checked={updateData.trusted ?? false}
                                  onChange={(e) => {
                                    setUpdateData((prev) => ({
                                      ...prev,
                                      trusted: e.target.checked,
                                    }));
                                  }}
                                  className="h-4 w-4 rounded border-gray-300 text-blue-600 focus:ring-blue-500"
                                />
                                <label
                                  htmlFor="trusted-provider"
                                  className="text-sm font-medium leading-none peer-disabled:cursor-not-allowed peer-disabled:opacity-70 flex items-center gap-1"
                                >
                                  Trusted Provider
                                  <InfoTip>
                                    <p className="text-sm text-muted-foreground">
                                      Mark this provider as trusted in strict
                                      mode. Trusted providers bypass error
                                      sanitization, allowing full error details
                                      to be returned. Non-trusted providers have
                                      sensitive error information removed.
                                    </p>
                                  </InfoTip>
                                </label>
                              </div>
                              <div className="flex items-center space-x-2">
                                <input
                                  type="checkbox"
                                  id="open-responses-adapter"
                                  checked={
                                    updateData.open_responses_adapter ?? true
                                  }
                                  onChange={(e) => {
                                    setUpdateData((prev) => ({
                                      ...prev,
                                      open_responses_adapter: e.target.checked,
                                    }));
                                  }}
                                  className="h-4 w-4 rounded border-gray-300 text-blue-600 focus:ring-blue-500"
                                />
                                <label
                                  htmlFor="open-responses-adapter"
                                  className="text-sm font-medium leading-none peer-disabled:cursor-not-allowed peer-disabled:opacity-70 flex items-center gap-1"
                                >
                                  Responses API Adapter
                                  <InfoTip>
                                    <p className="text-sm text-muted-foreground">
                                      Enable the adapter that converts OpenAI
                                      Responses API requests (/v1/responses) to
                                      Chat Completions (/v1/chat/completions)
                                      for providers that don&apos;t natively
                                      support the Responses API.
                                    </p>
                                  </InfoTip>
                                </label>
                              </div>
                            </>
                          )}
                        </div>
                      </div>

                      {/* Batch Completion Windows Section */}
                      <div className="border-t pt-4">
                        <div className="flex items-center gap-1 mb-3">
                          <label className="text-sm text-gray-600 font-medium">
                            Batch Completion Windows
                          </label>
                          <InfoTip>
                            <p className="text-sm text-muted-foreground">
                              All globally configured windows are allowed by
                              default. Uncheck to restrict specific windows for
                              this model.
                            </p>
                          </InfoTip>
                        </div>
                        <div className="space-y-2">
                          {availableCompletionWindows.map((window) => {
                            const current =
                              updateData.allowed_batch_completion_windows;
                            const isChecked =
                              current === null || current.includes(window);
                            return (
                              <label
                                key={window}
                                className="flex items-center gap-2 text-sm"
                              >
                                <Checkbox
                                  checked={isChecked}
                                  onCheckedChange={(checked) =>
                                    setUpdateData((prev) => {
                                      const current =
                                        prev.allowed_batch_completion_windows;
                                      let next: string[];
                                      if (checked) {
                                        if (current === null) return prev;
                                        // Re-checking: add back (guard against duplicates)
                                        next = current.includes(window)
                                          ? current
                                          : [...current, window];
                                      } else if (current === null) {
                                        // First uncheck from defaults: populate with all except this one
                                        next =
                                          availableCompletionWindows.filter(
                                            (w) => w !== window,
                                          );
                                      } else {
                                        // Already restricted: remove this one
                                        next = current.filter(
                                          (w) => w !== window,
                                        );
                                      }
                                      // If all are checked again, clear back to defaults (null)
                                      const allChecked =
                                        availableCompletionWindows.every((w) =>
                                          next.includes(w),
                                        );
                                      return {
                                        ...prev,
                                        allowed_batch_completion_windows:
                                          allChecked ? null : next,
                                      };
                                    })
                                  }
                                />
                                <span className="font-mono text-xs">
                                  {window}
                                </span>
                              </label>
                            );
                          })}
                        </div>
                        {updateData.allowed_batch_completion_windows !==
                          null &&
                          updateData.allowed_batch_completion_windows.length >=
                            0 && (
                          <Button
                            type="button"
                            variant="outline"
                            size="sm"
                            className="mt-3 text-xs"
                            onClick={() =>
                              setUpdateData((prev) => ({
                                ...prev,
                                allowed_batch_completion_windows: null,
                              }))
                            }
                          >
                            Allow All
                          </Button>
                        )}
                      </div>

                      {/* Traffic Routing Rules Section */}
                      <div className="border-t pt-4">
                        <div className="flex items-center gap-1 mb-3">
                          <label className="text-sm text-gray-600 font-medium">
                            Traffic Routing Rules
                          </label>
                          <InfoTip>
                            <p className="text-sm text-muted-foreground">
                              Match API key purpose and either deny traffic or
                              transparently redirect to another model alias.
                            </p>
                          </InfoTip>
                        </div>
                        <div className="space-y-2">
                          {updateData.traffic_routing_rules.length > 0 && (
                            <div className="hidden md:grid md:grid-cols-12 gap-2 text-xs text-muted-foreground">
                              <span className="md:col-span-3">Purpose</span>
                              <span className="md:col-span-3">Action</span>
                              <span className="md:col-span-5">Target</span>
                            </div>
                          )}
                          {updateData.traffic_routing_rules.map((rule, index) => (
                            <div
                              key={index}
                              className="grid grid-cols-1 md:grid-cols-12 gap-2 items-start"
                            >
                              <div className="md:col-span-3">
                                <Select
                                  value={rule.api_key_purpose}
                                  onValueChange={(value) =>
                                    setUpdateData((prev) => ({
                                      ...prev,
                                      traffic_routing_rules:
                                        prev.traffic_routing_rules.map((r, i) =>
                                          i === index
                                            ? {
                                                ...r,
                                                api_key_purpose:
                                                  value as ApiKeyPurpose,
                                              }
                                            : r,
                                        ),
                                    }))
                                  }
                                >
                                  <SelectTrigger>
                                    <SelectValue />
                                  </SelectTrigger>
                                  <SelectContent>
                                    {TRAFFIC_PURPOSE_OPTIONS.filter(
                                      (purpose) =>
                                        purpose === rule.api_key_purpose ||
                                        !updateData.traffic_routing_rules.some(
                                          (r) =>
                                            r.api_key_purpose === purpose,
                                        ),
                                    ).map((purpose) => (
                                      <SelectItem key={purpose} value={purpose}>
                                        {purpose}
                                      </SelectItem>
                                    ))}
                                  </SelectContent>
                                </Select>
                              </div>
                              <div className="md:col-span-3">
                                <Select
                                  value={rule.action.type}
                                  onValueChange={(value) =>
                                    setUpdateData((prev) => ({
                                      ...prev,
                                      traffic_routing_rules:
                                        prev.traffic_routing_rules.map((r, i) => {
                                          if (i !== index) return r;
                                          if (value === "redirect") {
                                            return {
                                              ...r,
                                              action: {
                                                type: "redirect",
                                                target:
                                                  r.action.type === "redirect"
                                                    ? r.action.target
                                                    : "",
                                              },
                                            };
                                          }
                                          return {
                                            ...r,
                                            action: { type: "deny" },
                                          };
                                        }),
                                    }))
                                  }
                                >
                                  <SelectTrigger>
                                    <SelectValue />
                                  </SelectTrigger>
                                  <SelectContent>
                                    <SelectItem value="deny">Deny</SelectItem>
                                    <SelectItem value="redirect">
                                      Redirect
                                    </SelectItem>
                                  </SelectContent>
                                </Select>
                              </div>
                              <div className="md:col-span-5">
                                {rule.action.type === "redirect" ? (
                                  <ModelCombobox
                                    value={
                                      rule.action.target
                                        ? ({ id: "", alias: rule.action.target } as any)
                                        : null
                                    }
                                    onValueChange={(model) =>
                                      setUpdateData((prev) => ({
                                        ...prev,
                                        traffic_routing_rules:
                                          prev.traffic_routing_rules.map((r, i) =>
                                            i === index
                                              ? {
                                                  ...r,
                                                  action: {
                                                    type: "redirect",
                                                    target: model.alias,
                                                  },
                                                }
                                              : r,
                                          ),
                                      }))
                                    }
                                    placeholder="Select target model..."
                                    className="w-full font-mono text-xs"
                                    filterFn={(model) =>
                                      model.id !== modelId
                                    }
                                  />
                                ) : (
                                  <p className="text-xs text-muted-foreground h-10 flex items-center">
                                    Return 403 Forbidden
                                  </p>
                                )}
                              </div>
                              <div className="md:col-span-1 flex justify-end">
                                <Button
                                  type="button"
                                  variant="outline"
                                  size="sm"
                                  className="h-9 w-9 p-0"
                                  onClick={() =>
                                    setUpdateData((prev) => ({
                                      ...prev,
                                      traffic_routing_rules:
                                        prev.traffic_routing_rules.filter(
                                          (_, i) => i !== index,
                                        ),
                                    }))
                                  }
                                >
                                  <X className="h-4 w-4" />
                                </Button>
                              </div>
                            </div>
                          ))}
                        </div>
                        {(() => {
                          const usedPurposes = new Set(
                            updateData.traffic_routing_rules.map(
                              (r) => r.api_key_purpose,
                            ),
                          );
                          const nextPurpose =
                            TRAFFIC_PURPOSE_OPTIONS.find(
                              (p) => !usedPurposes.has(p),
                            );
                          return (
                            <Button
                              type="button"
                              variant="outline"
                              size="sm"
                              className="mt-3"
                              disabled={!nextPurpose}
                              onClick={() => {
                                if (!nextPurpose) return;
                                setUpdateData((prev) => ({
                                  ...prev,
                                  traffic_routing_rules: [
                                    ...prev.traffic_routing_rules,
                                    {
                                      api_key_purpose: nextPurpose,
                                      action: { type: "deny" },
                                    },
                                  ],
                                }));
                              }}
                            >
                              Add Rule
                            </Button>
                          );
                        })()}
                      </div>

                      {/* Rate Limiting Section */}
                      <div className="border-t pt-4">
                        <div className="flex items-center gap-1 mb-3">
                          <label className="text-sm text-gray-600 font-medium">
                            Global Rate Limiting
                          </label>
                          <InfoTip>
                            <p className="text-sm text-muted-foreground">
                              Set system-wide rate limits for this model.
                              These apply to all users and override individual
                              API key limits. Leave fields blank for no
                              limits/defaults.
                            </p>
                          </InfoTip>
                        </div>
                        <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
                          <div>
                            <label className="text-sm text-gray-600 mb-2 flex items-center gap-1">
                              Requests per Second
                              <InfoTip>
                                <p className="text-sm text-muted-foreground">
                                  Sustained request rate limit. Temporary
                                  bursts can exceed this up to the burst size.
                                  Exceeding this limit returns 429 errors.
                                </p>
                              </InfoTip>
                            </label>
                            <Input
                              type="number"
                              min="1"
                              max="10000"
                              step="1"
                              value={updateData.requests_per_second || ""}
                              onChange={(e) =>
                                setUpdateData((prev) => ({
                                  ...prev,
                                  requests_per_second:
                                    e.target.value === ""
                                      ? null
                                      : Number(e.target.value),
                                }))
                              }
                              placeholder={
                                updateData.requests_per_second !== null
                                  ? updateData.requests_per_second?.toString() ||
                                    "None"
                                  : "None"
                              }
                            />
                          </div>
                          <div>
                            <label className="text-sm text-gray-600 mb-2 flex items-center gap-1">
                              Burst Size
                              <InfoTip>
                                <p className="text-sm text-muted-foreground">
                                  Maximum number of requests allowed in a
                                  temporary burst above the sustained rate.
                                </p>
                              </InfoTip>
                            </label>
                            <Input
                              type="number"
                              min="1"
                              max="50000"
                              step="1"
                              value={updateData.burst_size || ""}
                              onChange={(e) =>
                                setUpdateData((prev) => ({
                                  ...prev,
                                  burst_size:
                                    e.target.value === ""
                                      ? null
                                      : Number(e.target.value),
                                }))
                              }
                              placeholder={
                                updateData.burst_size !== null
                                  ? updateData.burst_size?.toString() || "None"
                                  : "None"
                              }
                            />
                          </div>
                          <div>
                            <label className="text-sm text-gray-600 mb-2 flex items-center gap-1">
                              Maximum Concurrent Requests
                              <InfoTip>
                                <p className="text-sm text-muted-foreground">
                                  Maximum number of requests that can be
                                  processed concurrently. Exceeding this limit
                                  returns 429 errors.
                                </p>
                              </InfoTip>
                            </label>
                            <Input
                              type="number"
                              min="1"
                              max="10000"
                              step="1"
                              value={updateData.capacity || ""}
                              onChange={(e) =>
                                setUpdateData((prev) => ({
                                  ...prev,
                                  capacity:
                                    e.target.value === ""
                                      ? null
                                      : Number(e.target.value),
                                }))
                              }
                              placeholder={
                                updateData.capacity !== null
                                  ? updateData.capacity?.toString() || "None"
                                  : "None"
                              }
                            />
                          </div>
                          <div>
                            <label className="text-sm text-gray-600 mb-2 flex items-center gap-1">
                              Per-Daemon Batch Concurrency
                              <InfoTip>
                                <p className="text-sm text-muted-foreground">
                                  Maximum concurrent batch requests each
                                  daemon can send to this model. Total
                                  capacity scales with the number of running
                                  daemons.
                                </p>
                              </InfoTip>
                              {runningDaemonCount > 0 && (
                                <span className="text-xs text-gray-500">
                                  ({runningDaemonCount}{" "}
                                  {runningDaemonCount === 1
                                    ? "daemon"
                                    : "daemons"}{" "}
                                  running
                                  {updateData.batch_capacity
                                    ? ` · ${(updateData.batch_capacity * runningDaemonCount).toLocaleString()} total capacity`
                                    : ""}
                                  )
                                </span>
                              )}
                            </label>
                            <Input
                              type="number"
                              min="1"
                              max="10000"
                              step="1"
                              value={updateData.batch_capacity || ""}
                              onChange={(e) =>
                                setUpdateData((prev) => ({
                                  ...prev,
                                  batch_capacity:
                                    e.target.value === ""
                                      ? null
                                      : Number(e.target.value),
                                }))
                              }
                              placeholder={
                                updateData.batch_capacity !== null
                                  ? updateData.batch_capacity?.toString() ||
                                    "None"
                                  : "None"
                              }
                            />
                          </div>
                          <div>
                            <label className="text-sm text-gray-600 mb-2 flex items-center gap-1">
                              Throughput
                              <InfoTip>
                                <p className="text-sm text-muted-foreground">
                                  Model throughput in requests per second,
                                  used for batch SLA capacity calculations.
                                  Defaults to 100 req/s if not set.
                                </p>
                              </InfoTip>
                            </label>
                            <Input
                              type="number"
                              min="0.01"
                              max="1000"
                              step="any"
                              value={updateData.throughput || ""}
                              onChange={(e) =>
                                setUpdateData((prev) => ({
                                  ...prev,
                                  throughput:
                                    e.target.value === ""
                                      ? null
                                      : Number(e.target.value),
                                }))
                              }
                              placeholder={
                                updateData.throughput !== null
                                  ? updateData.throughput?.toString() || "None"
                                  : "None"
                              }
                            />
                          </div>
                        </div>
                        {(updateData.requests_per_second ||
                          updateData.burst_size ||
                          updateData.capacity ||
                          updateData.batch_capacity ||
                          updateData.throughput) && (
                          <div className="mt-3">
                            <Button
                              type="button"
                              variant="outline"
                              size="sm"
                              onClick={() =>
                                setUpdateData((prev) => ({
                                  ...prev,
                                  requests_per_second: null,
                                  burst_size: null,
                                  capacity: null,
                                  batch_capacity: null,
                                  throughput: null,
                                }))
                              }
                              className="text-xs"
                            >
                              Clear Rate Limits & Capacity
                            </Button>
                          </div>
                        )}
                        {updateData.burst_size &&
                          !updateData.requests_per_second && (
                            <div className="mt-2 p-2 bg-yellow-50 border border-yellow-200 rounded-md">
                              <p className="text-xs text-yellow-700">
                                ⚠️ Burst size will be ignored without requests
                                per second. Set requests per second to enable
                                rate limiting.
                              </p>
                            </div>
                          )}
                        {updateData.batch_capacity &&
                          updateData.capacity &&
                          runningDaemonCount > 0 &&
                          updateData.batch_capacity * runningDaemonCount >
                            updateData.capacity && (
                            <div className="mt-2 p-2 bg-yellow-50 border border-yellow-200 rounded-md">
                              <p className="text-xs text-yellow-700">
                                ⚠️ Total batch capacity (
                                {(
                                  updateData.batch_capacity * runningDaemonCount
                                ).toLocaleString()}{" "}
                                = {updateData.batch_capacity.toLocaleString()} ×{" "}
                                {runningDaemonCount}{" "}
                                {runningDaemonCount === 1
                                  ? "daemon"
                                  : "daemons"}
                                ) exceeds the maximum concurrent requests limit
                                ({updateData.capacity.toLocaleString()}). Batch
                                requests may be rate limited.
                              </p>
                            </div>
                          )}
                      </div>

                      <div className="flex items-center gap-3 pt-4 border-t justify-end">
                        <Button
                          onClick={handleSave}
                          disabled={updateModelMutation.isPending}
                          size="sm"
                        >
                          {updateModelMutation.isPending ? (
                            <>
                              <div className="w-4 h-4 border-2 border-white border-t-transparent rounded-full animate-spin mr-2" />
                              Saving...
                            </>
                          ) : (
                            <>
                              <Check className="mr-2 h-4 w-4" />
                              Save Changes
                            </>
                          )}
                        </Button>
                        <Button
                          variant="outline"
                          onClick={handleModelDetailsCancel}
                          disabled={updateModelMutation.isPending}
                          size="sm"
                        >
                          Cancel
                        </Button>
                        {settingsError && (
                          <p className="text-sm text-red-600 ml-3">
                            {settingsError}
                          </p>
                        )}
                      </div>
                    </div>
                  ) : (
                    <div className="space-y-6">
                      {canManageGroups && (
                      <div className="grid grid-cols-1 md:grid-cols-3 gap-6">
                          <div>
                            <div className="flex items-center gap-1 mb-1">
                              <p className="text-sm text-gray-600">Full Name</p>
                              <InfoTip>
                                <p className="text-sm text-muted-foreground">
                                  The name under which the model is available
                                  at the upstream endpoint.
                                </p>
                              </InfoTip>
                            </div>
                            <p className="font-medium">{model.model_name}</p>
                          </div>
                          <div>
                            <div className="flex items-center gap-1 mb-1">
                              <p className="text-sm text-gray-600">Alias</p>
                              <InfoTip>
                                <p className="text-sm text-muted-foreground">
                                  The name under which the model will be made
                                  available in the control layer API.
                                </p>
                              </InfoTip>
                            </div>
                            <p className="font-medium">{model.alias}</p>
                          </div>
                          <div>
                            <div className="flex items-center gap-1 mb-1">
                              <p className="text-sm text-gray-600">Type</p>
                              <InfoTip>
                                <p className="text-sm text-muted-foreground">
                                  The type of the model. Determines which
                                  playground is used.
                                </p>
                              </InfoTip>
                            </div>
                            <Badge variant="outline">
                              {model.model_type || "UNKNOWN"}
                            </Badge>
                          </div>
                      </div>
                      )}
                      <div>
                        <div className="flex items-center gap-1 mb-1">
                          <p className="text-sm text-gray-600">Description</p>
                          {canManageGroups && (
                            <InfoTip>
                              <p className="text-sm text-muted-foreground">
                                User provided description for the model.
                                Displayed to all users when viewing the model on
                                the overview page.
                              </p>
                            </InfoTip>
                          )}
                        </div>
                        {model.description ? (
                          <Markdown className="text-sm text-gray-700">
                            {model.description}
                          </Markdown>
                        ) : (
                          <p className="text-gray-700">
                            No description provided
                          </p>
                        )}
                      </div>

                      {/* Capabilities Section - only show for CHAT models */}
                      {(model.model_type === "CHAT" || !model.model_type) &&
                        canManageGroups && (
                          <div className="border-t pt-6">
                            <div className="flex items-center gap-1 mb-3">
                              <p className="text-sm text-gray-600 font-medium">
                                Capabilities
                              </p>
                            </div>
                            <div className="space-y-3">
                              <div className="flex items-center justify-between">
                                <div className="flex items-center space-x-2">
                                  <input
                                    type="checkbox"
                                    id="vision-capability-readonly"
                                    checked={
                                      model.capabilities?.includes("vision") ??
                                      false
                                    }
                                    onChange={async (e) => {
                                      const newCapabilities = e.target.checked
                                        ? [
                                            ...(model.capabilities || []),
                                            "vision",
                                          ]
                                        : (model.capabilities || []).filter(
                                            (c) => c !== "vision",
                                          );

                                      try {
                                        await updateModelMutation.mutateAsync({
                                          id: model.id,
                                          data: {
                                            capabilities: newCapabilities,
                                          },
                                        });
                                      } catch (error) {
                                        console.error(
                                          "Failed to update capabilities:",
                                          error,
                                        );
                                      }
                                    }}
                                    disabled={updateModelMutation.isPending}
                                    className="h-4 w-4 rounded border-gray-300 text-blue-600 focus:ring-blue-500 disabled:opacity-50"
                                  />
                                  <label
                                    htmlFor="vision-capability-readonly"
                                    className="text-sm font-medium leading-none peer-disabled:cursor-not-allowed peer-disabled:opacity-70 flex items-center gap-1"
                                  >
                                    Vision
                                    <InfoTip>
                                      <p className="text-sm text-muted-foreground">
                                        Enables image upload in the
                                        playground.
                                      </p>
                                    </InfoTip>
                                  </label>
                                </div>
                              </div>
                            </div>
                          </div>
                        )}

                      {/* Batch Configuration Display */}
                      {canManageGroups && (
                        <div className="border-t pt-6 space-y-4">
                          <div>
                            <div className="flex items-center gap-1 mb-1">
                              <p className="text-sm text-gray-600">
                                Batch Completion Windows
                              </p>
                              <InfoTip>
                                <p className="text-sm text-muted-foreground">
                                  Allowed batch completion windows for this model.
                                  Empty means global defaults from config.
                                </p>
                              </InfoTip>
                            </div>
                            {model.allowed_batch_completion_windows &&
                            model.allowed_batch_completion_windows.length > 0 ? (
                              <div className="flex flex-wrap gap-2">
                                {model.allowed_batch_completion_windows.map((window) => (
                                  <Badge key={window} variant="outline" className="font-mono">
                                    {window}
                                  </Badge>
                                ))}
                              </div>
                            ) : (
                              <p className="text-sm text-muted-foreground">
                                Global defaults ({availableCompletionWindows.join(", ")})
                              </p>
                            )}
                          </div>
                          <div>
                            <div className="flex items-center gap-1 mb-1">
                              <p className="text-sm text-gray-600">
                                Traffic Routing Rules
                              </p>
                              <InfoTip>
                                <p className="text-sm text-muted-foreground">
                                  Purpose-specific deny or redirect behavior.
                                </p>
                              </InfoTip>
                            </div>
                            {model.traffic_routing_rules &&
                            model.traffic_routing_rules.length > 0 ? (
                              <div className="space-y-2">
                                {model.traffic_routing_rules.map((rule, index) => (
                                  <div
                                    key={index}
                                    className="flex items-center justify-between rounded-md border bg-muted px-3 py-2"
                                  >
                                    <div className="flex items-center gap-2 text-sm">
                                      <Badge variant="outline">{rule.api_key_purpose}</Badge>
                                      <span className="text-muted-foreground">→</span>
                                      {rule.action.type === "deny" ? (
                                        <span className="font-medium">deny</span>
                                      ) : (
                                        <span className="font-medium">
                                          redirect to
                                          <span className="ml-1 font-mono text-xs">
                                            {rule.action.target}
                                          </span>
                                        </span>
                                      )}
                                    </div>
                                  </div>
                                ))}
                              </div>
                            ) : (
                              <p className="text-sm text-muted-foreground">
                                No routing rules configured.
                              </p>
                            )}
                          </div>
                        </div>
                      )}

                      {/* Pricing Display - visible to all users when billing is enabled */}
                      {
                        <div className="border-t pt-6">
                          <div className="flex items-center justify-between mb-3">
                            <div className="flex items-center gap-1">
                              <p className="text-sm text-gray-600">
                                Pricing Tariffs
                              </p>
                              {canManageGroups && (
                                <InfoTip>
                                  <p className="text-sm text-muted-foreground">
                                    Pricing tiers for different API key
                                    purposes. Set different rates for realtime,
                                    batch, and playground usage. Click "Manage
                                    Tariffs" to configure pricing.
                                  </p>
                                </InfoTip>
                              )}
                            </div>
                            {canManageGroups && (
                              <Button
                                variant="outline"
                                size="sm"
                                onClick={() => setShowPricingModal(true)}
                                className="h-8"
                              >
                                <Edit className="h-3 w-3 mr-1" />
                                Manage Tariffs
                              </Button>
                            )}
                          </div>
                          {model.tariffs && model.tariffs.length > 0 ? (
                            <div className="space-y-3">
                              {model.tariffs.map((tariff) => (
                                <div
                                  key={tariff.id}
                                  className="bg-gray-50 rounded-lg p-3"
                                >
                                  <div className="flex items-center gap-2 mb-2">
                                    <p className="font-medium text-sm">
                                      {tariff.name}
                                    </p>
                                    <span className="text-xs text-gray-500 ml-auto">
                                      Valid from{" "}
                                      {new Date(
                                        tariff.valid_from,
                                      ).toLocaleString()}
                                    </span>
                                  </div>
                                  <div className="grid grid-cols-2 gap-4 text-sm">
                                    <div>
                                      <p className="text-xs text-gray-500">
                                        Input (per 1M tokens)
                                      </p>
                                      <p className="font-medium">
                                        $
                                        {(
                                          parseFloat(
                                            tariff.input_price_per_token,
                                          ) * 1000000
                                        ).toFixed(2)}
                                      </p>
                                    </div>
                                    <div>
                                      <p className="text-xs text-gray-500">
                                        Output (per 1M tokens)
                                      </p>
                                      <p className="font-medium">
                                        $
                                        {(
                                          parseFloat(
                                            tariff.output_price_per_token,
                                          ) * 1000000
                                        ).toFixed(2)}
                                      </p>
                                    </div>
                                  </div>
                                </div>
                              ))}
                            </div>
                          ) : (
                            <p className="text-sm text-gray-500 mt-2">
                              No tariffs configured.
                              {canManageGroups &&
                                ' Click "Manage Tariffs" to set up pricing.'}
                            </p>
                          )}
                        </div>
                      }

                      {/* Rate Limiting & Capacity Display - only show for Platform Managers */}
                      {canManageGroups &&
                        (model.requests_per_second !== undefined ||
                          model.burst_size !== undefined ||
                          model.capacity !== undefined ||
                          model.batch_capacity !== undefined ||
                          model.throughput !== undefined) && (
                          <div className="border-t pt-6">
                            <div className="flex items-center gap-1 mb-1">
                              <p className="text-sm text-gray-600">
                                Rate Limiting & Capacity
                              </p>
                              <InfoTip>
                                <p className="text-sm text-muted-foreground">
                                  Rate limits control request throughput.
                                  Capacity limits control concurrent requests.
                                </p>
                              </InfoTip>
                            </div>
                            <div className="grid grid-cols-1 md:grid-cols-2 gap-6">
                              <div>
                                <p className="text-xs text-gray-500 mb-1 flex items-center gap-1">
                                  Requests per Second
                                  <InfoTip>
                                    <p className="text-sm text-muted-foreground">
                                      Sustained request rate limit. Temporary
                                      bursts can exceed this up to the burst
                                      size. Exceeding this limit returns 429
                                      errors.
                                    </p>
                                  </InfoTip>
                                </p>
                                <p className="font-medium">
                                  {model.requests_per_second
                                    ? `${model.requests_per_second} req/s`
                                    : "No limit"}
                                </p>
                              </div>
                              <div>
                                <p className="text-xs text-gray-500 mb-1 flex items-center gap-1">
                                  Burst Size
                                  <InfoTip>
                                    <p className="text-sm text-muted-foreground">
                                      Maximum number of requests allowed in a
                                      temporary burst above the sustained
                                      rate.
                                    </p>
                                  </InfoTip>
                                </p>
                                <p className="font-medium">
                                  {model.burst_size
                                    ? model.burst_size.toLocaleString()
                                    : "No limit"}
                                </p>
                              </div>
                              <div>
                                <p className="text-xs text-gray-500 mb-1 flex items-center gap-1">
                                  Maximum Concurrent Requests
                                  <InfoTip>
                                    <p className="text-sm text-muted-foreground">
                                      Maximum number of requests that can be
                                      processed concurrently. Exceeding this
                                      limit returns 429 errors.
                                    </p>
                                  </InfoTip>
                                </p>
                                <p className="font-medium">
                                  {model.capacity
                                    ? `${model.capacity.toLocaleString()} concurrent`
                                    : "No limit"}
                                </p>
                              </div>
                              <div>
                                <p className="text-xs text-gray-500 mb-1 flex items-center gap-1">
                                  Per-Daemon Batch Concurrency
                                  <InfoTip>
                                    <p className="text-sm text-muted-foreground">
                                      Maximum concurrent batch requests each
                                      daemon can send to this model. Total
                                      capacity scales with the number of
                                      running daemons.
                                    </p>
                                  </InfoTip>
                                  {runningDaemonCount > 0 && (
                                    <span className="text-xs text-gray-500">
                                      ({runningDaemonCount}{" "}
                                      {runningDaemonCount === 1
                                        ? "daemon"
                                        : "daemons"}{" "}
                                      running)
                                    </span>
                                  )}
                                </p>
                                <p className="font-medium">
                                  {model.batch_capacity
                                    ? `${model.batch_capacity.toLocaleString()} per daemon`
                                    : "No limit"}
                                </p>
                              </div>
                              <div>
                                <p className="text-xs text-gray-500 mb-1 flex items-center gap-1">
                                  Throughput
                                  <InfoTip>
                                    <p className="text-sm text-muted-foreground">
                                      Model throughput in requests per second,
                                      used for batch SLA capacity
                                      calculations. Defaults to 100 req/s if
                                      not set.
                                    </p>
                                  </InfoTip>
                                </p>
                                <p className="font-medium">
                                  {model.throughput
                                    ? `${model.throughput} req/s`
                                    : "100 req/s (default)"}
                                </p>
                              </div>
                            </div>
                            {model.burst_size && !model.requests_per_second && (
                              <div className="mt-4 p-2 bg-yellow-50 border border-yellow-200 rounded-md">
                                <p className="text-xs text-yellow-700">
                                  ⚠️ Burst size will be ignored without requests
                                  per second. Set requests per second to enable
                                  rate limiting.
                                </p>
                              </div>
                            )}
                            {model.batch_capacity &&
                              model.capacity &&
                              runningDaemonCount > 0 &&
                              model.batch_capacity * runningDaemonCount >
                                model.capacity && (
                                <div className="mt-4 p-2 bg-yellow-50 border border-yellow-200 rounded-md">
                                  <p className="text-xs text-yellow-700">
                                    ⚠️ Total batch capacity (
                                    {(
                                      model.batch_capacity * runningDaemonCount
                                    ).toLocaleString()}{" "}
                                    = {model.batch_capacity.toLocaleString()} ×{" "}
                                    {runningDaemonCount}{" "}
                                    {runningDaemonCount === 1
                                      ? "daemon"
                                      : "daemons"}
                                    ) exceeds the maximum concurrent requests
                                    limit ({model.capacity.toLocaleString()}).
                                    Batch requests may be rate limited.
                                  </p>
                                </div>
                              )}
                          </div>
                        )}
                    </div>
                  )}
                </CardContent>
              </Card>

              {/* Usage Metrics - only show for users with Analytics permission */}
              {canViewAnalytics && model.metrics && (
                <Card className="p-0 gap-0 rounded-lg">
                  <CardHeader className="px-6 pt-5 pb-4">
                    <CardTitle>Usage Metrics</CardTitle>
                    <CardDescription>
                      Request statistics and performance data
                    </CardDescription>
                  </CardHeader>
                  <CardContent className="px-6 pb-6 pt-0">
                    <div className="grid grid-cols-2 md:grid-cols-4 gap-6">
                      <div>
                        <div className="flex items-center gap-1 mb-1">
                          <p className="text-sm text-gray-600">
                            Total Requests
                          </p>
                          <InfoTip className="w-40">
                            <p className="text-xs text-muted-foreground">
                              Total requests made to this model
                            </p>
                          </InfoTip>
                        </div>
                        <p className="text-xl font-bold text-gray-900">
                          {model.metrics.total_requests.toLocaleString()}
                        </p>
                      </div>
                      <div>
                        <div className="flex items-center gap-1 mb-1">
                          <p className="text-sm text-gray-600">Avg Latency</p>
                          <InfoTip className="w-40">
                            <p className="text-xs text-muted-foreground">
                              Average response time across all requests
                            </p>
                          </InfoTip>
                        </div>
                        <p className="text-xl font-bold text-gray-900">
                          {model.metrics.avg_latency_ms
                            ? model.metrics.avg_latency_ms >= 1000
                              ? `${(model.metrics.avg_latency_ms / 1000).toFixed(1)}s`
                              : `${Math.round(model.metrics.avg_latency_ms)}ms`
                            : "N/A"}
                        </p>
                      </div>
                      <div>
                        <div className="flex items-center gap-1 mb-1">
                          <p className="text-sm text-gray-600">Total Tokens</p>
                          <InfoTip className="w-48">
                            <div className="text-xs text-muted-foreground">
                              <p>
                                Input:{" "}
                                {model.metrics.total_input_tokens.toLocaleString()}
                              </p>
                              <p>
                                Output:{" "}
                                {model.metrics.total_output_tokens.toLocaleString()}
                              </p>
                              <p className="mt-1 font-medium">
                                Total tokens processed
                              </p>
                            </div>
                          </InfoTip>
                        </div>
                        <p className="text-xl font-bold text-gray-900">
                          {(
                            model.metrics.total_input_tokens +
                            model.metrics.total_output_tokens
                          ).toLocaleString()}
                        </p>
                        <p className="text-xs text-gray-500">
                          {model.metrics.total_input_tokens.toLocaleString()} in
                          + {model.metrics.total_output_tokens.toLocaleString()}{" "}
                          out
                        </p>
                      </div>
                      <div>
                        <div className="flex items-center gap-1 mb-1">
                          <p className="text-sm text-gray-600">Last Active</p>
                          <InfoTip className="w-36">
                            <p className="text-xs text-muted-foreground">
                              Last request received
                            </p>
                          </InfoTip>
                        </div>
                        <p className="text-xl font-bold text-gray-900">
                          {model.metrics.last_active_at
                            ? (() => {
                                const date = new Date(
                                  model.metrics.last_active_at,
                                );
                                const now = new Date();
                                const diffMs = now.getTime() - date.getTime();
                                const diffMinutes = Math.floor(
                                  diffMs / (1000 * 60),
                                );
                                const diffHours = Math.floor(
                                  diffMs / (1000 * 60 * 60),
                                );
                                const diffDays = Math.floor(
                                  diffMs / (1000 * 60 * 60 * 24),
                                );

                                if (diffMinutes < 1) return "Now";
                                if (diffMinutes < 60) return `${diffMinutes}m`;
                                if (diffHours < 24) return `${diffHours}h`;
                                if (diffDays < 7) return `${diffDays}d`;
                                return date.toLocaleDateString();
                              })()
                            : "Never"}
                        </p>
                      </div>
                    </div>
                  </CardContent>
                </Card>
              )}
            </div>

            {/* Sidebar */}
            <div className="space-y-6">
              {/* Quick Actions */}
              <Card className="p-0 gap-0 rounded-lg">
                <CardHeader className="px-6 pt-5 pb-4">
                  <CardTitle>Quick Actions</CardTitle>
                </CardHeader>
                <CardContent className="px-6 pb-6 pt-0 space-y-3">
                  <Button
                    className="w-full justify-start"
                    onClick={() => {
                      const currentUrl = `/models/${model.id}${fromUrl ? `?from=${encodeURIComponent(fromUrl)}` : ""}`;
                      navigate(
                        `/playground?model=${encodeURIComponent(model.alias)}&from=${encodeURIComponent(currentUrl)}`,
                      );
                    }}
                  >
                    <Play className="mr-2 h-4 w-4" />
                    Try in Playground
                  </Button>
                  {canViewAnalytics && (
                    <>
                      <Button
                        variant="outline"
                        className="w-full justify-start"
                        onClick={() => {
                          const currentUrl = `/models/${model.id}${fromUrl ? `?from=${encodeURIComponent(fromUrl)}` : ""}`;
                          navigate(
                            `/analytics?model=${encodeURIComponent(model.alias)}&from=${encodeURIComponent(currentUrl)}`,
                          );
                        }}
                      >
                        <BarChart3 className="mr-2 h-4 w-4" />
                        View Analytics
                      </Button>
                    </>
                  )}
                  <Button
                    variant="outline"
                    className="w-full justify-start"
                    onClick={() => setShowApiExamples(true)}
                  >
                    <Code className="mr-2 h-4 w-4" />
                    API Examples
                  </Button>
                  {canManageGroups && (
                    <Button
                      variant="outline"
                      className="w-full justify-start"
                      onClick={() => setShowAccessModal(true)}
                    >
                      <Users className="mr-2 h-4 w-4" />
                      Manage Access
                    </Button>
                  )}
                </CardContent>
              </Card>

              {/* Activity - only show for users with Analytics permission */}
              {canViewAnalytics &&
                model.metrics &&
                model.metrics.time_series && (
                  <Card className="p-0 gap-0 rounded-lg">
                    <CardHeader className="px-6 pt-5 pb-4">
                      <CardTitle>Activity</CardTitle>
                      <CardDescription>
                        Request volume over time
                      </CardDescription>
                    </CardHeader>
                    <CardContent className="px-6 pb-6 pt-0">
                      <div className="flex items-center justify-center">
                        <Sparkline
                          data={model.metrics.time_series}
                          width={280}
                          height={60}
                          className="w-full h-auto"
                        />
                      </div>
                    </CardContent>
                  </Card>
                )}
            </div>
          </div>
        </TabsContent>

        {canManageGroups && (
          <TabsContent value="usage">
            <UserUsageTable modelAlias={model.alias} />
          </TabsContent>
        )}

        {canManageGroups && (
          <TabsContent value="probes">
            <ModelProbes model={model} canManageProbes={canManageGroups} />
          </TabsContent>
        )}

        {canManageGroups && model.is_composite && (
          <TabsContent value="providers">
            <ProvidersTab model={model} canManage={canManageGroups} />
          </TabsContent>
        )}
      </Tabs>

      {/* API Examples Modal */}
      <ApiExamples
        isOpen={showApiExamples}
        onClose={() => setShowApiExamples(false)}
        model={model}
      />

      {/* Access Management Modal */}
      <AccessManagementModal
        isOpen={showAccessModal}
        onClose={() => setShowAccessModal(false)}
        model={model}
      />

      {/* Update Model Pricing Modal */}
      <UpdateModelPricingModal
        isOpen={showPricingModal}
        modelId={model.id}
        modelName={model.alias}
        onClose={() => setShowPricingModal(false)}
      />
    </div>
  );
};

export default ModelInfo;
