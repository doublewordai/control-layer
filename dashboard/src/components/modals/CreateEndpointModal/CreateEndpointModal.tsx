import React, { useState, useEffect } from "react";
import {
  Server,
  Check,
  AlertCircle,
  Loader2,
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
  const [urlPopoverOpen, setUrlPopoverOpen] = useState(false);

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
    }
  }, [isOpen, form]);

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

    const createData: EndpointCreateRequest = {
      name: data.name.trim(),
      url: data.url.trim(),
      ...(data.description?.trim() && { description: data.description.trim() }),
      ...(data.apiKey?.trim() && { api_key: data.apiKey.trim() }),
      ...(data.selectedModels.length > 0 && {
        model_filter: data.selectedModels,
      }),
    };

    try {
      await createEndpointMutation.mutateAsync(createData);
      onSuccess();
      onClose();
    } catch (err) {
      setValidationError(
        err instanceof Error ? err.message : "Failed to create endpoint",
      );
    }
  };

  const handleSelectAll = () => {
    const currentSelection = form.getValues("selectedModels");
    if (currentSelection.length === availableModels.length) {
      form.setValue("selectedModels", []);
    } else {
      form.setValue(
        "selectedModels",
        availableModels.map((m) => m.id),
      );
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
                            <FormLabel>Select Models</FormLabel>
                            <FormDescription className="text-xs">
                              {field.value?.length || 0} of{" "}
                              {availableModels.length} selected
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
                        <div className="max-h-40 overflow-y-auto border rounded-lg mt-2">
                          {availableModels.map((model) => (
                            <div
                              key={model.id}
                              className="flex items-center space-x-2 p-2 border-b last:border-b-0 hover:bg-gray-50"
                            >
                              <FormControl>
                                <Checkbox
                                  checked={field.value?.includes(model.id)}
                                  onCheckedChange={(checked) => {
                                    const current = field.value || [];
                                    if (checked) {
                                      field.onChange([...current, model.id]);
                                    } else {
                                      field.onChange(
                                        current.filter((id) => id !== model.id),
                                      );
                                    }
                                  }}
                                />
                              </FormControl>
                              <div className="flex-1 min-w-0">
                                <p className="text-sm truncate">{model.id}</p>
                                <p className="text-xs text-gray-500">
                                  {model.owned_by}
                                </p>
                              </div>
                            </div>
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
                !form.watch("selectedModels")?.length
              }
            >
              {createEndpointMutation.isPending ? (
                <>
                  <Loader2 className="w-4 h-4 mr-2 animate-spin" />
                  Creating Endpoint...
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
