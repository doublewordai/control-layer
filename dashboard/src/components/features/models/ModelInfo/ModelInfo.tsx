import React, { useState, useEffect, useMemo } from "react";
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
} from "lucide-react";
import {
  useModels,
  useEndpoints,
  useUpdateModel,
  type ModelsInclude,
} from "../../../../api/control-layer";
import { useAuthorization } from "../../../../utils";
import { useSettings } from "../../../../contexts";
import {
  ApiExamples,
  AccessManagementModal,
  UpdateModelPricingModal,
} from "../../../modals";
import UserUsageTable from "./UserUsageTable";
import ModelProbes from "./ModelProbes";
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
import {
  HoverCard,
  HoverCardContent,
  HoverCardTrigger,
} from "../../../ui/hover-card";
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

// Form schema for alias editing
const aliasFormSchema = z.object({
  alias: z
    .string()
    .min(1, "Alias is required")
    .max(100, "Alias must be 100 characters or less"),
});

const ModelInfo: React.FC = () => {
  const { modelId } = useParams<{ modelId: string }>();
  const navigate = useNavigate();
  const [searchParams] = useSearchParams();
  const { hasPermission } = useAuthorization();
  const { isFeatureEnabled } = useSettings();
  const canManageGroups = hasPermission("manage-groups");
  const canViewAnalytics = hasPermission("analytics");
  const showPricing = isFeatureEnabled("use_billing");

  const fromUrl = searchParams.get("from");

  // Get tab from URL or default to "overview"
  const tabFromUrl = searchParams.get("tab");
  const [activeTab, setActiveTab] = useState<string>(() => {
    // Only allow usage tab if user has permission
    if (tabFromUrl === "usage" && canManageGroups) return "usage";
    if (tabFromUrl === "probes" && canManageGroups) return "probes";
    return "overview";
  });

  // Update activeTab when URL changes
  useEffect(() => {
    const tabFromUrl = searchParams.get("tab");
    if (
      tabFromUrl === "overview" ||
      (tabFromUrl === "usage" && canManageGroups) ||
      (tabFromUrl === "probes" && canManageGroups)
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
    requests_per_second: null as number | null,
    burst_size: null as number | null,
    capacity: null as number | null,
    batch_capacity: null as number | null,
  });
  const [settingsError, setSettingsError] = useState<string | null>(null);
  const [showApiExamples, setShowApiExamples] = useState(false);
  const [isEditingAlias, setIsEditingAlias] = useState(false);
  const [showAccessModal, setShowAccessModal] = useState(false);
  const [isEditingModelDetails, setIsEditingModelDetails] = useState(false);
  const [showPricingModal, setShowPricingModal] = useState(false);

  // Alias form
  const aliasForm = useForm<z.infer<typeof aliasFormSchema>>({
    resolver: zodResolver(aliasFormSchema),
    defaultValues: {
      alias: "",
    },
  });

  const updateModelMutation = useUpdateModel();

  // Build include parameter based on permissions - always include status to match Models page cache
  // IMPORTANT: Order must match Models.tsx to ensure cache reuse
  const includeParam = useMemo(() => {
    const parts: string[] = ["status"]; // Always include status to reuse cache from Models page
    if (showPricing) parts.push("pricing");
    if (canManageGroups) parts.push("groups");
    if (canViewAnalytics) parts.push("metrics");
    if (showPricing) parts.push("pricing");
    return parts.join(",");
  }, [canManageGroups, canViewAnalytics, showPricing]);

  const {
    data: rawModelsData,
    isLoading: modelsLoading,
    error: modelsError,
  } = useModels({
    include: includeParam as ModelsInclude,
    accessible: !canManageGroups, // Match Models page: admins see all (false), non-admins see accessible only (true)
  });

  const {
    data: endpointsData,
    isLoading: endpointsLoading,
    error: endpointsError,
  } = useEndpoints();

  const loading = modelsLoading || endpointsLoading;
  const error = modelsError
    ? (modelsError as Error).message
    : endpointsError
      ? (endpointsError as Error).message
      : null;

  // Find the specific model and endpoint
  const model = rawModelsData?.find((m) => m.id === modelId);
  const endpoint = endpointsData?.find((e) => e.id === model?.hosted_on);

  // Initialize form data when model is loaded
  useEffect(() => {
    if (model) {
      const effectiveType = model.model_type || "CHAT";

      setUpdateData({
        alias: model.alias,
        description: model.description || "",
        model_type: effectiveType as "CHAT" | "EMBEDDINGS",
        capabilities: model.capabilities || [],
        requests_per_second: model.requests_per_second || null,
        burst_size: model.burst_size || null,
        capacity: model.capacity || null,
        batch_capacity: model.batch_capacity || null,
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
          // Always include rate limiting and capacity fields to handle clearing properly
          // Send null as the actual value when clearing (not undefined)
          requests_per_second: updateData.requests_per_second,
          burst_size: updateData.burst_size,
          capacity: updateData.capacity,
          batch_capacity: updateData.batch_capacity,
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
        requests_per_second: model.requests_per_second || null,
        burst_size: model.burst_size || null,
        capacity: model.capacity || null,
        batch_capacity: model.batch_capacity || null,
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

  // Pricing modal handler
  const handlePricingSubmit = async (pricing: {
    input_price_per_token?: number;
    output_price_per_token?: number;
  }) => {
    if (!model) return;
    setSettingsError(null);

    try {
      await updateModelMutation.mutateAsync({
        id: model.id,
        data: {
          pricing: {
            input_price_per_token: pricing.input_price_per_token ?? null,
            output_price_per_token: pricing.output_price_per_token ?? null,
          },
        },
      });
      setShowPricingModal(false);
    } catch (error) {
      setSettingsError(
        error instanceof Error ? error.message : "Failed to update pricing",
      );
    }
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
            <div className="flex-1">
              <div className="flex flex-col sm:flex-row sm:items-center sm:justify-between gap-4">
                <div>
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
                    <h1 className="text-3xl font-bold text-doubleword-neutral-900">
                      {model.alias}
                    </h1>
                  )}
                  <p className="text-doubleword-neutral-600 mt-1">
                    {model.model_name} • {endpoint?.name || "Unknown endpoint"}
                  </p>
                </div>
                <div className="flex items-center justify-center sm:justify-start gap-3">
                  <TabsList className="w-full sm:w-auto">
                    <TabsTrigger
                      value="overview"
                      className="flex items-center gap-2"
                    >
                      <Info className="h-4 w-4" />
                      Overview
                    </TabsTrigger>
                    {canManageGroups && (
                      <TabsTrigger
                        value="usage"
                        className="flex items-center gap-2"
                      >
                        <Users className="h-4 w-4" />
                        Usage
                      </TabsTrigger>
                    )}
                    {canManageGroups && (
                      <TabsTrigger
                        value="probes"
                        className="flex items-center gap-2"
                      >
                        <Activity className="h-4 w-4" />
                        Uptime
                      </TabsTrigger>
                    )}
                  </TabsList>
                </div>
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
                          <HoverCard openDelay={100} closeDelay={50}>
                            <HoverCardTrigger asChild>
                              <Info className="h-3 w-3 text-gray-400 hover:text-gray-600" />
                            </HoverCardTrigger>
                            <HoverCardContent className="w-80" sideOffset={5}>
                              <p className="text-sm text-muted-foreground">
                                User provided description for the model.
                                Displayed to all users when viewing the model on
                                the overview page.
                              </p>
                            </HoverCardContent>
                          </HoverCard>
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
                          className="resize-none"
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
                              <HoverCard openDelay={100} closeDelay={50}>
                                <HoverCardTrigger asChild>
                                  <Info className="h-3 w-3 text-gray-400 hover:text-gray-600" />
                                </HoverCardTrigger>
                                <HoverCardContent
                                  className="w-80"
                                  sideOffset={5}
                                >
                                  <p className="text-sm text-muted-foreground">
                                    Enables image upload in the playground.
                                  </p>
                                </HoverCardContent>
                              </HoverCard>
                            </label>
                          </div>
                        </div>
                      )}

                      {/* Rate Limiting Section */}
                      <div className="border-t pt-4">
                        <div className="flex items-center gap-1 mb-3">
                          <label className="text-sm text-gray-600 font-medium">
                            Global Rate Limiting
                          </label>
                          <HoverCard openDelay={100} closeDelay={50}>
                            <HoverCardTrigger asChild>
                              <Info className="h-3 w-3 text-gray-400 hover:text-gray-600" />
                            </HoverCardTrigger>
                            <HoverCardContent className="w-80" sideOffset={5}>
                              <p className="text-sm text-muted-foreground">
                                Set system-wide rate limits for this model.
                                These apply to all users and override individual
                                API key limits.
                              </p>
                            </HoverCardContent>
                          </HoverCard>
                        </div>
                        <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
                          <div>
                            <label className="text-sm text-gray-600 mb-2 flex items-center gap-1">
                              Requests per Second
                              <HoverCard openDelay={100} closeDelay={50}>
                                <HoverCardTrigger asChild>
                                  <Info className="h-3 w-3 text-gray-400 hover:text-gray-600" />
                                </HoverCardTrigger>
                                <HoverCardContent className="w-80" sideOffset={5}>
                                  <p className="text-sm text-muted-foreground">
                                    Sustained request rate limit. Temporary bursts can exceed this up to the burst size. Exceeding this limit returns 429 errors.
                                  </p>
                                </HoverCardContent>
                              </HoverCard>
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
                              <HoverCard openDelay={100} closeDelay={50}>
                                <HoverCardTrigger asChild>
                                  <Info className="h-3 w-3 text-gray-400 hover:text-gray-600" />
                                </HoverCardTrigger>
                                <HoverCardContent className="w-80" sideOffset={5}>
                                  <p className="text-sm text-muted-foreground">
                                    Maximum number of requests allowed in a temporary burst above the sustained rate.
                                  </p>
                                </HoverCardContent>
                              </HoverCard>
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
                              <HoverCard openDelay={100} closeDelay={50}>
                                <HoverCardTrigger asChild>
                                  <Info className="h-3 w-3 text-gray-400 hover:text-gray-600" />
                                </HoverCardTrigger>
                                <HoverCardContent className="w-80" sideOffset={5}>
                                  <p className="text-sm text-muted-foreground">
                                    Maximum number of requests that can be processed concurrently. Exceeding this limit returns 429 errors.
                                  </p>
                                </HoverCardContent>
                              </HoverCard>
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
                              Maximum Batch Concurrent Requests
                              <HoverCard openDelay={100} closeDelay={50}>
                                <HoverCardTrigger asChild>
                                  <Info className="h-3 w-3 text-gray-400 hover:text-gray-600" />
                                </HoverCardTrigger>
                                <HoverCardContent className="w-80" sideOffset={5}>
                                  <p className="text-sm text-muted-foreground">
                                    Maximum number of concurrent requests the batching system can send to this model.
                                  </p>
                                </HoverCardContent>
                              </HoverCard>
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
                                  ? updateData.batch_capacity?.toString() || "None"
                                  : "None"
                              }
                            />
                          </div>
                        </div>
                        {(updateData.requests_per_second ||
                          updateData.burst_size ||
                          updateData.capacity ||
                          updateData.batch_capacity) && (
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
                                }))
                              }
                              className="text-xs"
                            >
                              Clear Rate Limits & Capacity
                            </Button>
                          </div>
                        )}
                        <p className="text-xs text-gray-500 mt-2">
                          Leave fields blank for no limits.
                        </p>
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
                          updateData.batch_capacity > updateData.capacity && (
                            <div className="mt-2 p-2 bg-yellow-50 border border-yellow-200 rounded-md">
                              <p className="text-xs text-yellow-700">
                                ⚠️ Maximum Batch Concurrent Requests is higher than Maximum Concurrent Requests. Batch requests may be rate limited.
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
                      <div className="grid grid-cols-1 md:grid-cols-3 gap-6">
                        <div>
                          <div className="flex items-center gap-1 mb-1">
                            <p className="text-sm text-gray-600">Full Name</p>
                            <HoverCard openDelay={100} closeDelay={50}>
                              <HoverCardTrigger asChild>
                                <Info className="h-3 w-3 text-gray-400 hover:text-gray-600 " />
                              </HoverCardTrigger>
                              <HoverCardContent className="w-80" sideOffset={5}>
                                <p className="text-sm text-muted-foreground">
                                  The name under which the model is available at
                                  the upstream endpoint.
                                </p>
                              </HoverCardContent>
                            </HoverCard>
                          </div>
                          <p className="font-medium">{model.model_name}</p>
                        </div>
                        <div>
                          <div className="flex items-center gap-1 mb-1">
                            <p className="text-sm text-gray-600">Alias</p>
                            <HoverCard openDelay={100} closeDelay={50}>
                              <HoverCardTrigger asChild>
                                <Info className="h-3 w-3 text-gray-400 hover:text-gray-600 " />
                              </HoverCardTrigger>
                              <HoverCardContent className="w-80" sideOffset={5}>
                                <p className="text-sm text-muted-foreground">
                                  The name under which the model will be made
                                  available in the control layer API.
                                </p>
                              </HoverCardContent>
                            </HoverCard>
                          </div>
                          <p className="font-medium">{model.alias}</p>
                        </div>
                        <div>
                          <div className="flex items-center gap-1 mb-1">
                            <p className="text-sm text-gray-600">Type</p>
                            <HoverCard openDelay={100} closeDelay={50}>
                              <HoverCardTrigger asChild>
                                <Info className="h-3 w-3 text-gray-400 hover:text-gray-600" />
                              </HoverCardTrigger>
                              <HoverCardContent className="w-80" sideOffset={5}>
                                <p className="text-sm text-muted-foreground">
                                  The type of the model. Determines which
                                  playground is used.
                                </p>
                              </HoverCardContent>
                            </HoverCard>
                          </div>
                          <Badge variant="outline">
                            {model.model_type || "UNKNOWN"}
                          </Badge>
                        </div>
                      </div>
                      <div>
                        <div className="flex items-center gap-1 mb-1">
                          <p className="text-sm text-gray-600">Description</p>
                          <HoverCard openDelay={100} closeDelay={50}>
                            <HoverCardTrigger asChild>
                              <Info className="h-3 w-3 text-gray-400 hover:text-gray-600" />
                            </HoverCardTrigger>
                            <HoverCardContent className="w-80" sideOffset={5}>
                              <p className="text-sm text-muted-foreground">
                                User provided description for the model.
                                Displayed to all users when viewing the model on
                                the overview page.
                              </p>
                            </HoverCardContent>
                          </HoverCard>
                        </div>
                        <p className="text-gray-700">
                          {model.description || "No description provided"}
                        </p>
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
                                    ? [...(model.capabilities || []), "vision"]
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
                                <HoverCard openDelay={100} closeDelay={50}>
                                  <HoverCardTrigger asChild>
                                    <Info className="h-3 w-3 text-gray-400 hover:text-gray-600" />
                                  </HoverCardTrigger>
                                  <HoverCardContent
                                    className="w-80"
                                    sideOffset={5}
                                  >
                                    <p className="text-sm text-muted-foreground">
                                      Enables image upload in the playground.
                                    </p>
                                  </HoverCardContent>
                                </HoverCard>
                              </label>
                            </div>
                          </div>
                        )}

                      {/* Pricing Display - visible to all users when billing is enabled */}
                      {showPricing && (
                        <div className="border-t pt-6">
                          <div className="flex items-center justify-between mb-1">
                            <div className="flex items-center gap-1">
                              <p className="text-sm text-gray-600">Pricing</p>
                              <HoverCard openDelay={100} closeDelay={50}>
                                <HoverCardTrigger asChild>
                                  <Info className="h-3 w-3 text-gray-400 hover:text-gray-600" />
                                </HoverCardTrigger>
                                <HoverCardContent
                                  className="w-80"
                                  sideOffset={5}
                                >
                                  <p className="text-sm text-muted-foreground">
                                    Customer-facing pricing rates per token.
                                    {canManageGroups && ` Click ${model.pricing ? "Edit" : "Set Pricing"} to update pricing.`}
                                  </p>
                                </HoverCardContent>
                              </HoverCard>
                            </div>
                            {canManageGroups && (
                              <Button
                                variant="outline"
                                size="sm"
                                onClick={() => setShowPricingModal(true)}
                                className="h-8"
                              >
                                <Edit className="h-3 w-3 mr-1" />
                                {model.pricing ? "Edit" : "Set Pricing"}
                              </Button>
                            )}
                          </div>
                          {model.pricing ? (
                            <div className="grid grid-cols-1 md:grid-cols-2 gap-6">
                              <div>
                                <p className="text-xs text-gray-500 mb-1">
                                  Input Price per Token
                                </p>
                                <p className="font-medium">
                                  {model.pricing.input_price_per_token
                                    ? `$${Number(model.pricing.input_price_per_token).toFixed(4)}`
                                    : "Not set"}
                                </p>
                              </div>
                              <div>
                                <p className="text-xs text-gray-500 mb-1">
                                  Output Price per Token
                                </p>
                                <p className="font-medium">
                                  {model.pricing.output_price_per_token
                                    ? `$${Number(model.pricing.output_price_per_token).toFixed(4)}`
                                    : "Not set"}
                                </p>
                              </div>
                            </div>
                          ) : (
                            <p className="text-sm text-gray-500 mt-2">
                              No pricing configured.{canManageGroups && ' Click "Set Pricing" to add pricing information.'}
                            </p>
                          )}
                        </div>
                      )}

                      {/* Rate Limiting & Capacity Display - only show for Platform Managers */}
                      {canManageGroups &&
                        (model.requests_per_second !== undefined ||
                          model.burst_size !== undefined ||
                          model.capacity !== undefined ||
                          model.batch_capacity !== undefined) && (
                          <div className="border-t pt-6">
                            <div className="flex items-center gap-1 mb-1">
                              <p className="text-sm text-gray-600">
                                Rate Limiting & Capacity
                              </p>
                              <HoverCard openDelay={100} closeDelay={50}>
                                <HoverCardTrigger asChild>
                                  <Info className="h-3 w-3 text-gray-400 hover:text-gray-600" />
                                </HoverCardTrigger>
                                <HoverCardContent
                                  className="w-80"
                                  sideOffset={5}
                                >
                                  <p className="text-sm text-muted-foreground">
                                    Rate limits control request throughput. Capacity limits control concurrent requests.
                                  </p>
                                </HoverCardContent>
                              </HoverCard>
                            </div>
                            <div className="grid grid-cols-1 md:grid-cols-2 gap-6">
                              <div>
                                <p className="text-xs text-gray-500 mb-1 flex items-center gap-1">
                                  Requests per Second
                                  <HoverCard openDelay={100} closeDelay={50}>
                                    <HoverCardTrigger asChild>
                                      <Info className="h-3 w-3 text-gray-400 hover:text-gray-600" />
                                    </HoverCardTrigger>
                                    <HoverCardContent className="w-80" sideOffset={5}>
                                      <p className="text-sm text-muted-foreground">
                                        Sustained request rate limit. Temporary bursts can exceed this up to the burst size. Exceeding this limit returns 429 errors.
                                      </p>
                                    </HoverCardContent>
                                  </HoverCard>
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
                                  <HoverCard openDelay={100} closeDelay={50}>
                                    <HoverCardTrigger asChild>
                                      <Info className="h-3 w-3 text-gray-400 hover:text-gray-600" />
                                    </HoverCardTrigger>
                                    <HoverCardContent className="w-80" sideOffset={5}>
                                      <p className="text-sm text-muted-foreground">
                                        Maximum number of requests allowed in a temporary burst above the sustained rate.
                                      </p>
                                    </HoverCardContent>
                                  </HoverCard>
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
                                  <HoverCard openDelay={100} closeDelay={50}>
                                    <HoverCardTrigger asChild>
                                      <Info className="h-3 w-3 text-gray-400 hover:text-gray-600" />
                                    </HoverCardTrigger>
                                    <HoverCardContent className="w-80" sideOffset={5}>
                                      <p className="text-sm text-muted-foreground">
                                        Maximum number of requests that can be processed concurrently. Exceeding this limit returns 429 errors.
                                      </p>
                                    </HoverCardContent>
                                  </HoverCard>
                                </p>
                                <p className="font-medium">
                                  {model.capacity
                                    ? `${model.capacity.toLocaleString()} concurrent`
                                    : "No limit"}
                                </p>
                              </div>
                              <div>
                                <p className="text-xs text-gray-500 mb-1 flex items-center gap-1">
                                  Maximum Batch Concurrent Requests
                                  <HoverCard openDelay={100} closeDelay={50}>
                                    <HoverCardTrigger asChild>
                                      <Info className="h-3 w-3 text-gray-400 hover:text-gray-600" />
                                    </HoverCardTrigger>
                                    <HoverCardContent className="w-80" sideOffset={5}>
                                      <p className="text-sm text-muted-foreground">
                                        Maximum number of concurrent requests the batching system can send to this model.
                                      </p>
                                    </HoverCardContent>
                                  </HoverCard>
                                </p>
                                <p className="font-medium">
                                  {model.batch_capacity
                                    ? `${model.batch_capacity.toLocaleString()} concurrent`
                                    : "No limit"}
                                </p>
                              </div>
                            </div>
                            {model.burst_size && !model.requests_per_second && (
                              <div className="mt-4 p-2 bg-yellow-50 border border-yellow-200 rounded-md">
                                <p className="text-xs text-yellow-700">
                                  ⚠️ Burst size will be ignored without requests per second. Set requests per second to enable rate limiting.
                                </p>
                              </div>
                            )}
                            {model.batch_capacity && model.capacity && model.batch_capacity > model.capacity && (
                              <div className="mt-4 p-2 bg-yellow-50 border border-yellow-200 rounded-md">
                                <p className="text-xs text-yellow-700">
                                  ⚠️ Maximum Batch Concurrent Requests is higher than Maximum Concurrent Requests. Batch requests may be rate limited.
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
                          <HoverCard openDelay={100} closeDelay={50}>
                            <HoverCardTrigger asChild>
                              <Info className="h-3 w-3 text-gray-400 hover:text-gray-600 " />
                            </HoverCardTrigger>
                            <HoverCardContent className="w-40" sideOffset={5}>
                              <p className="text-xs text-muted-foreground">
                                Total requests made to this model
                              </p>
                            </HoverCardContent>
                          </HoverCard>
                        </div>
                        <p className="text-xl font-bold text-gray-900">
                          {model.metrics.total_requests.toLocaleString()}
                        </p>
                      </div>
                      <div>
                        <div className="flex items-center gap-1 mb-1">
                          <p className="text-sm text-gray-600">Avg Latency</p>
                          <HoverCard openDelay={100} closeDelay={50}>
                            <HoverCardTrigger asChild>
                              <Info className="h-3 w-3 text-gray-400 hover:text-gray-600 " />
                            </HoverCardTrigger>
                            <HoverCardContent className="w-40" sideOffset={5}>
                              <p className="text-xs text-muted-foreground">
                                Average response time across all requests
                              </p>
                            </HoverCardContent>
                          </HoverCard>
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
                          <HoverCard openDelay={100} closeDelay={50}>
                            <HoverCardTrigger asChild>
                              <Info className="h-3 w-3 text-gray-400 hover:text-gray-600 " />
                            </HoverCardTrigger>
                            <HoverCardContent className="w-48" sideOffset={5}>
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
                            </HoverCardContent>
                          </HoverCard>
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
                          <HoverCard openDelay={100} closeDelay={50}>
                            <HoverCardTrigger asChild>
                              <Info className="h-3 w-3 text-gray-400 hover:text-gray-600 " />
                            </HoverCardTrigger>
                            <HoverCardContent className="w-36" sideOffset={5}>
                              <p className="text-xs text-muted-foreground">
                                Last request received
                              </p>
                            </HoverCardContent>
                          </HoverCard>
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
                  <Button
                    variant="outline"
                    className="w-full justify-start"
                    onClick={() => {
                      const currentUrl = `/models/${model.id}${fromUrl ? `?from=${encodeURIComponent(fromUrl)}` : ""}`;
                      navigate(
                        `/analytics?model=${encodeURIComponent(model.alias)}&tab=requests&from=${encodeURIComponent(currentUrl)}`,
                      );
                    }}
                  >
                    <Activity className="mr-2 h-4 w-4" />
                    View Traffic
                  </Button>
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
            <UserUsageTable modelAlias={model.alias} showPricing={showPricing} />
          </TabsContent>
        )}

        {canManageGroups && (
          <TabsContent value="probes">
            <ModelProbes model={model} />
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
        modelName={model.alias}
        currentPricing={model.pricing}
        onSubmit={handlePricingSubmit}
        onClose={() => setShowPricingModal(false)}
        isLoading={updateModelMutation.isPending}
      />
    </div>
  );
};

export default ModelInfo;
