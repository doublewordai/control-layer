import React, { useState, useEffect } from "react";
import { Server, Check, AlertCircle, Loader2 } from "lucide-react";
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
