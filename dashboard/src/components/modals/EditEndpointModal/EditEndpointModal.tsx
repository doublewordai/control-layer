import React, { useState, useEffect } from "react";
import {
  Server,
  Check,
  AlertCircle,
  Loader2,
  Edit2,
  X,
  Info,
  ChevronRight,
  ChevronDown,
  Eye,
  EyeOff,
} from "lucide-react";
import { useForm } from "react-hook-form";
import { zodResolver } from "@hookform/resolvers/zod";
import * as z from "zod";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
  DialogDescription,
} from "../../ui/dialog";
import {
  Form,
  FormControl,
  FormField,
  FormItem,
  FormLabel,
  FormMessage,
} from "../../ui/form";
import { Input } from "../../ui/input";
import { Textarea } from "../../ui/textarea";
import { Checkbox } from "../../ui/checkbox";
import { Button } from "../../ui/button";
import {
  HoverCard,
  HoverCardContent,
  HoverCardTrigger,
} from "../../ui/hover-card";
import { Popover, PopoverContent, PopoverTrigger } from "../../ui/popover";
import {
  useValidateEndpoint,
  useUpdateEndpoint,
  dwctlApi,
} from "../../../api/control-layer";
import type {
  EndpointValidateRequest,
  AvailableModel,
  EndpointUpdateRequest,
  Endpoint,
} from "../../../api/control-layer/types";

interface EditEndpointModalProps {
  isOpen: boolean;
  onClose: () => void;
  onSuccess: () => void;
  endpoint: Endpoint;
}

type ValidationState = "idle" | "testing" | "success" | "error";

const formSchema = z.object({
  url: z.string().min(1, "URL is required").url("Please enter a valid URL"),
  apiKey: z.string().optional(),
  name: z.string().min(1, "Endpoint name is required"),
  description: z.string().optional(),
  selectedModels: z.array(z.string()),
  authHeaderName: z.string().optional(),
  authHeaderPrefix: z.string().optional(),
});

type FormData = z.infer<typeof formSchema>;

export const EditEndpointModal: React.FC<EditEndpointModalProps> = ({
  isOpen,
  onClose,
  onSuccess,
  endpoint,
}) => {
  const [validationState, setValidationState] =
    useState<ValidationState>("idle");
  const [validationError, setValidationError] = useState<string | null>(null);
  const [availableModels, setAvailableModels] = useState<AvailableModel[]>([]);
  const [modelAliases, setModelAliases] = useState<Record<string, string>>({});

  // Track conflicts between aliases both locally and from backend
  const [backendConflicts, setBackendConflicts] = useState<Set<string>>(
    new Set(),
  );
  const [localConflicts, setLocalConflicts] = useState<string[]>([]);

  // Track if URL has changed to require re-validation
  const [urlChanged, setUrlChanged] = useState(false);

  // Manual model configuration
  const [manualMode, setManualMode] = useState(false);
  const [manualModelInput, setManualModelInput] = useState("");
  const [autoDiscoverModels, setAutoDiscoverModels] = useState(true);

  // Step navigation
  const [currentStep, setCurrentStep] = useState<1 | 2>(1);

  // Advanced configuration
  const [advancedPopoverOpen, setAdvancedPopoverOpen] = useState(false);
  const [showApiKey, setShowApiKey] = useState(false);

  const validateEndpointMutation = useValidateEndpoint();
  const updateEndpointMutation = useUpdateEndpoint();

  const form = useForm<FormData>({
    resolver: zodResolver(formSchema),
    defaultValues: {
      url: "",
      apiKey: "",
      name: "",
      description: "",
      selectedModels: [],
      authHeaderName: "",
      authHeaderPrefix: "",
    },
  });

  // Initialize form with endpoint data and fetch current deployments
  useEffect(() => {
    // Fetch current deployments to get actual aliases being used
    const fetchCurrentDeployments = async () => {
      try {
        const currentModels = await dwctlApi.models.list({
          endpoint: endpoint.id,
        });

        // Build current alias mapping from deployed models
        const currentAliases: Record<string, string> = {};
        currentModels.data.forEach((model) => {
          currentAliases[model.model_name] = model.alias;
        });

        setModelAliases(currentAliases);
      } catch (error) {
        console.error("Failed to fetch current deployments:", error);
        // Graceful fallback - new models will default to model name as alias
      }
    };

    if (isOpen && endpoint) {
      form.reset({
        url: endpoint.url,
        apiKey: "",
        name: endpoint.name,
        description: endpoint.description || "",
        selectedModels: endpoint.model_filter || [],
        authHeaderName: "",
        authHeaderPrefix: "",
      });

      setValidationState("idle");
      setValidationError(null);
      setAvailableModels([]);
      setModelAliases({});
      setBackendConflicts(new Set());
      setLocalConflicts([]);
      setUrlChanged(false);

      fetchCurrentDeployments();
    }
  }, [isOpen, endpoint, form]);

  // Check for duplicate aliases among currently selected models
  const checkLocalAliasConflicts = (
    updatedAliases?: Record<string, string>,
  ) => {
    const selectedModels = form.getValues("selectedModels") || [];
    const aliasesToCheck = updatedAliases || modelAliases;
    const selectedAliases = selectedModels.map(
      (id) => aliasesToCheck[id] || id,
    );
    const duplicateAliases = selectedAliases.filter(
      (alias, index) => selectedAliases.indexOf(alias) !== index,
    );
    return [...new Set(duplicateAliases)];
  };

  // Check for both local and backend alias conflicts for a specific model
  const checkAliasConflict = (currentModelId: string, aliasToCheck: string) => {
    const selectedModels = form.getValues("selectedModels") || [];
    const localConflict = Object.entries(modelAliases).some(
      ([modelId, alias]) =>
        modelId !== currentModelId &&
        alias === aliasToCheck &&
        selectedModels.includes(modelId),
    );

    const backendConflict = backendConflicts.has(aliasToCheck);

    return {
      hasConflict: localConflict || backendConflict,
      hasLocalConflict: localConflict,
      hasBackendConflict: backendConflict,
    };
  };

  const handleTestConnection = async () => {
    const url = form.getValues("url");

    if (!url.trim()) {
      form.setError("url", { message: "Please enter a URL" });
      return;
    }

    form.clearErrors("url");
    setValidationState("testing");
    setValidationError(null);

    // Use existing endpoint validation to check models available
    const validateData: EndpointValidateRequest = {
      type: "existing",
      endpoint_id: endpoint.id,
    };

    try {
      const result = await validateEndpointMutation.mutateAsync(validateData);

      if (result.status === "success" && result.models) {
        setAvailableModels(result.models.data);
        setValidationState("success");
        setUrlChanged(false);
        setManualMode(false);

        // Merge existing aliases with new models from the endpoint
        const updatedAliases = { ...modelAliases };

        result.models.data.forEach((model) => {
          // Only set default alias if we don't already have one for this model
          if (!updatedAliases[model.id]) {
            updatedAliases[model.id] = model.id;
          }
        });

        setModelAliases(updatedAliases);

        // Restore previous selection if available
        const currentSelection = form.getValues("selectedModels") || [];
        if (currentSelection.length === 0 && endpoint.model_filter) {
          form.setValue("selectedModels", endpoint.model_filter);
        }

        setCurrentStep(2);
      } else {
        setValidationError(result.error || "Unknown validation error");
        setValidationState("error");
      }
    } catch (err) {
      setValidationError(
        err instanceof Error ? err.message : "Failed to test connection",
      );
      setValidationState("error");
    }
  };

  const handleUrlChange = (newUrl: string) => {
    const isChanged = newUrl.trim() !== endpoint.url;
    setUrlChanged(isChanged);

    // Reset validation state if URL changed
    if (isChanged && validationState === "success") {
      setValidationState("idle");
      setAvailableModels([]);
      form.setValue("selectedModels", []);
      setModelAliases({});
      setBackendConflicts(new Set());
      setLocalConflicts([]);
    }

    if (validationError) {
      setValidationError(null);
      if (!isChanged) {
        setValidationState("success");
      } else {
        setValidationState("idle");
      }
    }
  };

  const handleConfigureManually = () => {
    setManualMode(true);
    setCurrentStep(2);
    setValidationState("idle");
    setValidationError(null);

    // Pre-populate with current model filter if available
    if (endpoint.model_filter && endpoint.model_filter.length > 0) {
      setManualModelInput(endpoint.model_filter.join("\n"));
    }
  };

  const handleApplyManualModels = () => {
    // Parse manual input
    const modelNames = manualModelInput
      .split("\n")
      .map((s) => s.trim())
      .filter(Boolean);

    if (modelNames.length === 0) {
      setValidationError("Please enter at least one model name");
      return;
    }

    // Create AvailableModel objects from model names
    const models: AvailableModel[] = modelNames.map((name) => ({
      id: name,
      created: 0,
      object: "model" as const,
      owned_by: "",
    }));

    setAvailableModels(models);
    setValidationState("success");

    // Initialize aliases to model names
    const initialAliases = models.reduce(
      (acc, model) => ({
        ...acc,
        [model.id]: model.id,
      }),
      {},
    );
    setModelAliases(initialAliases);

    // Select all models by default
    form.setValue(
      "selectedModels",
      models.map((m) => m.id),
    );

    setUrlChanged(false); // Mark as validated since we have models
    setCurrentStep(2);
  };

  const handleBack = () => {
    setCurrentStep(1);
    setValidationState("idle");
    setValidationError(null);
    setBackendConflicts(new Set());
    setLocalConflicts([]);
  };

  const onSubmit = async (data: FormData) => {
    if (urlChanged && validationState !== "success") {
      setValidationError(
        "Please test the endpoint connection after changing the URL",
      );
      return;
    }

    setBackendConflicts(new Set());

    // Build alias mapping for all selected models
    const aliasMapping: Record<string, string> = {};
    data.selectedModels.forEach((modelId) => {
      const alias = modelAliases[modelId] || modelId;
      aliasMapping[modelId] = alias.trim();
    });

    const updateData: EndpointUpdateRequest = {
      name: data.name.trim(),
      url: data.url.trim(),
      ...(data.description?.trim() && { description: data.description.trim() }),
      ...(data.apiKey?.trim() && { api_key: data.apiKey.trim() }),
      ...(data.selectedModels.length > 0 && {
        model_filter: data.selectedModels,
      }),
      ...(data.selectedModels.length > 0 && { alias_mapping: aliasMapping }),
      ...(data.authHeaderName?.trim() && {
        auth_header_name: data.authHeaderName.trim(),
      }),
      ...(data.authHeaderPrefix?.trim() && {
        auth_header_prefix: data.authHeaderPrefix.trim(),
      }),
    };

    try {
      await updateEndpointMutation.mutateAsync({
        id: endpoint.id.toString(),
        data: updateData,
      });
      onSuccess();
      onClose();
    } catch (err: any) {
      // Handle different types of conflicts
      if (err.status === 409 || err.response?.status === 409) {
        const responseData = err.response?.data || err.data;

        if (responseData?.resource === "endpoint") {
          form.setError("name", {
            type: "endpoint_name_conflict",
            message: "Endpoint name already exists.",
          });
          setValidationError("Please choose a different endpoint name.");
          return;
        }

        // Handle structured alias conflicts from backend
        if (responseData && responseData.conflicts) {
          const conflictAliases = responseData.conflicts.map(
            (c: any) => c.attempted_alias || c.alias,
          );
          setBackendConflicts(new Set(conflictAliases));
          setValidationError(
            "Some model aliases already exist. Please edit the highlighted aliases.",
          );
        } else {
          setValidationError(
            responseData?.message ||
              "A conflict occurred. Please check your input.",
          );
        }
      } else {
        const errorMessage = err.message || "Failed to update endpoint";
        setValidationError(errorMessage);
      }
    }
  };

  // Model row component with inline alias editing functionality
  interface ModelRowWithAliasProps {
    model: AvailableModel;
    isSelected: boolean;
    alias: string;
    onSelectionChange: (checked: boolean) => void;
    onAliasChange: (newAlias: string) => void;
    conflictInfo: {
      hasConflict: boolean;
      hasLocalConflict: boolean;
      hasBackendConflict: boolean;
    };
    onConflictClear: (oldAlias: string) => void;
  }

  const ModelRowWithAlias: React.FC<ModelRowWithAliasProps> = ({
    model,
    isSelected,
    alias,
    onSelectionChange,
    onAliasChange,
    conflictInfo,
    onConflictClear,
  }) => {
    const [isEditing, setIsEditing] = useState(false);
    const [tempAlias, setTempAlias] = useState(alias);

    useEffect(() => {
      setTempAlias(alias);
    }, [alias]);

    const { hasConflict, hasLocalConflict, hasBackendConflict } = conflictInfo;

    const handleSaveAlias = () => {
      const newAlias = tempAlias.trim() || model.id;
      onAliasChange(newAlias);
      setIsEditing(false);

      // Clear backend conflict if alias was changed
      if (hasBackendConflict && newAlias !== alias) {
        onConflictClear(alias);
      }
    };

    const handleCancelEdit = () => {
      setTempAlias(alias);
      setIsEditing(false);
    };

    return (
      <div
        className={`grid grid-cols-12 gap-3 p-3 border-b last:border-b-0 hover:bg-gray-50 transition-colors ${
          hasBackendConflict
            ? "bg-red-50/50 border-l-4 border-l-red-400"
            : hasLocalConflict
              ? "bg-orange-50/50 border-l-4 border-l-orange-400"
              : ""
        }`}
      >
        <div className="col-span-1 flex items-center">
          <Checkbox checked={isSelected} onCheckedChange={onSelectionChange} />
        </div>

        <div className="col-span-5 flex flex-col justify-center min-w-0">
          <p className="text-sm font-medium truncate">{model.id}</p>
          <p className="text-xs text-gray-500 truncate">{model.owned_by}</p>
        </div>

        <div className="col-span-6 flex items-center space-x-2">
          {isEditing ? (
            <div className="flex items-center space-x-1 flex-1">
              <Input
                value={tempAlias}
                onChange={(e) => setTempAlias(e.target.value)}
                className={`text-sm h-8 ${
                  hasBackendConflict
                    ? "border-red-400 focus:border-red-500 focus:ring-red-500/20"
                    : hasLocalConflict
                      ? "border-orange-400 focus:border-orange-500 focus:ring-orange-500/20"
                      : ""
                }`}
                placeholder={model.id}
                onKeyDown={(e) => {
                  if (e.key === "Enter") handleSaveAlias();
                  if (e.key === "Escape") handleCancelEdit();
                }}
                autoFocus
              />
              <Button
                type="button"
                variant="ghost"
                size="sm"
                onClick={handleSaveAlias}
                className="h-8 w-8 p-0"
              >
                <Check className="w-3 h-3" />
              </Button>
              <Button
                type="button"
                variant="ghost"
                size="sm"
                onClick={handleCancelEdit}
                className="h-8 w-8 p-0"
              >
                <X className="w-3 h-3" />
              </Button>
            </div>
          ) : (
            <div className="flex items-center justify-between flex-1">
              <div className="flex items-center space-x-2 flex-1 min-w-0">
                <span
                  className={`text-sm truncate ${
                    hasBackendConflict
                      ? "text-red-700"
                      : hasLocalConflict
                        ? "text-orange-600"
                        : alias !== model.id
                          ? "text-blue-600"
                          : "text-gray-700"
                  }`}
                >
                  {alias}
                </span>
                {alias !== model.id && !hasConflict && (
                  <span
                    className="text-xs text-blue-500"
                    aria-label="Custom alias"
                  >
                    (custom)
                  </span>
                )}
              </div>
              <Button
                type="button"
                variant="ghost"
                size="sm"
                onClick={() => setIsEditing(true)}
                className="h-8 w-8 p-0 text-gray-400 hover:text-gray-600"
                disabled={!isSelected}
                title="Edit alias"
              >
                <Edit2 className="w-3 h-3" />
              </Button>
            </div>
          )}
        </div>
      </div>
    );
  };

  const handleSelectAll = () => {
    const currentSelection = form.getValues("selectedModels") || [];
    const allModelIds = availableModels.map((m) => m.id);

    if (currentSelection.length === availableModels.length) {
      form.setValue("selectedModels", []);
    } else {
      form.setValue("selectedModels", allModelIds);

      // Ensure all models have aliases set
      const newAliases = { ...modelAliases };
      allModelIds.forEach((modelId) => {
        if (!newAliases[modelId]) {
          newAliases[modelId] = modelId;
        }
      });
      setModelAliases(newAliases);
    }
  };

  const canUpdate =
    form.watch("name")?.trim() &&
    !updateEndpointMutation.isPending &&
    validationState !== "testing" &&
    (!urlChanged || validationState === "success") &&
    backendConflicts.size === 0 &&
    localConflicts.length === 0;

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-2xl max-h-[90vh] overflow-y-auto [&>button]:hidden">
        <DialogHeader>
          <div className="flex items-center justify-between">
            <DialogTitle>Edit Endpoint</DialogTitle>

            {/* Stepper */}
            <div className="flex items-center space-x-2">
              {/* Step 1: Connection */}
              <div
                className={`flex items-center ${
                  currentStep === 1
                    ? "text-gray-700 font-medium"
                    : currentStep > 1
                      ? "text-emerald-600 font-medium"
                      : "text-gray-400"
                }`}
              >
                <div
                  className={`w-8 h-8 rounded-full flex items-center justify-center border-2 ${
                    currentStep === 1
                      ? "border-gray-700 bg-gray-700 text-white"
                      : currentStep > 1
                        ? "border-emerald-500 bg-emerald-500 text-white"
                        : "border-gray-300 text-gray-400"
                  }`}
                >
                  {currentStep > 1 ? <Check className="w-4 h-4" /> : "1"}
                </div>
                <span className="ml-2 text-sm">Connection</span>
              </div>

              {/* Connector Line */}
              <div
                className={`w-12 h-0.5 ${currentStep > 1 ? "bg-emerald-500" : "bg-gray-300"}`}
              />

              {/* Step 2: Models */}
              <div
                className={`flex items-center ${
                  currentStep === 2
                    ? "text-gray-700 font-medium"
                    : "text-gray-400"
                }`}
              >
                <div
                  className={`w-8 h-8 rounded-full flex items-center justify-center border-2 ${
                    currentStep === 2
                      ? "border-gray-700 bg-gray-700 text-white"
                      : "border-gray-300 text-gray-400"
                  }`}
                >
                  2
                </div>
                <span className="ml-2 text-sm">Models</span>
              </div>
            </div>
          </div>
          <DialogDescription>Adjust endpoint settings</DialogDescription>
        </DialogHeader>

        <Form {...form}>
          <form onSubmit={form.handleSubmit(onSubmit)} className="space-y-6">
            {/* Step 1: Connection Details */}
            {currentStep === 1 && (
              <div className="space-y-6">
                <FormField
                  control={form.control}
                  name="url"
                  render={({ field }) => (
                    <FormItem>
                      <FormLabel>
                        Endpoint URL *
                        {urlChanged && (
                          <span className="text-yellow-600 text-xs ml-2">
                            (Changed - requires testing)
                          </span>
                        )}
                      </FormLabel>
                      <FormControl>
                        <Input
                          placeholder="https://api.example.com"
                          {...field}
                          onChange={(e) => {
                            field.onChange(e);
                            handleUrlChange(e.target.value);
                          }}
                        />
                      </FormControl>
                      <FormMessage />
                    </FormItem>
                  )}
                />

                {endpoint.requires_api_key && (
                  <FormField
                    control={form.control}
                    name="apiKey"
                    render={({ field }) => (
                      <FormItem>
                        <FormLabel>
                          API Key (optional)
                          <span className="text-xs text-gray-500 ml-2">
                            Leave empty to keep existing key
                          </span>
                        </FormLabel>
                        <FormControl>
                          <div className="relative">
                            <Input
                              type={showApiKey ? "text" : "password"}
                              placeholder="sk-..."
                              {...field}
                              className="pr-10"
                            />
                            <button
                              type="button"
                              className="absolute right-0 top-0 h-full px-3 text-gray-500 hover:text-gray-700 transition-colors"
                              onClick={() => setShowApiKey(!showApiKey)}
                            >
                              {showApiKey ? (
                                <EyeOff className="h-4 w-4" />
                              ) : (
                                <Eye className="h-4 w-4" />
                              )}
                              <span className="sr-only">
                                {showApiKey ? "Hide API key" : "Show API key"}
                              </span>
                            </button>
                          </div>
                        </FormControl>
                      </FormItem>
                    )}
                  />
                )}

                {/* Advanced Configuration and Auto-discover */}
                <div className="flex items-center justify-between">
                  <Popover
                    open={advancedPopoverOpen}
                    onOpenChange={setAdvancedPopoverOpen}
                  >
                    <PopoverTrigger asChild>
                      <button
                        type="button"
                        className="flex items-center gap-2 text-sm font-medium text-gray-700 hover:text-gray-900 transition-colors group"
                      >
                        Advanced Configuration
                        <ChevronDown
                          className={`w-4 h-4 transition-transform group-hover:translate-y-px ${advancedPopoverOpen ? "rotate-180" : ""}`}
                        />
                      </button>
                    </PopoverTrigger>
                    <PopoverContent className="w-96 p-4" align="start">
                      <div className="space-y-4">
                        <FormField
                          control={form.control}
                          name="authHeaderName"
                          render={({ field }) => (
                            <FormItem>
                              <FormLabel>Authorization Header Name</FormLabel>
                              <FormControl>
                                <Input
                                  placeholder='"Authorization"'
                                  {...field}
                                />
                              </FormControl>
                              <p className="text-xs text-gray-500">
                                The HTTP header name provided with upstream
                                requests to this endpoint.
                              </p>
                            </FormItem>
                          )}
                        />

                        <FormField
                          control={form.control}
                          name="authHeaderPrefix"
                          render={({ field }) => (
                            <FormItem>
                              <FormLabel>Authorization Header Prefix</FormLabel>
                              <FormControl>
                                <Input placeholder='"Bearer "' {...field} />
                              </FormControl>
                              <p className="text-xs text-gray-500">
                                The prefix before the API key header value.
                                Default is "Bearer " (with trailing space).
                              </p>
                            </FormItem>
                          )}
                        />
                      </div>
                    </PopoverContent>
                  </Popover>

                  {/* Auto-discover models checkbox */}
                  <div className="flex items-center space-x-2">
                    <Checkbox
                      id="auto-discover-edit"
                      checked={autoDiscoverModels}
                      onCheckedChange={(checked) =>
                        setAutoDiscoverModels(checked === true)
                      }
                    />
                    <label
                      htmlFor="auto-discover-edit"
                      className="text-sm font-medium leading-none peer-disabled:cursor-not-allowed peer-disabled:opacity-70 cursor-pointer"
                    >
                      Auto-discover models
                    </label>
                    <HoverCard openDelay={200} closeDelay={100}>
                      <HoverCardTrigger asChild>
                        <button
                          type="button"
                          className="text-gray-500 hover:text-gray-700 transition-colors"
                          onFocus={(e) => e.preventDefault()}
                          tabIndex={-1}
                        >
                          <Info className="h-4 w-4" />
                          <span className="sr-only">
                            Auto-discover information
                          </span>
                        </button>
                      </HoverCardTrigger>
                      <HoverCardContent className="w-80" sideOffset={5}>
                        <p className="text-sm text-muted-foreground">
                          When enabled, the endpoint will be queried for
                          available models via the <code>/v1/models</code> API.
                          If disabled, you can manually specify the model names.
                        </p>
                      </HoverCardContent>
                    </HoverCard>
                  </div>
                </div>

                {/* Validation Error Banner */}
                {validationState === "error" && (
                  <div className="p-4 bg-red-50 border border-red-200 rounded-lg">
                    <div className="flex items-center space-x-2">
                      <AlertCircle className="w-5 h-5 text-red-600" />
                      <p className="text-red-800 font-medium">
                        Connection Failed
                      </p>
                    </div>
                    <p className="text-red-700 text-sm mt-1">
                      {validationError}
                    </p>
                  </div>
                )}
              </div>
            )}

            {/* Step 2: Configure Models */}
            {currentStep === 2 && (
              <div className="space-y-6">
                <FormField
                  control={form.control}
                  name="name"
                  render={({ field, fieldState }) => (
                    <FormItem>
                      <FormLabel>Display Name *</FormLabel>
                      <FormControl>
                        <Input
                          placeholder="My API Endpoint"
                          {...field}
                          onChange={(e) => {
                            field.onChange(e);
                            if (form.formState.errors.name) {
                              form.clearErrors("name");
                            }
                          }}
                          className={
                            fieldState.error
                              ? "border-red-500 focus:border-red-500"
                              : ""
                          }
                        />
                      </FormControl>
                      {fieldState.error?.message !==
                        "endpoint_name_conflict" && <FormMessage />}
                    </FormItem>
                  )}
                />

                <FormField
                  control={form.control}
                  name="description"
                  render={({ field }) => (
                    <FormItem>
                      <FormLabel>Description (optional)</FormLabel>
                      <FormControl>
                        <Textarea
                          placeholder="Description of this endpoint..."
                          className="resize-none"
                          rows={3}
                          {...field}
                        />
                      </FormControl>
                    </FormItem>
                  )}
                />

                {/* Success Banner */}
                {validationState === "success" &&
                  availableModels.length > 0 && (
                    <div className="p-2 bg-green-50 border border-green-200 rounded-lg">
                      <div className="flex items-center space-x-2">
                        <Check className="w-4 h-4 text-green-600" />
                        <p className="text-sm text-green-800">
                          {manualMode
                            ? `${availableModels.length} model${availableModels.length === 1 ? "" : "s"} configured`
                            : `Models refreshed • ${availableModels.length} model${availableModels.length === 1 ? "" : "s"} found`}
                        </p>
                      </div>
                    </div>
                  )}

                {/* Manual Model Entry */}
                {manualMode && validationState !== "success" && (
                  <div className="space-y-3">
                    <div>
                      <label className="text-sm font-medium">
                        Model Names (one per line)
                      </label>
                      <p className="text-xs text-gray-500 mt-1">
                        Enter the model names available on this endpoint
                      </p>
                    </div>
                    <Textarea
                      placeholder="gpt-4&#10;gpt-3.5-turbo&#10;my-custom-model"
                      value={manualModelInput}
                      onChange={(e) => setManualModelInput(e.target.value)}
                      rows={8}
                      className="font-mono text-sm"
                    />
                    <Button
                      type="button"
                      onClick={handleApplyManualModels}
                      disabled={!manualModelInput.trim()}
                      className="w-full"
                    >
                      Configure Models
                    </Button>
                  </div>
                )}

                {/* Model Selection */}
                {validationState === "success" &&
                  availableModels.length > 0 && (
                    <FormField
                      control={form.control}
                      name="selectedModels"
                      render={({ field }) => (
                        <FormItem>
                          <div className="flex items-center justify-between">
                            <div>
                              <FormLabel>
                                Select Models & Configure Aliases
                              </FormLabel>
                              <p className="text-xs text-gray-500">
                                {field.value?.length || 0} of{" "}
                                {availableModels.length} selected • Aliases
                                default to model names but can be customized
                              </p>
                            </div>
                            <div className="flex space-x-2">
                              <Button
                                type="button"
                                variant="link"
                                onClick={handleSelectAll}
                                className="h-auto p-0 text-xs"
                              >
                                {field.value?.length === availableModels.length
                                  ? "Deselect All"
                                  : "Select All"}
                              </Button>
                              <Button
                                type="button"
                                onClick={handleTestConnection}
                                disabled={
                                  !form.watch("url")?.trim() ||
                                  validateEndpointMutation.isPending
                                }
                                variant="outline"
                                size="sm"
                              >
                                {validateEndpointMutation.isPending ? (
                                  <>
                                    <Loader2 className="w-4 h-4 mr-2 animate-spin" />
                                    Refreshing...
                                  </>
                                ) : (
                                  <>
                                    <Server className="w-4 h-4 mr-2" />
                                    Refresh List
                                  </>
                                )}
                              </Button>
                            </div>
                          </div>

                          <div className="max-h-60 overflow-y-auto border rounded-lg mt-2">
                            <div className="sticky top-0 bg-gray-50 border-b px-3 py-2 text-xs font-medium text-gray-600">
                              <div className="grid grid-cols-12 gap-3">
                                <div className="col-span-1"></div>
                                <div className="col-span-5">Model Name</div>
                                <div className="col-span-6">
                                  Alias (used for routing)
                                </div>
                              </div>
                            </div>

                            {availableModels.map((model) => (
                              <ModelRowWithAlias
                                key={model.id}
                                model={model}
                                isSelected={
                                  field.value?.includes(model.id) || false
                                }
                                alias={modelAliases[model.id] || model.id}
                                onSelectionChange={(checked) => {
                                  const current = field.value || [];
                                  if (checked) {
                                    field.onChange([...current, model.id]);
                                    if (!modelAliases[model.id]) {
                                      setModelAliases((prev) => ({
                                        ...prev,
                                        [model.id]: model.id,
                                      }));
                                    }
                                  } else {
                                    field.onChange(
                                      current.filter((id) => id !== model.id),
                                    );
                                  }
                                }}
                                onAliasChange={(newAlias) => {
                                  const trimmedAlias = newAlias.trim();
                                  const updatedAliases = {
                                    ...modelAliases,
                                    [model.id]: trimmedAlias,
                                  };
                                  setModelAliases(updatedAliases);

                                  const conflicts =
                                    checkLocalAliasConflicts(updatedAliases);
                                  setLocalConflicts(conflicts);
                                }}
                                onConflictClear={(oldAlias) => {
                                  setBackendConflicts((prev) => {
                                    const updated = new Set(prev);
                                    updated.delete(oldAlias);
                                    return updated;
                                  });
                                }}
                                conflictInfo={checkAliasConflict(
                                  model.id,
                                  modelAliases[model.id] || model.id,
                                )}
                              />
                            ))}
                          </div>
                          <FormMessage />
                        </FormItem>
                      )}
                    />
                  )}

                {/* Error message displays */}
                {form.formState.errors.name?.type ===
                  "endpoint_name_conflict" && (
                  <div className="p-3 bg-red-50 border border-red-200 rounded-md">
                    <div className="flex items-center space-x-2">
                      <AlertCircle className="w-4 h-4 text-red-500 shrink-0" />
                      <p className="text-sm text-red-700">
                        <strong>Endpoint name conflict:</strong> Please choose a
                        different display name above.
                      </p>
                    </div>
                  </div>
                )}

                {localConflicts.length > 0 && (
                  <div className="p-3 bg-orange-50 border border-orange-200 rounded-md">
                    <div className="flex items-center space-x-2">
                      <AlertCircle className="w-4 h-4 text-orange-500 shrink-0" />
                      <p className="text-sm text-orange-700">
                        <strong>Duplicate aliases detected:</strong>{" "}
                        {localConflicts.join(", ")}. Please ensure all aliases
                        are unique.
                      </p>
                    </div>
                  </div>
                )}

                {backendConflicts.size > 0 && (
                  <div className="p-3 bg-red-50 border border-red-200 rounded-md">
                    <div className="flex items-center space-x-2">
                      <AlertCircle className="w-4 h-4 text-red-500 shrink-0" />
                      <p className="text-sm text-red-700">
                        <strong>Model alias conflict:</strong> Please edit the
                        highlighted aliases above.
                      </p>
                    </div>
                  </div>
                )}

                {validationError &&
                  !validationError.includes("endpoint name") &&
                  backendConflicts.size === 0 && (
                    <div className="p-4 bg-red-50 border border-red-200 rounded-lg">
                      <p className="text-red-800 text-sm">{validationError}</p>
                    </div>
                  )}
              </div>
            )}
          </form>
        </Form>

        <DialogFooter>
          {currentStep === 1 ? (
            <>
              <Button type="button" variant="outline" onClick={onClose}>
                Cancel
              </Button>
              <Button
                type="button"
                onClick={
                  autoDiscoverModels
                    ? handleTestConnection
                    : handleConfigureManually
                }
                disabled={
                  !form.watch("url")?.trim() ||
                  (autoDiscoverModels && validationState === "testing")
                }
              >
                {validationState === "testing" ? (
                  <>
                    <Loader2 className="w-4 h-4 mr-2 animate-spin" />
                    Testing Connection...
                  </>
                ) : autoDiscoverModels ? (
                  <>
                    <Server className="w-4 h-4 mr-2" />
                    Discover Models
                  </>
                ) : (
                  <>
                    Next
                    <ChevronRight className="w-4 h-4 ml-2" />
                  </>
                )}
              </Button>
            </>
          ) : (
            <>
              <Button type="button" variant="outline" onClick={handleBack}>
                Back
              </Button>
              <Button
                onClick={() => form.handleSubmit(onSubmit)()}
                disabled={!canUpdate}
              >
                {updateEndpointMutation.isPending ? (
                  <>
                    <Loader2 className="w-4 h-4 mr-2 animate-spin" />
                    Updating...
                  </>
                ) : backendConflicts.size > 0 ? (
                  <>
                    <AlertCircle className="w-4 h-4 mr-2" />
                    Resolve Conflicts
                  </>
                ) : localConflicts.length > 0 ? (
                  <>
                    <AlertCircle className="w-4 h-4 mr-2" />
                    Fix Duplicate Aliases
                  </>
                ) : (
                  <>
                    <Check className="w-4 h-4 mr-2" />
                    Update Endpoint
                  </>
                )}
              </Button>
            </>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};
