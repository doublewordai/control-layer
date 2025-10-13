import React, { useState, useEffect } from "react";
import { Server, Check, AlertCircle, Loader2, Edit2, X } from "lucide-react";
import { useForm } from "react-hook-form";
import { zodResolver } from "@hookform/resolvers/zod";
import * as z from "zod";
import { useValidateEndpoint, useCreateEndpoint } from "../../../api/clay";
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
import type {
  EndpointValidateRequest,
  AvailableModel,
  EndpointCreateRequest,
} from "../../../api/clay/types";

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
    }
  }, [isOpen, form]);

  // Add state to track backend conflicts
  const [backendConflicts, setBackendConflicts] = useState<Set<string>>(new Set());

  // Update reset effect to clear backend conflicts
  useEffect(() => {
    if (isOpen) {
      form.reset();
      setValidationState("idle");
      setValidationError(null);
      setAvailableModels([]);
      setModelAliases({});
      setBackendConflicts(new Set()); // Clear backend conflicts
    }
  }, [isOpen, form]);

  // Function to check for alias conflicts
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

    // Check for local alias conflicts among selected models
    const selectedModels = data.selectedModels;
    const selectedAliases = selectedModels.map(id => modelAliases[id] || id);
    const duplicateAliases = selectedAliases.filter((alias, index) => 
      selectedAliases.indexOf(alias) !== index
    );

    if (duplicateAliases.length > 0) {
      setValidationError(
        `Duplicate aliases detected: ${duplicateAliases.join(', ')}. Please ensure all aliases are unique.`
      );
      return;
    }

    // Build alias mapping - only include entries where alias differs from model name
    const aliasMapping: Record<string, string> = {};
    selectedModels.forEach(modelId => {
      const alias = modelAliases[modelId] || modelId;
      if (alias !== modelId) {
        aliasMapping[modelId] = alias;
      }
    });

    const createData: EndpointCreateRequest = {
      name: data.name.trim(),
      url: data.url.trim(),
      ...(data.description?.trim() && { description: data.description.trim() }),
      ...(data.apiKey?.trim() && { api_key: data.apiKey.trim() }),
      model_filter: selectedModels, // Which models to import
      ...(Object.keys(aliasMapping).length > 0 && { alias_mapping: aliasMapping }), // Only include if we have custom aliases
    };

    try {
      await createEndpointMutation.mutateAsync(createData);
      onSuccess();
      onClose();
    } catch (err: any) {
      
      // Handle structured conflict errors from our new backend response
      if (err.isConflict && err.conflicts && err.conflicts.length > 0) {
        const conflictAliases = err.conflicts.map((c: any) => c.attempted_alias);
        setBackendConflicts(new Set(conflictAliases));
        
        const firstConflict = err.conflicts[0];
        setValidationError(
          `Alias "${firstConflict.attempted_alias}" already exists. Please choose a different alias.`
        );
      } else if (err.message && err.message.includes("Alias conflicts detected")) {
        const conflictAliases = parseConflictMessage(err.message);
        setBackendConflicts(new Set(conflictAliases));
        setValidationError(
          `Some aliases already exist. Please edit the highlighted aliases.`
        );
      } else {
        const errorMessage = err.message || "Failed to create endpoint";
        setValidationError(errorMessage);
      }
    }
  };

  // Helper function to parse conflict message from backend
  const parseConflictMessage = (message: string): string[] => {
    // Expected format: "Alias conflicts detected for models: model1 → alias1, model2 → alias2. These aliases already exist in the system."
    const match = message.match(/Alias conflicts detected for models: (.+?)\./);
    if (!match) return [];

    const conflictPairs = match[1].split(', ');
    const conflictAliases: string[] = [];

    conflictPairs.forEach(pair => {
      // Extract alias from "modelName → aliasName" format
      const aliasMatch = pair.match(/(.+?) → (.+)/);
      if (aliasMatch) {
        conflictAliases.push(aliasMatch[2].trim());
      }
    });

    return conflictAliases;
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
                {hasConflict && (
                  <div className="flex items-center space-x-1">
                    <AlertCircle className={`w-3 h-3 flex-shrink-0 ${
                      hasBackendConflict ? "text-red-500" : "text-orange-500"
                    }`} />
                    <span className="text-xs text-gray-500">
                      {hasBackendConflict ? "already exists" : "duplicate"}
                    </span>
                  </div>
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

  // Add the missing handleSelectAll function
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
                  <FormLabel>Endpoint URL *</FormLabel>
                  <FormControl>
                    <Input
                      placeholder="https://api.example.com"
                      {...field}
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
                  </FormControl>
                  <FormMessage />
                </FormItem>
              )}
            />

            <FormField
              control={form.control}
              name="apiKey"
              render={({ field }) => (
                <FormItem>
                  <FormLabel>API Key (optional)</FormLabel>
                  <FormControl>
                    <Input type="password" placeholder="sk-..." {...field} />
                  </FormControl>
                  <FormDescription>
                    Add an API key if the endpoint requires authentication
                  </FormDescription>
                </FormItem>
              )}
            />

            {/* Subtle Conflict Banner */}
            {backendConflicts.size > 0 && (
              <div className="p-3 bg-red-50 border border-red-200 rounded-md">
                <div className="flex items-center space-x-2">
                  <AlertCircle className="w-4 h-4 text-red-500 flex-shrink-0" />
                  <p className="text-sm text-red-700">
                    Alias clash detected. Please edit the highlighted alias below.
                  </p>
                </div>
              </div>
            )}

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
                                setModelAliases((prev) => ({
                                  ...prev,
                                  [model.id]: trimmedAlias,
                                }));
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
                  render={({ field }) => (
                    <FormItem>
                      <FormLabel>Display Name *</FormLabel>
                      <FormControl>
                        <Input placeholder="My API Endpoint" {...field} />
                      </FormControl>
                      <FormMessage />
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
                backendConflicts.size > 0
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
