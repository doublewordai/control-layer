import React, { useState, useEffect } from "react";
import { Server, Check, AlertCircle, Loader2, Edit2, X } from "lucide-react";
import { useForm } from "react-hook-form";
import { zodResolver } from "@hookform/resolvers/zod";
import * as z from "zod";
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
  FormField,
  FormItem,
  FormLabel,
  FormMessage,
} from "../../ui/form";
import { Input } from "../../ui/input";
import { Textarea } from "../../ui/textarea";
import { Checkbox } from "../../ui/checkbox";
import { Button } from "../../ui/button";
import { useValidateEndpoint, useUpdateEndpoint, waycastApi } from "../../../api/waycast";
import type {
  EndpointValidateRequest,
  AvailableModel,
  EndpointUpdateRequest,
  Endpoint,
} from "../../../api/waycast/types";

interface EditEndpointModalProps {
  isOpen: boolean;
  onClose: () => void;
  onSuccess: () => void;
  endpoint: Endpoint;
}

type ValidationState = "idle" | "testing" | "success" | "error";

// Form schema
const formSchema = z.object({
  url: z.string().min(1, "URL is required").url("Please enter a valid URL"),
  apiKey: z.string().optional(),
  name: z.string().min(1, "Endpoint name is required"),
  description: z.string().optional(),
  selectedModels: z.array(z.string()),
});

type FormData = z.infer<typeof formSchema>;

export const EditEndpointModal: React.FC<EditEndpointModalProps> = ({
  isOpen,
  onClose,
  onSuccess,
  endpoint,
}) => {
  // Validation state
  const [validationState, setValidationState] = useState<ValidationState>("idle");
  const [validationError, setValidationError] = useState<string | null>(null);
  const [availableModels, setAvailableModels] = useState<AvailableModel[]>([]);
  const [modelAliases, setModelAliases] = useState<Record<string, string>>({});
  
  // Conflict tracking
  const [backendConflicts, setBackendConflicts] = useState<Set<string>>(new Set());
  const [localConflicts, setLocalConflicts] = useState<string[]>([]);
  
  // Track if URL has changed to require re-validation
  const [urlChanged, setUrlChanged] = useState(false);

  const validateEndpointMutation = useValidateEndpoint();
  const updateEndpointMutation = useUpdateEndpoint();

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

  // Initialize form with endpoint data when modal opens
  useEffect(() => {
    if (isOpen && endpoint) {
      form.reset({
        url: endpoint.url,
        apiKey: "",
        name: endpoint.name,
        description: endpoint.description || "",
        selectedModels: endpoint.model_filter || [],
      });
      
      setValidationState("idle");
      setValidationError(null);
      setAvailableModels([]);
      setModelAliases({});
      setBackendConflicts(new Set());
      setLocalConflicts([]);
      setUrlChanged(false);

      // Fetch current deployments to get actual aliases
      fetchCurrentDeployments();
    }
  }, [isOpen, endpoint, form]);

  // Function to fetch current deployments and set aliases
  const fetchCurrentDeployments = async () => {
    try {
      const currentModels = await waycastApi.models.listByEndpoint(endpoint.id);
      
      // Build current alias mapping from deployed models
      const currentAliases: Record<string, string> = {};
      currentModels.forEach(model => {
        // The model object has both model_name and alias
        currentAliases[model.model_name] = model.alias;
      });
      
      setModelAliases(currentAliases);
      
      console.log('Current deployments loaded:', {
        models: currentModels,
        aliases: currentAliases
      });
    } catch (error) {
      console.error('Failed to fetch current deployments:', error);
      // If we can't fetch current deployments, we'll fall back to the validation flow
      // This is fine - new models will just default to model name as alias
    }
  };

  // Function to check for local alias conflicts among selected models
  const checkLocalAliasConflicts = (updatedAliases?: Record<string, string>) => {
    const selectedModels = form.getValues("selectedModels") || [];
    const aliasesToCheck = updatedAliases || modelAliases;
    const selectedAliases = selectedModels.map(id => aliasesToCheck[id] || id);
    const duplicateAliases = selectedAliases.filter((alias, index) => 
      selectedAliases.indexOf(alias) !== index
    );
    return [...new Set(duplicateAliases)];
  };

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
    
    if (!url.trim()) {
      form.setError("url", { message: "Please enter a URL" });
      return;
    }

    // Clear URL errors
    form.clearErrors("url");

    setValidationState("testing");
    setValidationError(null);

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

        // Merge existing aliases with new models from the endpoint
        const updatedAliases = { ...modelAliases }; // Start with existing aliases
        
        result.models.data.forEach(model => {
          // Only set default alias if we don't already have one for this model
          if (!updatedAliases[model.id]) {
            updatedAliases[model.id] = model.id; // Default to model name
          }
        });
        
        setModelAliases(updatedAliases);

        // Update form selection based on current deployments and endpoint filter
        const currentSelection = form.getValues("selectedModels") || [];
        if (currentSelection.length === 0) {
          if (endpoint.model_filter) {
            // Use the endpoint's model filter as the selection
            form.setValue("selectedModels", endpoint.model_filter);
          } else {
            // If no filter and no current selection, don't auto-select anything
            // Let user choose which models they want
          }
        }
      } else {
        setValidationError(result.error || "Unknown validation error");
        setValidationState("error");
      }
    } catch (err) {
      setValidationError(
        err instanceof Error ? err.message : "Failed to test connection"
      );
      setValidationState("error");
    }
  };

  const handleUrlChange = (newUrl: string) => {
    const isChanged = newUrl.trim() !== endpoint.url;
    setUrlChanged(isChanged);

    if (isChanged && validationState === "success") {
      setValidationState("idle");
      setAvailableModels([]);
      form.setValue("selectedModels", []);
      setModelAliases({});
      setBackendConflicts(new Set());
      setLocalConflicts([]);
    }

    // Clear any validation errors
    if (validationError) {
      setValidationError(null);
      if (!isChanged) {
        setValidationState("success");
      } else {
        setValidationState("idle");
      }
    }
  };

  const onSubmit = async (data: FormData) => {
    if (urlChanged && validationState !== "success") {
      setValidationError("Please test the endpoint connection after changing the URL");
      return;
    }

    // Clear previous backend conflicts
    setBackendConflicts(new Set());

    // Build alias mapping - include ALL selected models with their aliases
    const aliasMapping: Record<string, string> = {};
    data.selectedModels.forEach(modelId => {
      const alias = modelAliases[modelId] || modelId;
      // Always include the mapping, even if alias equals model name
      // The backend will handle setting the alias column appropriately
      aliasMapping[modelId] = alias.trim();
    });

    console.log('Sending alias mapping:', aliasMapping); // Debug log

    const updateData: EndpointUpdateRequest = {
      name: data.name.trim(),
      url: data.url.trim(),
      ...(data.description?.trim() && { description: data.description.trim() }),
      ...(data.apiKey?.trim() && { api_key: data.apiKey.trim() }),
      ...(data.selectedModels.length > 0 && { model_filter: data.selectedModels }),
      // Always include alias mapping when we have selected models
      ...(data.selectedModels.length > 0 && { alias_mapping: aliasMapping }),
    };

    console.log('Update payload:', updateData); // Debug log

    try {
      await updateEndpointMutation.mutateAsync({
        id: endpoint.id.toString(),
        data: updateData,
      });
      onSuccess();
      onClose();
    } catch (err: any) {
      console.log('Full error object:', err);
      
      // Handle different types of conflicts (same as create logic)
      if (err.status === 409 || err.response?.status === 409) {
        const responseData = err.response?.data || err.data;
        console.log('409 response data:', responseData);
        
        // Check if this is an endpoint conflict
        if (responseData?.resource === "endpoint") {
          form.setError("name", { 
            message: "endpoint_name_conflict"
          });
          setValidationError("Please choose a different endpoint name.");
          return;
        }
        
        // Check if this is a structured alias conflict
        if (responseData && responseData.conflicts) {
          const conflictAliases = responseData.conflicts.map((c: any) => c.attempted_alias || c.alias);
          setBackendConflicts(new Set(conflictAliases));
          setValidationError("Some model aliases already exist. Please edit the highlighted aliases.");
        } else {
          setValidationError(responseData?.message || "A conflict occurred. Please check your input.");
        }
      } else {
        const errorMessage = err.message || "Failed to update endpoint";
        setValidationError(errorMessage);
      }
    }
  };

  // Model row component with alias editing
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
        {/* Checkbox */}
        <div className="col-span-1 flex items-center">
          <Checkbox
            checked={isSelected}
            onCheckedChange={onSelectionChange}
          />
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
                {/* Show if this is a custom alias or if it's an existing deployment */}
                {alias !== model.id && !hasConflict && (
                  <span className="text-xs text-blue-500">(custom)</span>
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
      form.setValue("selectedModels", []);
    } else {
      form.setValue("selectedModels", allModelIds);
      
      const newAliases = { ...modelAliases };
      allModelIds.forEach(modelId => {
        if (!newAliases[modelId]) {
          newAliases[modelId] = modelId;
        }
      });
      setModelAliases(newAliases);
    }
  };

  const shouldShowValidation = urlChanged;
  const shouldShowModels = validationState === "success" && availableModels.length > 0;
  const canUpdate = 
    form.watch("name")?.trim() &&
    !updateEndpointMutation.isPending &&
    validationState !== "testing" &&
    (!urlChanged || validationState === "success") &&
    backendConflicts.size === 0 &&
    localConflicts.length === 0;

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-2xl max-h-[90vh] overflow-y-auto">
        <DialogHeader>
          <div className="flex items-center space-x-3">
            <div className="p-2 bg-doubleword-accent-blue rounded-lg">
              <Edit2 className="w-5 h-5 text-white" />
            </div>
            <DialogTitle>Edit Endpoint</DialogTitle>
          </div>
        </DialogHeader>

        <Form {...form}>
          <form onSubmit={form.handleSubmit(onSubmit)} className="space-y-6">
            {/* Basic Details */}
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

            {/* URL and API Key */}
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
                      <Input type="password" placeholder="sk-..." {...field} />
                    </FormControl>
                  </FormItem>
                )}
              />
            )}

            {/* Validation Status */}
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
              <div className="p-2 bg-green-50 border border-green-200 rounded-lg">
                <div className="flex items-center space-x-2">
                  <Check className="w-4 h-4 text-green-600" />
                  <p className="text-sm text-green-800">
                    Models refreshed • {availableModels.length} found
                  </p>
                </div>
              </div>
            )}

            {/* Configure Models Button */}
            {!shouldShowModels && (
              <div className="space-y-4">
                <div className="flex items-center justify-between">
                  <div>
                    <p className="text-sm text-gray-600">
                      Configure which models to sync from this endpoint
                    </p>
                  </div>
                  <Button
                    type="button"
                    onClick={handleTestConnection}
                    disabled={!form.watch("url")?.trim() || validationState === "testing"}
                    variant="secondary"
                  >
                    {validationState === "testing" ? (
                      <>
                        <Loader2 className="w-4 h-4 mr-2 animate-spin" />
                        Loading...
                      </>
                    ) : (
                      <>
                        <Server className="w-4 h-4 mr-2" />
                        Configure Models
                      </>
                    )}
                  </Button>
                </div>
              </div>
            )}

            {/* Model Selection */}
            {shouldShowModels && (
              <FormField
                control={form.control}
                name="selectedModels"
                render={({ field }) => (
                  <FormItem>
                    <div className="flex items-center justify-between">
                      <div>
                        <FormLabel>Select Models & Configure Aliases</FormLabel>
                        <p className="text-xs text-gray-500">
                          {field.value?.length || 0} of {availableModels.length} selected
                          • Aliases default to model names but can be customized
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
                          disabled={!form.watch("url")?.trim() || validateEndpointMutation.isPending}
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

            {/* Error messages */}
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

            {/* General validation error */}
            {validationError && !validationError.includes("endpoint name") && backendConflicts.size === 0 && (
              <div className="p-4 bg-red-50 border border-red-200 rounded-lg">
                <p className="text-red-800 text-sm">{validationError}</p>
              </div>
            )}
          </form>
        </Form>

        <DialogFooter>
          <Button onClick={onClose} type="button" variant="outline">
            Cancel
          </Button>

          {shouldShowValidation && (
            <Button
              type="button"
              onClick={handleTestConnection}
              disabled={!form.watch("url")?.trim() || validationState === "testing"}
              variant="secondary"
            >
              {validationState === "testing" ? (
                <>
                  <Loader2 className="w-4 h-4 mr-2 animate-spin" />
                  Testing...
                </>
              ) : (
                <>
                  <Server className="w-4 h-4 mr-2" />
                  Test Connection
                </>
              )}
            </Button>
          )}

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
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};
