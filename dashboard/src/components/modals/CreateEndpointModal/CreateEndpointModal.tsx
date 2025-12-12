import React, { useState, useEffect } from "react";
import {
  Check,
  AlertCircle,
  Loader2,
  Edit2,
  Info,
  ChevronDown,
  ChevronRight,
  Eye,
  EyeOff,
  X,
  RefreshCw,
} from "lucide-react";
import { useForm } from "react-hook-form";
import { zodResolver } from "@hookform/resolvers/zod";
import * as z from "zod";
import {
  useValidateEndpoint,
  useCreateEndpoint,
} from "../../../api/control-layer";
import { Button } from "../../ui/button";
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
  FormDescription,
  FormField,
  FormItem,
  FormLabel,
  FormMessage,
} from "../../ui/form";
import { Input } from "../../ui/input";
import { Textarea } from "../../ui/textarea";
import { Checkbox } from "../../ui/checkbox";
import {
  HoverCard,
  HoverCardContent,
  HoverCardTrigger,
} from "../../ui/hover-card";
import { Popover, PopoverContent, PopoverTrigger } from "../../ui/popover";
import type {
  EndpointValidateRequest,
  AvailableModel,
  EndpointCreateRequest,
} from "../../../api/control-layer/types";

interface CreateEndpointModalProps {
  isOpen: boolean;
  onClose: () => void;
  onSuccess: () => void;
}

type ValidationState = "idle" | "testing" | "success" | "error";

// Define the form schema
const formSchema = z.object({
  url: z.string().min(1, "URL is required").url("Please enter a valid URL"),
  apiKey: z.string().optional(),
  name: z.string().min(1, "Endpoint name is required"),
  description: z.string().optional(),
  selectedModels: z.array(z.string()).min(1, "Select at least one model"),
  authHeaderName: z.string().optional(),
  authHeaderPrefix: z.string().optional(),
});

type FormData = z.infer<typeof formSchema>;

// Popular endpoint presets
const POPULAR_ENDPOINTS = [
  {
    name: "OpenAI",
    url: "https://api.openai.com/v1",
    icon: "/endpoints/openai.svg",
    apiKeyUrl: "https://platform.openai.com/api-keys",
    requiresApiKey: true,
    apiKeyInstructions: () => (
      <>
        Manage your OpenAI keys{" "}
        <a
          href="https://platform.openai.com/api-keys"
          target="_blank"
          rel="noopener noreferrer"
          className="text-blue-600 hover:text-blue-700 underline"
        >
          here
        </a>
      </>
    ),
  },
  {
    name: "Anthropic",
    url: "https://api.anthropic.com/v1",
    icon: "/endpoints/anthropic.svg",
    apiKeyUrl: "https://console.anthropic.com/settings/keys",
    requiresApiKey: true,
    apiKeyInstructions: () => (
      <>
        Manage your Anthropic keys{" "}
        <a
          href="https://console.anthropic.com/settings/keys"
          target="_blank"
          rel="noopener noreferrer"
          className="text-blue-600 hover:text-blue-700 underline"
        >
          here
        </a>
      </>
    ),
  },
  {
    name: "Google",
    url: "https://generativelanguage.googleapis.com/v1beta/openai/",
    icon: "/endpoints/google.svg",
    apiKeyUrl: "https://aistudio.google.com/api-keys",
    requiresApiKey: true,
    apiKeyInstructions: () => (
      <>
        Manage your Google keys{" "}
        <a
          href="https://aistudio.google.com/api-keys"
          target="_blank"
          rel="noopener noreferrer"
          className="text-blue-600 hover:text-blue-700 underline"
        >
          here
        </a>
      </>
    ),
  },
  {
    name: "Snowflake SPCS Endpoint",
    url: "https://<your-endpoint-identifier>.snowflakecomputing.app/v1",
    domain: "snowflakecomputing.app",
    icon: "/endpoints/snowflake.png",
    apiKeyUrl:
      "https://docs.snowflake.com/en/user-guide/programmatic-access-tokens",
    requiresApiKey: true,
    authHeaderPrefix: "Snowflake Token=",
    quoteApiKey: true,
    endpointInstructions: () => (
      <>
        Replace{" "}
        <code className="px-1 py-0.5 bg-gray-100 rounded text-xs">
          &lt;your-endpoint-identifier&gt;
        </code>{" "}
        with your Snowflake endpoint identifier.
      </>
    ),
    apiKeyInstructions: () => (
      <>
        Provide a Snowflake PAT. See{" "}
        <a
          href="https://docs.snowflake.com/en/user-guide/programmatic-access-tokens"
          target="_blank"
          rel="noopener noreferrer"
          className="text-blue-600 hover:text-blue-700 underline"
        >
          https://docs.snowflake.com/en/user-guide/programmatic-access-tokens
        </a>{" "}
        for instructions.
      </>
    ),
  },
  {
    name: "Snowflake Cortex AI",
    url: "https://<account-identifier>.snowflakecomputing.com/api/v2/cortex/v1",
    domain: "snowflakecomputing.com",
    icon: "/endpoints/snowflake.png",
    apiKeyUrl:
      "https://docs.snowflake.com/en/user-guide/programmatic-access-tokens",
    requiresApiKey: true,
    skipValidation: true,
    knownModels: [
      {
        id: "claude-4-sonnet",
        created: 0,
        object: "model",
        owned_by: "anthropic",
      },
      {
        id: "claude-4-opus",
        created: 0,
        object: "model",
        owned_by: "anthropic",
      },
      {
        id: "claude-3-7-sonnet",
        created: 0,
        object: "model",
        owned_by: "anthropic",
      },
      {
        id: "claude-3-5-sonnet",
        created: 0,
        object: "model",
        owned_by: "anthropic",
      },
      { id: "openai-gpt-4.1", created: 0, object: "model", owned_by: "openai" },
      {
        id: "openai-gpt-5-chat",
        created: 0,
        object: "model",
        owned_by: "openai",
      },
      { id: "llama4-maverick", created: 0, object: "model", owned_by: "meta" },
      { id: "llama3.1-8b", created: 0, object: "model", owned_by: "meta" },
      { id: "llama3.1-70b", created: 0, object: "model", owned_by: "meta" },
      { id: "llama3.1-405b", created: 0, object: "model", owned_by: "meta" },
      { id: "deepseek-r1", created: 0, object: "model", owned_by: "deepseek" },
      { id: "mistral-7b", created: 0, object: "model", owned_by: "mistralai" },
      {
        id: "mistral-large",
        created: 0,
        object: "model",
        owned_by: "mistralai",
      },
      {
        id: "mistral-large2",
        created: 0,
        object: "model",
        owned_by: "mistralai",
      },
      {
        id: "snowflake-llama-3.3-70b",
        created: 0,
        object: "model",
        owned_by: "snowflake",
      },
    ],
    endpointInstructions: () => (
      <>
        Replace{" "}
        <code className="px-1 py-0.5 bg-gray-100 rounded text-xs">
          &lt;account-identifier&gt;
        </code>{" "}
        with your Snowflake account identifier.
      </>
    ),
    apiKeyInstructions: () => (
      <>
        Provide a Snowflake PAT. See{" "}
        <a
          href="https://docs.snowflake.com/en/user-guide/programmatic-access-tokens"
          target="_blank"
          rel="noopener noreferrer"
          className="text-blue-600 hover:text-blue-700 underline"
        >
          https://docs.snowflake.com/en/user-guide/programmatic-access-tokens
        </a>{" "}
        for instructions.
      </>
    ),
  },
];

export const CreateEndpointModal: React.FC<CreateEndpointModalProps> = ({
  isOpen,
  onClose,
  onSuccess,
}) => {
  // Validation state
  const [validationState, setValidationState] =
    useState<ValidationState>("idle");
  const [validationError, setValidationError] = useState<string | null>(null);
  const [availableModels, setAvailableModels] = useState<AvailableModel[]>([]);
  const [modelAliases, setModelAliases] = useState<Record<string, string>>({});
  const [urlPopoverOpen, setUrlPopoverOpen] = useState(false);
  const [backendConflicts, setBackendConflicts] = useState<Set<string>>(
    new Set(),
  );
  const [localConflicts, setLocalConflicts] = useState<string[]>([]);
  const [advancedPopoverOpen, setAdvancedPopoverOpen] = useState(false);
  const [showApiKey, setShowApiKey] = useState(false);
  const [quoteApiKey, setQuoteApiKey] = useState(false);
  const [manualMode, setManualMode] = useState(false);
  const [manualModelInput, setManualModelInput] = useState("");
  const [currentStep, setCurrentStep] = useState<1 | 2>(1);
  const [autoDiscoverModels, setAutoDiscoverModels] = useState(true);

  const validateEndpointMutation = useValidateEndpoint();
  const createEndpointMutation = useCreateEndpoint();

  // Initialize form
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

  // Reset form when modal opens/closes
  useEffect(() => {
    if (isOpen) {
      form.reset();
      setValidationState("idle");
      setValidationError(null);
      setAvailableModels([]);
      setModelAliases({});
      setBackendConflicts(new Set());
      setLocalConflicts([]);
      setAdvancedPopoverOpen(false);
      setShowApiKey(false);
      setQuoteApiKey(false);
      setManualMode(false);
      setManualModelInput("");
      setCurrentStep(1);
      setAutoDiscoverModels(true);
    }
  }, [isOpen, form]);

  const checkAliasConflict = (currentModelId: string, aliasToCheck: string) => {
    // Check for conflicts within current selection
    const selectedModels = form.getValues("selectedModels") || [];
    const localConflict = Object.entries(modelAliases).some(
      ([modelId, alias]) =>
        modelId !== currentModelId &&
        alias === aliasToCheck &&
        selectedModels.includes(modelId),
    );

    // Check for backend conflicts
    const backendConflict = backendConflicts.has(aliasToCheck);

    return {
      hasConflict: localConflict || backendConflict,
      hasLocalConflict: localConflict,
      hasBackendConflict: backendConflict,
    };
  };

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
    return [...new Set(duplicateAliases)]; // Remove duplicates from the conflicts array
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

    // Auto-populate name from URL if not set
    const url = form.getValues("url");
    if (!form.getValues("name") && url) {
      try {
        const urlObj = new URL(url);
        form.setValue("name", urlObj.hostname);
      } catch {
        // Invalid URL, ignore
      }
    }

    setCurrentStep(2);
  };

  const handleBack = () => {
    setCurrentStep(1);
    setValidationState("idle");
    setValidationError(null);
    setBackendConflicts(new Set());
    setLocalConflicts([]);
  };

  const handleSkipDiscovery = () => {
    const url = form.getValues("url");

    if (!url) {
      form.setError("url", { message: "Please enter a URL" });
      return;
    }

    // Check if this is a known endpoint with predefined models
    const matchedEndpoint = POPULAR_ENDPOINTS.find((ep) => {
      if (!url) return false;
      if (ep.domain) {
        return url.includes(ep.domain);
      }
      return url.trim() === ep.url;
    });

    // Pre-populate with known models if available
    if (matchedEndpoint?.knownModels) {
      const modelNames = matchedEndpoint.knownModels
        .map((m) => m.id)
        .join("\n");
      setManualModelInput(modelNames);
    }

    setManualMode(true);
    setCurrentStep(2);
    setValidationState("idle");
    setValidationError(null);
  };

  const handleTestConnection = async () => {
    const url = form.getValues("url");
    const apiKey = form.getValues("apiKey");
    const authHeaderName = form.getValues("authHeaderName");
    const authHeaderPrefix = form.getValues("authHeaderPrefix");

    if (!url) {
      form.setError("url", { message: "Please enter a URL" });
      return;
    }

    // Clear URL errors
    form.clearErrors("url");

    setValidationState("testing");
    setValidationError(null);

    // Check if this endpoint has skipValidation and knownModels
    const matchedEndpoint = POPULAR_ENDPOINTS.find((ep) => {
      if (!url) return false;
      if (ep.domain) {
        return url.includes(ep.domain);
      }
      return url.trim() === ep.url;
    });

    if (matchedEndpoint?.skipValidation && matchedEndpoint?.knownModels) {
      // Use known models without validation
      console.log("Using known models for", matchedEndpoint.name);

      setAvailableModels(matchedEndpoint.knownModels as AvailableModel[]);
      setValidationState("success");
      setManualMode(false);

      // Initialize aliases to model names
      const initialAliases = matchedEndpoint.knownModels.reduce(
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
        matchedEndpoint.knownModels.map((m) => m.id),
      );

      // Auto-populate name from URL if not set
      if (!form.getValues("name")) {
        try {
          const urlObj = new URL(url);
          form.setValue("name", urlObj.hostname);
        } catch {
          // Invalid URL, ignore
        }
      }

      setCurrentStep(2);
      return;
    }

    // Apply quote wrapping if enabled
    const processedApiKey = apiKey?.trim()
      ? quoteApiKey
        ? `"${apiKey.trim()}"`
        : apiKey.trim()
      : undefined;

    const validateData: EndpointValidateRequest = {
      type: "new",
      url: url.trim(),
      ...(processedApiKey && { api_key: processedApiKey }),
      ...(authHeaderName?.trim() && {
        auth_header_name: authHeaderName.trim(),
      }),
      ...(authHeaderPrefix?.trim() && {
        auth_header_prefix: authHeaderPrefix.trim(),
      }),
    };

    console.log("Validating endpoint with config:", {
      url: url.trim(),
      hasApiKey: !!apiKey?.trim(),
      quoteApiKey,
      authHeaderName: authHeaderName?.trim() || "Authorization (default)",
      authHeaderPrefix: authHeaderPrefix?.trim() || "Bearer  (default)",
      processedApiKey: processedApiKey || "(none)",
    });

    try {
      const result = await validateEndpointMutation.mutateAsync(validateData);

      if (result.status === "success" && result.models) {
        setAvailableModels(result.models.data);
        setValidationState("success");
        setManualMode(false);

        // Initialize aliases to model names
        const initialAliases = result.models.data.reduce(
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
          result.models.data.map((m) => m.id),
        );

        // Auto-populate name from URL if not set
        if (!form.getValues("name")) {
          try {
            const urlObj = new URL(url);
            form.setValue("name", urlObj.hostname);
          } catch {
            // Invalid URL, ignore
          }
        }

        setCurrentStep(2);
      } else {
        setValidationError(result.error || "Unknown validation error");
        setValidationState("error");
      }
    } catch (err) {
      setValidationError(
        err instanceof Error ? err.message : "Failed to validate endpoint",
      );
      setValidationState("error");
    }
  };

  const onSubmit = async (data: FormData) => {
    if (validationState !== "success") {
      setValidationError("Please test the endpoint connection first");
      return;
    }

    // Clear previous backend conflicts
    setBackendConflicts(new Set());

    // Build alias mapping - only include entries where alias differs from model name
    const aliasMapping: Record<string, string> = {};
    data.selectedModels.forEach((modelId) => {
      const alias = (modelAliases[modelId] || modelId).trim();
      if (alias !== modelId) {
        aliasMapping[modelId] = alias;
      }
    });

    // Apply quote wrapping if enabled
    const processedApiKey = data.apiKey?.trim()
      ? quoteApiKey
        ? `"${data.apiKey.trim()}"`
        : data.apiKey.trim()
      : undefined;

    // Check if this endpoint uses skipValidation (e.g., Snowflake Cortex AI)
    const matchedEndpoint = POPULAR_ENDPOINTS.find((ep) => {
      if (!data.url) return false;
      if (ep.domain) {
        return data.url.includes(ep.domain);
      }
      return data.url.trim() === ep.url;
    });

    const createData: EndpointCreateRequest = {
      name: data.name.trim(),
      url: data.url.trim(),
      ...(data.description?.trim() && { description: data.description.trim() }),
      ...(processedApiKey && { api_key: processedApiKey }),
      model_filter: data.selectedModels, // Which models to import
      ...(Object.keys(aliasMapping).length > 0 && {
        alias_mapping: aliasMapping,
      }), // Only include if we have custom aliases
      ...(data.authHeaderName?.trim() && {
        auth_header_name: data.authHeaderName.trim(),
      }),
      ...(data.authHeaderPrefix?.trim() && {
        auth_header_prefix: data.authHeaderPrefix.trim(),
      }),
      // Use skip_fetch for manual mode OR known skipValidation endpoints
      ...((manualMode || matchedEndpoint?.skipValidation) && {
        skip_fetch: true,
      }),
    };

    try {
      await createEndpointMutation.mutateAsync(createData);
      onSuccess();
      onClose();
    } catch (err: any) {
      // Handle different types of conflicts
      if (err.status === 409 || err.response?.status === 409) {
        const responseData = err.response?.data || err.data;

        // Check if this is an endpoint conflict using the simplified response
        if (responseData?.resource === "endpoint") {
          // Set error on the name field specifically (for red border only)
          form.setError("name", {
            message: "endpoint_name_conflict", // Use a special marker instead of user message
          });
          setValidationError("Please choose a different endpoint name.");
          return; // Don't set backend conflicts
        }

        // Check if this is a structured alias conflict (from sync logic)
        if (responseData && responseData.conflicts) {
          const conflictAliases = responseData.conflicts.map(
            (c: any) => c.attempted_alias || c.alias,
          );
          setBackendConflicts(new Set(conflictAliases));
          setValidationError(
            "Some model aliases already exist. Please edit the highlighted aliases.",
          );
        } else {
          // Generic 409 handling
          setValidationError(
            responseData?.message ||
              "A conflict occurred. Please check your input.",
          );
        }
      } else {
        // Handle other errors
        const errorMessage = err.message || "Failed to create endpoint";
        setValidationError(errorMessage);
      }
    }
  };

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

    // Update tempAlias when alias prop changes
    useEffect(() => {
      setTempAlias(alias);
    }, [alias]);

    const { hasConflict, hasLocalConflict, hasBackendConflict } = conflictInfo;

    const handleSaveAlias = () => {
      const newAlias = tempAlias.trim() || model.id;
      onAliasChange(newAlias);
      setIsEditing(false);

      // Clear backend conflict for this alias if user changed it
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
            ? "bg-red-50/50 border-l-4 border-l-red-400" // Much more subtle
            : hasLocalConflict
              ? "bg-orange-50/50 border-l-4 border-l-orange-400"
              : ""
        }`}
      >
        {/* Checkbox */}
        <div className="col-span-1 flex items-center">
          <Checkbox checked={isSelected} onCheckedChange={onSelectionChange} />
        </div>

        {/* Model Info */}
        <div className="col-span-5 flex flex-col justify-center min-w-0">
          <p className="text-sm font-medium truncate">{model.id}</p>
          <p className="text-xs text-gray-500 truncate">{model.owned_by}</p>
        </div>

        {/* Alias Field */}
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
                {alias.length > 35 ? (
                  <HoverCard openDelay={200} closeDelay={100}>
                    <HoverCardTrigger asChild>
                      <span
                        className={`text-sm truncate max-w-[250px] hover:opacity-70 transition-opacity cursor-default ${
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
                    </HoverCardTrigger>
                    <HoverCardContent
                      className="w-auto max-w-sm"
                      sideOffset={5}
                    >
                      <p className="text-sm break-all">{alias}</p>
                    </HoverCardContent>
                  </HoverCard>
                ) : (
                  <span
                    className={`text-sm truncate max-w-[250px] ${
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
                )}
                {alias !== model.id && !hasConflict && (
                  <span className="text-xs text-gray-400">(custom)</span>
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
      // Deselect all
      form.setValue("selectedModels", []);
    } else {
      // Select all
      form.setValue("selectedModels", allModelIds);

      // Initialize aliases for any models that don't have them
      const newAliases = { ...modelAliases };
      allModelIds.forEach((modelId) => {
        if (!newAliases[modelId]) {
          newAliases[modelId] = modelId;
        }
      });
      setModelAliases(newAliases);
    }
  };

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-2xl max-h-[90vh] flex flex-col [&>button]:hidden">
        <DialogHeader>
          <div className="flex items-center justify-between">
            <DialogTitle>Add Endpoint</DialogTitle>

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
          <DialogDescription>
            Add an endpoint to access and configure the provided models.
          </DialogDescription>
        </DialogHeader>

        <Form {...form}>
          <form
            onSubmit={form.handleSubmit(onSubmit)}
            className="flex flex-col flex-1 min-h-0"
          >
            <div className="space-y-6 overflow-y-auto flex-1 pr-1">
              {/* Step 1: Connection Details */}
              {currentStep === 1 && (
                <div className="space-y-6">
                  <FormField
                    control={form.control}
                    name="url"
                    render={({ field }) => {
                      const currentUrl = form.watch("url");
                      // Match endpoints - use domain property if available, otherwise exact URL match
                      const matchedEndpoint = POPULAR_ENDPOINTS.find((ep) => {
                        if (!currentUrl) return false;
                        // For endpoints with a domain property (like Snowflake), match by domain
                        if (ep.domain) {
                          return currentUrl.includes(ep.domain);
                        }
                        // For static URLs, match exactly
                        return currentUrl.trim() === ep.url;
                      });

                      return (
                        <FormItem>
                          <div className="flex items-center gap-1.5">
                            <FormLabel>Base URL *</FormLabel>
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
                                    Endpoint URL information
                                  </span>
                                </button>
                              </HoverCardTrigger>
                              <HoverCardContent className="w-80" sideOffset={5}>
                                <p className="text-sm text-muted-foreground">
                                  The base URL is the url that you provide when
                                  using the OpenAI client libraries. It might
                                  include a version specifier after the root
                                  domain: for example, https://api.openai.com/v1
                                </p>
                              </HoverCardContent>
                            </HoverCard>
                          </div>
                          <FormControl>
                            <div className="relative">
                              <Input
                                placeholder="https://api.example.com"
                                {...field}
                                className="pr-10"
                                type="url"
                                autoComplete="url"
                                onChange={(e) => {
                                  field.onChange(e);

                                  // Check if URL matches a known endpoint and update auto-discover
                                  const url = e.target.value;
                                  const matchedEndpoint =
                                    POPULAR_ENDPOINTS.find((ep) => {
                                      if (!url) return false;
                                      if (ep.domain) {
                                        return url.includes(ep.domain);
                                      }
                                      return url.trim() === ep.url;
                                    });

                                  if (matchedEndpoint) {
                                    setAutoDiscoverModels(
                                      !matchedEndpoint.skipValidation,
                                    );
                                  } else {
                                    // Default to true for unknown endpoints
                                    setAutoDiscoverModels(true);
                                  }

                                  // Reset validation state when URL changes
                                  if (validationState === "success") {
                                    setValidationState("idle");
                                    setAvailableModels([]);
                                    form.setValue("selectedModels", []);
                                    setBackendConflicts(new Set());
                                    setLocalConflicts([]);
                                  }
                                  if (validationError) {
                                    setValidationError(null);
                                    setValidationState("idle");
                                  }
                                }}
                              />
                              {field.value ? (
                                <button
                                  type="button"
                                  className="absolute right-0 top-0 h-full px-3 text-gray-500 hover:text-gray-700 transition-colors border-l"
                                  onClick={() => {
                                    form.setValue("url", "");
                                    // Reset validation state
                                    if (validationState === "success") {
                                      setValidationState("idle");
                                      setAvailableModels([]);
                                      form.setValue("selectedModels", []);
                                      setBackendConflicts(new Set());
                                      setLocalConflicts([]);
                                    }
                                    if (validationError) {
                                      setValidationError(null);
                                      setValidationState("idle");
                                    }
                                  }}
                                >
                                  <X className="h-4 w-4" />
                                  <span className="sr-only">Clear URL</span>
                                </button>
                              ) : (
                                <Popover
                                  open={urlPopoverOpen}
                                  onOpenChange={setUrlPopoverOpen}
                                >
                                  <PopoverTrigger asChild>
                                    <button
                                      type="button"
                                      className="absolute right-0 top-0 h-full px-3 text-gray-500 hover:text-gray-700 transition-colors border-l"
                                    >
                                      <ChevronDown className="h-4 w-4" />
                                      <span className="sr-only">
                                        Select popular endpoint
                                      </span>
                                    </button>
                                  </PopoverTrigger>
                                  <PopoverContent
                                    className="w-96 p-2"
                                    align="end"
                                  >
                                    <div className="space-y-1">
                                      <p className="text-xs font-medium text-gray-600 px-2 py-1">
                                        Popular Endpoints
                                      </p>
                                      {POPULAR_ENDPOINTS.map((endpoint) => (
                                        <button
                                          key={endpoint.url}
                                          type="button"
                                          className="w-full text-left px-2 py-2 text-sm hover:bg-gray-100 rounded transition-colors cursor-pointer flex items-center gap-3"
                                          onClick={() => {
                                            form.setValue("url", endpoint.url);
                                            setUrlPopoverOpen(false);

                                            // Set auto-discover based on endpoint capabilities
                                            // Disable for endpoints that don't support model discovery (skipValidation = true)
                                            setAutoDiscoverModels(
                                              !endpoint.skipValidation,
                                            );

                                            // Set Snowflake-specific settings if this is Snowflake SPCS Endpoint
                                            if (
                                              endpoint.name ===
                                              "Snowflake SPCS Endpoint"
                                            ) {
                                              form.setValue(
                                                "authHeaderPrefix",
                                                endpoint.authHeaderPrefix || "",
                                              );
                                              setQuoteApiKey(
                                                endpoint.quoteApiKey || false,
                                              );
                                            } else {
                                              // Clear these for other endpoints
                                              form.setValue(
                                                "authHeaderPrefix",
                                                "",
                                              );
                                              setQuoteApiKey(false);
                                            }

                                            // Reset validation state when changing URL
                                            if (validationState === "success") {
                                              setValidationState("idle");
                                              setAvailableModels([]);
                                              form.setValue(
                                                "selectedModels",
                                                [],
                                              );
                                              setBackendConflicts(new Set());
                                              setLocalConflicts([]);
                                            }
                                            if (validationError) {
                                              setValidationError(null);
                                              setValidationState("idle");
                                            }
                                          }}
                                        >
                                          {endpoint.icon && (
                                            <img
                                              src={endpoint.icon}
                                              alt={`${endpoint.name} logo`}
                                              className="w-5 h-5 shrink-0"
                                            />
                                          )}
                                          <div className="flex-1 min-w-0">
                                            <div className="font-medium text-gray-900">
                                              {endpoint.name}
                                            </div>
                                            <div className="text-xs text-gray-500 font-mono truncate">
                                              {endpoint.url}
                                            </div>
                                          </div>
                                        </button>
                                      ))}
                                    </div>
                                  </PopoverContent>
                                </Popover>
                              )}
                            </div>
                          </FormControl>
                          <FormDescription>
                            {matchedEndpoint?.endpointInstructions
                              ? matchedEndpoint.endpointInstructions()
                              : "The base URL of your OpenAI-compatible inference endpoint."}
                          </FormDescription>
                          <FormMessage />
                        </FormItem>
                      );
                    }}
                  />

                  <FormField
                    control={form.control}
                    name="apiKey"
                    render={({ field }) => {
                      const currentUrl = form.watch("url");
                      // Match endpoints - use domain property if available, otherwise exact URL match
                      const matchedEndpoint = POPULAR_ENDPOINTS.find((ep) => {
                        if (!currentUrl) return false;
                        // For endpoints with a domain property (like Snowflake), match by domain
                        if (ep.domain) {
                          return currentUrl.includes(ep.domain);
                        }
                        // For static URLs, match exactly
                        return currentUrl.trim() === ep.url;
                      });

                      return (
                        <FormItem>
                          <FormLabel>
                            API Key{" "}
                            {matchedEndpoint?.requiresApiKey
                              ? "*"
                              : "(optional)"}
                          </FormLabel>
                          <FormControl>
                            <div className="relative">
                              <Input
                                type={showApiKey ? "text" : "password"}
                                autoComplete="new-password"
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
                          <FormDescription>
                            {matchedEndpoint?.apiKeyInstructions
                              ? matchedEndpoint.apiKeyInstructions()
                              : "Add an API key if the endpoint requires authentication"}
                          </FormDescription>
                        </FormItem>
                      );
                    }}
                  />

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
                                <FormDescription>
                                  The HTTP header name provided with upstream
                                  requests to this endpoint.
                                </FormDescription>
                              </FormItem>
                            )}
                          />

                          <FormField
                            control={form.control}
                            name="authHeaderPrefix"
                            render={({ field }) => (
                              <FormItem>
                                <FormLabel>
                                  Authorization Header Prefix
                                </FormLabel>
                                <FormControl>
                                  <Input placeholder='"Bearer "' {...field} />
                                </FormControl>
                                <FormDescription>
                                  The prefix before the API key header value.
                                  Default is "Bearer " (with trailing space).
                                </FormDescription>
                              </FormItem>
                            )}
                          />

                          <div className="flex items-center space-x-2 pt-2 border-t">
                            <Checkbox
                              id="quote-api-key"
                              checked={quoteApiKey}
                              onCheckedChange={(checked) =>
                                setQuoteApiKey(checked === true)
                              }
                            />
                            <label
                              htmlFor="quote-api-key"
                              className="text-sm font-medium leading-none peer-disabled:cursor-not-allowed peer-disabled:opacity-70 cursor-pointer"
                            >
                              Quote API key in header
                            </label>
                          </div>
                          <FormDescription className="-mt-2">
                            Wrap the API key in double quotes. Useful for
                            endpoints like Snowflake that require quoted tokens.
                          </FormDescription>
                        </div>
                      </PopoverContent>
                    </Popover>

                    {/* Auto-discover models checkbox */}
                    <div className="flex items-center space-x-2">
                      <Checkbox
                        id="auto-discover"
                        checked={autoDiscoverModels}
                        onCheckedChange={(checked) =>
                          setAutoDiscoverModels(checked === true)
                        }
                      />
                      <label
                        htmlFor="auto-discover"
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
                            available models via the <code>/v1/models</code>{" "}
                            API. If disabled, you can manually specify the model
                            names.
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
                  {/* Endpoint Name - Required in Step 2 */}
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
                              // Clear name error when user types
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
                              : `Connected successfully • ${availableModels.length} model${availableModels.length === 1 ? "" : "s"} found`}
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
                                <FormDescription className="text-xs">
                                  {field.value?.length || 0} of{" "}
                                  {availableModels.length} selected • Aliases
                                  default to model names but can be customized
                                </FormDescription>
                              </div>
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
                            </div>

                            <div className="max-h-60 overflow-y-auto overflow-x-hidden border rounded-lg mt-2">
                              <div className="sticky top-0 bg-gray-50 border-b px-3 py-2 text-xs font-medium text-gray-600">
                                <div className="grid grid-cols-12 gap-3">
                                  <div className="col-span-1"></div>{" "}
                                  {/* Checkbox column */}
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
                                      // Initialize alias if not set
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

                                    // Immediately check for local conflicts with the updated aliases
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
                </div>
              )}

              {(form.formState.errors.name?.message ===
                "endpoint_name_conflict" ||
                validationError?.includes("endpoint name")) && (
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
                      {localConflicts.join(", ")}. Please ensure all aliases are
                      unique.
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
            </div>
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
                    : handleSkipDiscovery
                }
                disabled={
                  !form.watch("url") ||
                  (autoDiscoverModels &&
                    (validationState === "testing" ||
                      validateEndpointMutation.isPending))
                }
              >
                {validationState === "testing" ? (
                  <>
                    <Loader2 className="w-4 h-4 mr-2 animate-spin" />
                    Testing Connection...
                  </>
                ) : autoDiscoverModels ? (
                  <>
                    <RefreshCw className="w-4 h-4 mr-2" />
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
                disabled={
                  createEndpointMutation.isPending ||
                  !form.watch("name") ||
                  !form.watch("selectedModels")?.length ||
                  backendConflicts.size > 0 ||
                  localConflicts.length > 0 ||
                  (manualMode && !manualModelInput.trim())
                }
              >
                {createEndpointMutation.isPending ? (
                  <>
                    <Loader2 className="w-4 h-4 mr-2 animate-spin" />
                    Creating Endpoint...
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
                    Create Endpoint
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
