import React, { useState, useEffect } from "react";
import { Server, Check, AlertCircle, Loader2, Edit2 } from "lucide-react";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from "../../ui/dialog";
import { Button } from "../../ui/button";
import { useValidateEndpoint, useUpdateEndpoint } from "../../../api/waycast";
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

export const EditEndpointModal: React.FC<EditEndpointModalProps> = ({
  isOpen,
  onClose,
  onSuccess,
  endpoint,
}) => {
  // Form state
  const [url, setUrl] = useState("");
  const [apiKey, setApiKey] = useState("");
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");

  // Validation state
  const [validationState, setValidationState] =
    useState<ValidationState>("idle");
  const [validationError, setValidationError] = useState<string | null>(null);
  const [availableModels, setAvailableModels] = useState<AvailableModel[]>([]);

  // Model selection state
  const [selectedModels, setSelectedModels] = useState<Set<string>>(new Set());

  const [error, setError] = useState<string | null>(null);

  // Track if URL has changed to require re-validation
  const [urlChanged, setUrlChanged] = useState(false);

  const validateEndpointMutation = useValidateEndpoint();
  const updateEndpointMutation = useUpdateEndpoint();

  // Initialize form with endpoint data when modal opens
  useEffect(() => {
    if (isOpen && endpoint) {
      setUrl(endpoint.url);
      setApiKey("");
      setName(endpoint.name);
      setDescription(endpoint.description || "");
      setValidationState("idle");
      setValidationError(null);
      setAvailableModels([]);
      // Initialize with existing model filter if available
      setSelectedModels(
        endpoint.model_filter ? new Set(endpoint.model_filter) : new Set(),
      );
      setError(null);
      setUrlChanged(false);
    }
  }, [isOpen, endpoint]);

  const handleRefreshModels = async () => {
    if (!url.trim()) {
      setValidationError("Please enter a URL");
      return;
    }

    const trimmedUrl = url.trim();

    // Validate URL format
    if (
      !trimmedUrl.startsWith("http://") &&
      !trimmedUrl.startsWith("https://")
    ) {
      setValidationError("URL must start with http:// or https://");
      setValidationState("error");
      return;
    }

    setValidationState("testing");
    setValidationError(null);
    setError(null);

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
      } else {
        setValidationError(result.error || "Unknown validation error");
        setValidationState("error");
      }
    } catch (err) {
      setValidationError(
        err instanceof Error ? err.message : "Failed to refresh models",
      );
      setValidationState("error");
    }
  };

  const handleTestConnection = async () => {
    if (!url.trim()) {
      setValidationError("Please enter a URL");
      return;
    }

    const trimmedUrl = url.trim();

    // Validate URL format
    if (
      !trimmedUrl.startsWith("http://") &&
      !trimmedUrl.startsWith("https://")
    ) {
      setValidationError("URL must start with http:// or https://");
      setValidationState("error");
      return;
    }

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

        // If no existing model filter, select all available models by default
        if (!endpoint.model_filter) {
          setSelectedModels(new Set(result.models.data.map((m) => m.id)));
        }
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

  const handleModelToggle = (modelId: string) => {
    const newSelected = new Set(selectedModels);
    if (newSelected.has(modelId)) {
      newSelected.delete(modelId);
    } else {
      newSelected.add(modelId);
    }
    setSelectedModels(newSelected);
  };

  const handleSelectAll = () => {
    if (selectedModels.size === availableModels.length) {
      // Deselect all
      setSelectedModels(new Set());
    } else {
      // Select all
      setSelectedModels(new Set(availableModels.map((m) => m.id)));
    }
  };

  const handleUpdate = async () => {
    if (!name.trim()) {
      setError("Endpoint name is required");
      return;
    }

    // If URL changed, require validation
    if (urlChanged && validationState !== "success") {
      setError("Please test the endpoint connection after changing the URL");
      return;
    }

    setError(null);

    const updateData: EndpointUpdateRequest = {
      name: name.trim(),
      url: url.trim(),
      description: description.trim() || undefined,
      ...(apiKey.trim() && { api_key: apiKey.trim() }),
      ...(selectedModels.size > 0 && {
        model_filter: Array.from(selectedModels),
      }),
    };

    try {
      await updateEndpointMutation.mutateAsync({
        id: endpoint.id.toString(),
        data: updateData,
      });
      onSuccess();
      onClose();
    } catch (err) {
      setError(
        err instanceof Error ? err.message : "Failed to update endpoint",
      );
    }
  };

  const handleUrlChange = (newUrl: string) => {
    setUrl(newUrl);
    const isChanged = newUrl.trim() !== endpoint.url;
    setUrlChanged(isChanged);

    if (isChanged && validationState === "success") {
      setValidationState("idle");
      setAvailableModels([]);
      setSelectedModels(new Set());
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

  const shouldShowValidation = urlChanged;
  const shouldShowModels =
    validationState === "success" && availableModels.length > 0;
  const canUpdate =
    name.trim() &&
    !updateEndpointMutation.isPending &&
    validationState !== "testing" &&
    (!urlChanged || validationState === "success");

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

        <div className="space-y-6">
          {/* Basic Details */}
          <div className="space-y-4 mb-6">
            <div>
              <label className="block text-sm font-medium text-doubleword-neutral-900 mb-2">
                Display Name *
              </label>
              <input
                type="text"
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="My API Endpoint"
                className="w-full px-3 py-2 border border-doubleword-neutral-300 rounded-lg focus:outline-none focus:ring-2 focus:ring-doubleword-accent-blue focus:border-doubleword-accent-blue"
              />
            </div>

            <div>
              <label className="block text-sm font-medium text-doubleword-neutral-900 mb-2">
                Description (optional)
              </label>
              <textarea
                value={description}
                onChange={(e) => setDescription(e.target.value)}
                rows={3}
                placeholder="Description of this endpoint..."
                className="w-full px-3 py-2 border border-doubleword-neutral-300 rounded-lg focus:outline-none focus:ring-2 focus:ring-doubleword-accent-blue focus:border-doubleword-accent-blue resize-none"
              />
            </div>
          </div>

          {/* URL and API Key */}
          <div className="space-y-4 mb-6">
            <h3 className="text-lg font-medium text-doubleword-neutral-900">
              Connection Settings
            </h3>

            <div>
              <label className="block text-sm font-medium text-doubleword-neutral-900 mb-2">
                Endpoint URL *
                {urlChanged && (
                  <span className="text-yellow-600 text-xs ml-2">
                    (Changed - requires testing)
                  </span>
                )}
              </label>
              <input
                type="url"
                value={url}
                onChange={(e) => handleUrlChange(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter" && shouldShowValidation && url.trim()) {
                    e.preventDefault();
                    handleTestConnection();
                  }
                }}
                placeholder="https://api.example.com"
                className="w-full px-3 py-2 border border-doubleword-neutral-300 rounded-lg focus:outline-none focus:ring-2 focus:ring-doubleword-accent-blue focus:border-doubleword-accent-blue"
              />
            </div>

            {endpoint.requires_api_key && (
              <div>
                <label className="block text-sm font-medium text-doubleword-neutral-900 mb-2">
                  API Key (optional)
                  <span className="text-xs text-doubleword-neutral-500 ml-2">
                    Leave empty to keep existing key
                  </span>
                </label>
                <input
                  type="password"
                  value={apiKey}
                  onChange={(e) => setApiKey(e.target.value)}
                  onKeyDown={(e) => {
                    if (
                      e.key === "Enter" &&
                      shouldShowValidation &&
                      url.trim()
                    ) {
                      e.preventDefault();
                      handleTestConnection();
                    }
                  }}
                  placeholder="sk-..."
                  className="w-full px-3 py-2 border border-doubleword-neutral-300 rounded-lg focus:outline-none focus:ring-2 focus:ring-doubleword-accent-blue focus:border-doubleword-accent-blue"
                />
              </div>
            )}
          </div>

          {/* Validation Status */}
          {validationState === "testing" && (
            <div className="mb-6 p-4 bg-blue-50 border border-blue-200 rounded-lg">
              <div className="flex items-center space-x-2">
                <Loader2 className="w-5 h-5 text-blue-600 animate-spin" />
                <p className="text-blue-800 font-medium">
                  Testing connection and fetching models...
                </p>
              </div>
            </div>
          )}

          {validationState === "error" && (
            <div className="mb-6 p-4 bg-red-50 border border-red-200 rounded-lg">
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

          {/* Refresh Models Button */}
          {!shouldShowModels && (
            <div className="space-y-4 mb-6">
              <div className="flex items-center justify-between">
                <div>
                  <p className="text-sm text-doubleword-neutral-600">
                    Configure which models to sync from this endpoint
                  </p>
                </div>
                <button
                  onClick={handleRefreshModels}
                  disabled={!url.trim() || validationState === "testing"}
                  className="inline-flex items-center px-3 py-2 bg-gray-600 text-white text-sm font-medium rounded-lg hover:bg-gray-700 disabled:bg-gray-400 disabled:cursor-not-allowed transition-colors shadow-sm"
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
                </button>
              </div>
            </div>
          )}

          {/* Model Settings */}
          {shouldShowModels && (
            <div className="space-y-4 mb-6">
              <div className="flex items-center justify-between">
                <h3 className="text-lg font-medium text-doubleword-neutral-900">
                  Model Settings
                </h3>
                <button
                  onClick={handleRefreshModels}
                  disabled={!url.trim() || validateEndpointMutation.isPending}
                  className="inline-flex items-center px-3 py-2 bg-gray-500 text-white text-sm font-medium rounded-lg hover:bg-gray-600 disabled:bg-gray-400 disabled:cursor-not-allowed transition-colors"
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
                </button>
              </div>

              <div className="flex items-center justify-between mb-3">
                <p className="text-sm text-doubleword-neutral-600">
                  Select which models to sync ({selectedModels.size} of{" "}
                  {availableModels.length} selected)
                </p>
                <button
                  onClick={handleSelectAll}
                  className="text-sm text-doubleword-accent-blue hover:text-blue-700"
                >
                  {selectedModels.size === availableModels.length
                    ? "Deselect All"
                    : "Select All"}
                </button>
              </div>

              <div className="max-h-40 overflow-y-auto border border-doubleword-neutral-200 rounded-lg">
                {availableModels.map((model) => (
                  <div
                    key={model.id}
                    className="flex items-center space-x-2 p-2 border-b border-doubleword-neutral-100 last:border-b-0 hover:bg-doubleword-neutral-50"
                  >
                    <input
                      type="checkbox"
                      checked={selectedModels.has(model.id)}
                      onChange={() => handleModelToggle(model.id)}
                      className="h-4 w-4 text-doubleword-accent-blue focus:ring-doubleword-accent-blue border-gray-300 rounded"
                    />
                    <div className="flex-1 min-w-0">
                      <p className="text-sm truncate">{model.id}</p>
                      <p className="text-xs text-doubleword-neutral-500">
                        {model.owned_by}
                      </p>
                    </div>
                  </div>
                ))}
              </div>
            </div>
          )}

          {/* Error Display */}
          {error && (
            <div className="mt-4 p-4 bg-red-50 border border-red-200 rounded-lg">
              <p className="text-red-800 text-sm">{error}</p>
            </div>
          )}
        </div>

        <DialogFooter>
          <Button onClick={onClose} type="button" variant="outline">
            Cancel
          </Button>

          {shouldShowValidation && (
            <Button
              onClick={handleTestConnection}
              disabled={!url.trim() || validationState === "testing"}
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

          <Button onClick={handleUpdate} disabled={!canUpdate}>
            {updateEndpointMutation.isPending ? (
              <>
                <Loader2 className="w-4 h-4 mr-2 animate-spin" />
                Updating...
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
