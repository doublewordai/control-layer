import React, { useState, useEffect } from "react";
import {
  Server,
  Check,
  AlertCircle,
  Loader2,
  Edit2,
  Info,
  ChevronDown,
  X,
} from "lucide-react";
import { useForm } from "react-hook-form";
import { zodResolver } from "@hookform/resolvers/zod";
import * as z from "zod";
import { useValidateEndpoint, useCreateEndpoint } from "../../../api/control-layer";
import { Button } from "../../ui/button";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
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
  },
  {
    name: "Anthropic",
    url: "https://api.anthropic.com/v1",
    icon: "/endpoints/anthropic.svg",
    apiKeyUrl: "https://console.anthropic.com/settings/keys",
    requiresApiKey: true,
  },
  {
    name: "Google",
    url: "https://generativelanguage.googleapis.com/v1beta/openai/",
    icon: "/endpoints/google.svg",
    apiKeyUrl: "https://aistudio.google.com/api-keys",
    requiresApiKey: true,
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
  const [backendConflicts, setBackendConflicts] = useState<Set<string>>(new Set());
  const [localConflicts, setLocalConflicts] = useState<string[]>([]);

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
    }
  }, [isOpen, form]);

  const checkAliasConflict = (currentModelId: string, aliasToCheck: string) => {
    // Check for conflicts within current selection
    const selectedModels = form.getValues("selectedModels") || [];
    const localConflict = Object.entries(modelAliases).some(
      ([modelId, alias]) => 
        modelId !== currentModelId && 
        alias === aliasToCheck &&
        selectedModels.includes(modelId)
    );

    // Check for backend conflicts
    const backendConflict = backendConflicts.has(aliasToCheck);

    return {
      hasConflict: localConflict || backendConflict,
      hasLocalConflict: localConflict,
      hasBackendConflict: backendConflict,
    };
  };

  const checkLocalAliasConflicts = (updatedAliases?: Record<string, string>) => {
    const selectedModels = form.getValues("selectedModels") || [];
    const aliasesToCheck = updatedAliases || modelAliases;
    const selectedAliases = selectedModels.map(id => aliasesToCheck[id] || id);
    const duplicateAliases = selectedAliases.filter((alias, index) => 
      selectedAliases.indexOf(alias) !== index
    );
    return [...new Set(duplicateAliases)]; // Remove duplicates from the conflicts array
  };

  const handleTestConnection = async () => {
    const url = form.getValues("url");
    const apiKey = form.getValues("apiKey");

    if (!url) {
      form.setError("url", { message: "Please enter a URL" });
      return;
    }

    // Clear URL errors
    form.clearErrors("url");

    setValidationState("testing");
    setValidationError(null);

    const validateData: EndpointValidateRequest = {
      type: "new",
      url: url.trim(),
      ...(apiKey?.trim() && { api_key: apiKey.trim() }),
    };

    try {
      const result = await validateEndpointMutation.mutateAsync(validateData);

      if (result.status === "success" && result.models) {
        setAvailableModels(result.models.data);
        setValidationState("success");

        // Initialize aliases to model names
        const initialAliases = result.models.data.reduce((acc, model) => ({
          ...acc,
          [model.id]: model.id
        }), {});
        setModelAliases(initialAliases);

        // Select all models by default
        form.setValue("selectedModels", result.models.data.map((m) => m.id));
        
        // Auto-populate name from URL if not set
        if (!form.getValues("name")) {
          try {
            const urlObj = new URL(url);
            form.setValue("name", urlObj.hostname);
          } catch {
            // Invalid URL, ignore
          }
        }
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
    data.selectedModels.forEach(modelId => {
      const alias = (modelAliases[modelId] || modelId).trim();
      if (alias !== modelId) {
        aliasMapping[modelId] = alias;
      }
    });

    const createData: EndpointCreateRequest = {
      name: data.name.trim(),
      url: data.url.trim(),
      ...(data.description?.trim() && { description: data.description.trim() }),
      ...(data.apiKey?.trim() && { api_key: data.apiKey.trim() }),
      model_filter: data.selectedModels, // Which models to import
      ...(Object.keys(aliasMapping).length > 0 && { alias_mapping: aliasMapping }), // Only include if we have custom aliases
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
            message: "endpoint_name_conflict" // Use a special marker instead of user message
          });
          setValidationError("Please choose a different endpoint name.");
          return; // Don't set backend conflicts
        }
        
        // Check if this is a structured alias conflict (from sync logic)
        if (responseData && responseData.conflicts) {
          const conflictAliases = responseData.conflicts.map((c: any) => c.attempted_alias || c.alias);
          setBackendConflicts(new Set(conflictAliases));
          setValidationError("Some model aliases already exist. Please edit the highlighted aliases.");
        } else {
          // Generic 409 handling
          setValidationError(responseData?.message || "A conflict occurred. Please check your input.");
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
          <Checkbox
            checked={isSelected}
            onCheckedChange={onSelectionChange}
          />
        </div>

        {/* Model Info */}
        <div className="col-span-5 flex flex-col justify-center min-w-0">
          <p className="text-sm font-medium truncate">
            {model.id}
          </p>
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
    const allModelIds = availableModels.map(m => m.id);
    
    if (currentSelection.length === availableModels.length) {
      // Deselect all
      form.setValue("selectedModels", []);
    } else {
      // Select all
      form.setValue("selectedModels", allModelIds);
      
      // Initialize aliases for any models that don't have them
      const newAliases = { ...modelAliases };
      allModelIds.forEach(modelId => {
        if (!newAliases[modelId]) {
          newAliases[modelId] = modelId;
        }
      });
      setModelAliases(newAliases);
    }
  };

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-2xl max-h-[90vh] overflow-y-auto">
        <DialogHeader>
          <div className="flex items-center space-x-3">
            <div className="p-2 bg-doubleword-accent-blue rounded-lg">
              <Server className="w-5 h-5 text-white" />
            </div>
            <DialogTitle>Add Endpoint</DialogTitle>
          </div>
        </DialogHeader>

        <Form {...form}>
          <form onSubmit={form.handleSubmit(onSubmit)} className="space-y-6">
            {/* Step 1: URL and API Key */}
            <FormField
              control={form.control}
              name="url"
              render={({ field }) => (
                <FormItem>
                  <div className="flex items-center gap-1.5">
                    <FormLabel>Endpoint URL *</FormLabel>
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
                          The base URL of your OpenAI-compatible inference
                          endpoint.
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
                        onChange={(e) => {
                          field.onChange(e);
                          // Reset validation state when URL changes
                          if (validationState === "success") {
                            setValidationState("idle");
                            setAvailableModels([]);
                            form.setValue("selectedModels", []);
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
                          <PopoverContent className="w-96 p-2" align="end">
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
                                    // Reset validation state when changing URL
                                    if (validationState === "success") {
                                      setValidationState("idle");
                                      setAvailableModels([]);
                                      form.setValue("selectedModels", []);
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
                                      className="w-5 h-5 flex-shrink-0"
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
                  <FormMessage />
                </FormItem>
              )}
            />

            <FormField
              control={form.control}
              name="apiKey"
              render={({ field }) => {
                const currentUrl = form.watch("url");
                const matchedEndpoint = POPULAR_ENDPOINTS.find(
                  (ep) => currentUrl && currentUrl.trim() === ep.url,
                );

                return (
                  <FormItem>
                    <FormLabel>
                      API Key{" "}
                      {matchedEndpoint?.requiresApiKey ? "*" : "(optional)"}
                    </FormLabel>
                    <FormControl>
                      <Input type="password" placeholder="sk-..." {...field} />
                    </FormControl>
                    <FormDescription>
                      {matchedEndpoint ? (
                        <>
                          Manage your {matchedEndpoint.name} keys{" "}
                          <a
                            href={matchedEndpoint.apiKeyUrl}
                            target="_blank"
                            rel="noopener noreferrer"
                            className="text-blue-600 hover:text-blue-700 underline"
                          >
                            here
                          </a>
                        </>
                      ) : (
                        "Add an API key if the endpoint requires authentication"
                      )}
                    </FormDescription>
                  </FormItem>
                );
              }}
            />

            {/* Validation Error Banner */}
            {validationState === "error" && (
              <div className="p-4 bg-red-50 border border-red-200 rounded-lg">
                <div className="flex items-center space-x-2">
                  <AlertCircle className="w-5 h-5 text-red-600" />
                  <p className="text-red-800 font-medium">Connection Failed</p>
                </div>
                <p className="text-red-700 text-sm mt-1">{validationError}</p>
              </div>
            )}

            {validationState === "success" && (
              <>
                <div className="p-2 bg-green-50 border border-green-200 rounded-lg">
                  <div className="flex items-center space-x-2">
                    <Check className="w-4 h-4 text-green-600" />
                    <p className="text-sm text-green-800">
                      Connected successfully • {availableModels.length} models
                      found
                    </p>
                  </div>
                </div>

                {/* Step 2: Model Selection */}
                {availableModels.length > 0 && (
                  <FormField
                    control={form.control}
                    name="selectedModels"
                    render={({ field }) => (
                      <FormItem>
                        <div className="flex items-center justify-between">
                          <div>
                            <FormLabel>Select Models & Configure Aliases</FormLabel>
                            <FormDescription className="text-xs">
                              {field.value?.length || 0} of {availableModels.length} selected
                              • Aliases default to model names but can be customized
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

                        <div className="max-h-60 overflow-y-auto border rounded-lg mt-2">
                          <div className="sticky top-0 bg-gray-50 border-b px-3 py-2 text-xs font-medium text-gray-600">
                            <div className="grid grid-cols-12 gap-3">
                              <div className="col-span-1"></div> {/* Checkbox column */}
                              <div className="col-span-5">Model Name</div>
                              <div className="col-span-6">Alias (used for routing)</div>
                            </div>
                          </div>

                          {availableModels.map((model) => (
                            <ModelRowWithAlias
                              key={model.id}
                              model={model}
                              isSelected={field.value?.includes(model.id) || false}
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
                                  field.onChange(current.filter((id) => id !== model.id));
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
                                const conflicts = checkLocalAliasConflicts(updatedAliases);
                                setLocalConflicts(conflicts);
                              }}
                              onConflictClear={(oldAlias) => {
                                setBackendConflicts((prev) => {
                                  const updated = new Set(prev);
                                  updated.delete(oldAlias);
                                  return updated;
                                });
                              }}
                              conflictInfo={checkAliasConflict(model.id, modelAliases[model.id] || model.id)}
                            />
                          ))}
                        </div>
                        <FormMessage />
                      </FormItem>
                    )}
                  />
                )}

                {/* Step 3: Endpoint Details */}
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
                          className={fieldState.error ? "border-red-500 focus:border-red-500" : ""}
                        />
                      </FormControl>
                      {fieldState.error?.message !== "endpoint_name_conflict" && <FormMessage />}
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
              </>
            )}

            {(form.formState.errors.name?.message === "endpoint_name_conflict" || validationError?.includes("endpoint name")) && (
              <div className="p-3 bg-red-50 border border-red-200 rounded-md">
                <div className="flex items-center space-x-2">
                  <AlertCircle className="w-4 h-4 text-red-500 flex-shrink-0" />
                  <p className="text-sm text-red-700">
                    <strong>Endpoint name conflict:</strong> Please choose a different display name above.
                  </p>
                </div>
              </div>
            )}

            {localConflicts.length > 0 && (
              <div className="p-3 bg-orange-50 border border-orange-200 rounded-md">
                <div className="flex items-center space-x-2">
                  <AlertCircle className="w-4 h-4 text-orange-500 flex-shrink-0" />
                  <p className="text-sm text-orange-700">
                    <strong>Duplicate aliases detected:</strong> {localConflicts.join(', ')}. Please ensure all aliases are unique.
                  </p>
                </div>
              </div>
            )}

            {backendConflicts.size > 0 && (
              <div className="p-3 bg-red-50 border border-red-200 rounded-md">
                <div className="flex items-center space-x-2">
                  <AlertCircle className="w-4 h-4 text-red-500 flex-shrink-0" />
                  <p className="text-sm text-red-700">
                    <strong>Model alias conflict:</strong> Please edit the highlighted aliases above.
                  </p>
                </div>
              </div>
            )}
          </form>
        </Form>

        <DialogFooter>
          <Button type="button" variant="outline" onClick={onClose}>
            Cancel
          </Button>
          {validationState !== "success" ? (
            <Button
              type="button"
              onClick={handleTestConnection}
              disabled={
                !form.watch("url") ||
                validationState === "testing" ||
                validateEndpointMutation.isPending
              }
            >
              {validationState === "testing" ? (
                <>
                  <Loader2 className="w-4 h-4 mr-2 animate-spin" />
                  Testing Connection...
                </>
              ) : (
                <>
                  <Server className="w-4 h-4 mr-2" />
                  Test Connection
                </>
              )}
            </Button>
          ) : (
            <Button
              onClick={() => form.handleSubmit(onSubmit)()}
              disabled={
                createEndpointMutation.isPending ||
                !form.watch("name") ||
                !form.watch("selectedModels")?.length ||
                backendConflicts.size > 0 ||
                localConflicts.length > 0
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
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};
