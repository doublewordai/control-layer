import React, { useCallback, useEffect, useMemo, useState } from "react";
import {
  Check,
  AlertCircle,
  Loader2,
  Info,
  ChevronDown,
  Eye,
  EyeOff,
  X,
  RefreshCw,
} from "lucide-react";
import { useForm } from "react-hook-form";
import { zodResolver } from "@hookform/resolvers/zod";
import * as z from "zod";
import { toast } from "sonner";
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
import { AddModelPalette } from "../EditEndpointModal/AddModelPalette";
import { ImportedModelsTable } from "../EditEndpointModal/ImportedModelsTable";
import { useEndpointModelsState } from "../EditEndpointModal/useEndpointModelsState";
import type { DeploymentReferences } from "../EditEndpointModal/references";

interface CreateEndpointModalProps {
  isOpen: boolean;
  onClose: () => void;
  onSuccess: () => void;
}

type ValidationState = "idle" | "testing" | "success" | "error";

const formSchema = z.object({
  url: z.string().min(1, "URL is required").url("Please enter a valid URL"),
  apiKey: z.string().optional(),
  name: z.string().min(1, "Endpoint name is required"),
  description: z.string().optional(),
  authHeaderName: z.string().optional(),
  authHeaderPrefix: z.string().optional(),
});

type FormData = z.infer<typeof formSchema>;

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
        object: "model" as const,
        owned_by: "anthropic",
      },
      {
        id: "claude-4-opus",
        created: 0,
        object: "model" as const,
        owned_by: "anthropic",
      },
      {
        id: "claude-3-7-sonnet",
        created: 0,
        object: "model" as const,
        owned_by: "anthropic",
      },
      {
        id: "claude-3-5-sonnet",
        created: 0,
        object: "model" as const,
        owned_by: "anthropic",
      },
      { id: "openai-gpt-4.1", created: 0, object: "model" as const, owned_by: "openai" },
      {
        id: "openai-gpt-5-chat",
        created: 0,
        object: "model" as const,
        owned_by: "openai",
      },
      { id: "llama4-maverick", created: 0, object: "model" as const, owned_by: "meta" },
      { id: "llama3.1-8b", created: 0, object: "model" as const, owned_by: "meta" },
      { id: "llama3.1-70b", created: 0, object: "model" as const, owned_by: "meta" },
      { id: "llama3.1-405b", created: 0, object: "model" as const, owned_by: "meta" },
      { id: "deepseek-r1", created: 0, object: "model" as const, owned_by: "deepseek" },
      { id: "mistral-7b", created: 0, object: "model" as const, owned_by: "mistralai" },
      {
        id: "mistral-large",
        created: 0,
        object: "model" as const,
        owned_by: "mistralai",
      },
      {
        id: "mistral-large2",
        created: 0,
        object: "model" as const,
        owned_by: "mistralai",
      },
      {
        id: "snowflake-llama-3.3-70b",
        created: 0,
        object: "model" as const,
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

function findMatchedEndpoint(url: string | undefined) {
  if (!url) return undefined;
  return POPULAR_ENDPOINTS.find((ep) => {
    if (ep.domain) return url.includes(ep.domain);
    return url.trim() === ep.url;
  });
}

export const CreateEndpointModal: React.FC<CreateEndpointModalProps> = ({
  isOpen,
  onClose,
  onSuccess,
}) => {
  // ----- Step 1 (Connection) state -----
  const [validationState, setValidationState] =
    useState<ValidationState>("idle");
  const [validationError, setValidationError] = useState<string | null>(null);
  const [urlPopoverOpen, setUrlPopoverOpen] = useState(false);
  const [advancedPopoverOpen, setAdvancedPopoverOpen] = useState(false);
  const [showApiKey, setShowApiKey] = useState(false);
  const [quoteApiKey, setQuoteApiKey] = useState(false);
  const [currentStep, setCurrentStep] = useState<1 | 2>(1);

  // ----- Step 2 (Models) state -----
  // catalog feeds the AddModelPalette and seeds the staged-state hook so all
  // discovered models start as imported (matching the "select all by default"
  // behaviour of the previous flow).
  const [catalog, setCatalog] = useState<AvailableModel[]>([]);
  // True when the user explicitly bypassed discovery — sent as `skip_fetch` so
  // the backend doesn't try to re-fetch a model list the endpoint doesn't expose.
  const [discoveryWasSkipped, setDiscoveryWasSkipped] = useState(false);
  const [backendConflicts, setBackendConflicts] = useState<Set<string>>(
    new Set(),
  );
  const [submitError, setSubmitError] = useState<string | null>(null);

  const validateEndpointMutation = useValidateEndpoint();
  const createEndpointMutation = useCreateEndpoint();

  const form = useForm<FormData>({
    resolver: zodResolver(formSchema),
    defaultValues: {
      url: "",
      apiKey: "",
      name: "",
      description: "",
      authHeaderName: "",
      authHeaderPrefix: "",
    },
  });

  // Reset all state when the modal opens.
  useEffect(() => {
    if (!isOpen) return;
    form.reset();
    setValidationState("idle");
    setValidationError(null);
    setCatalog([]);
    setDiscoveryWasSkipped(false);
    setBackendConflicts(new Set());
    setAdvancedPopoverOpen(false);
    setShowApiKey(false);
    setQuoteApiKey(false);
    setCurrentStep(1);
    setSubmitError(null);
  }, [isOpen, form]);

  const initialDeployments = useMemo(
    () => catalog.map((m) => ({ modelName: m.id, alias: m.id })),
    [catalog],
  );

  const modelsState = useEndpointModelsState(initialDeployments);

  // No deployments exist yet for a new endpoint, so references can't exist.
  // Pass an empty map; the silent-removal path always fires.
  const referencesByModelName = useMemo<Map<string, DeploymentReferences>>(
    () => new Map(),
    [],
  );

  const conflictingAliases = useMemo(() => {
    const groups = new Map<string, string[]>();
    for (const d of modelsState.deployments) {
      const normalized = d.alias.trim().toLowerCase();
      if (!normalized) continue;
      const list = groups.get(normalized) ?? [];
      list.push(d.alias);
      groups.set(normalized, list);
    }
    const dupes = new Set<string>();
    for (const [, originals] of groups) {
      if (originals.length > 1) {
        for (const a of originals) dupes.add(a);
      }
    }
    return dupes;
  }, [modelsState.deployments]);

  const importedModelNames = useMemo(
    () => new Set(modelsState.deployments.map((d) => d.modelName)),
    [modelsState.deployments],
  );

  const invalidateBackendConflicts = useCallback(() => {
    setBackendConflicts((prev) => (prev.size === 0 ? prev : new Set()));
  }, []);

  const handleAddModel = (modelName: string) => {
    modelsState.addModel(modelName);
    invalidateBackendConflicts();
  };

  const handleAliasChange = (modelName: string, alias: string) => {
    modelsState.setAlias(modelName, alias);
    invalidateBackendConflicts();
  };

  const handleRemoveModel = (modelName: string) => {
    // No references possible for a brand-new endpoint, so removal is always silent.
    const undo = modelsState.removeModel(modelName);
    invalidateBackendConflicts();
    toast(`Removed ${modelName}`, {
      action: { label: "Undo", onClick: undo },
    });
  };

  const autoPopulateNameFromUrl = (url: string) => {
    if (form.getValues("name")) return;
    try {
      const urlObj = new URL(url);
      form.setValue("name", urlObj.hostname);
    } catch {
      // Invalid URL — leave name unset; the user will fill it in.
    }
  };

  const handleUrlChange = (url: string) => {
    const matched = findMatchedEndpoint(url);
    setQuoteApiKey(matched?.quoteApiKey ?? false);

    // Reset validation if the user changes URL after a successful validate.
    if (validationState === "success") {
      setValidationState("idle");
      setCatalog([]);
      setDiscoveryWasSkipped(false);
      setBackendConflicts(new Set());
    }
    if (validationError) {
      setValidationError(null);
      setValidationState("idle");
    }
  };

  const advanceWithCatalog = (
    discoveredCatalog: AvailableModel[],
    skipped: boolean,
  ) => {
    setCatalog(discoveredCatalog);
    setDiscoveryWasSkipped(skipped);
    setValidationState("success");
    autoPopulateNameFromUrl(form.getValues("url"));
    setCurrentStep(2);
  };

  const handleDiscoverModels = async () => {
    const url = form.getValues("url");
    const apiKey = form.getValues("apiKey");
    const authHeaderName = form.getValues("authHeaderName");
    const authHeaderPrefix = form.getValues("authHeaderPrefix");

    if (!url) {
      form.setError("url", { message: "Please enter a URL" });
      return;
    }

    form.clearErrors("url");
    setValidationState("testing");
    setValidationError(null);

    // skipValidation endpoints (e.g. Snowflake Cortex AI) ship a static catalog
    // so we bypass the network probe entirely.
    const matched = findMatchedEndpoint(url);
    if (matched?.skipValidation && matched?.knownModels) {
      advanceWithCatalog(matched.knownModels as AvailableModel[], true);
      return;
    }

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

    try {
      const result = await validateEndpointMutation.mutateAsync(validateData);

      if (result.status === "success" && result.models) {
        advanceWithCatalog(result.models.data, false);
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

  const handleContinueWithoutDiscovery = () => {
    const url = form.getValues("url");
    if (!url) {
      form.setError("url", { message: "Please enter a URL" });
      return;
    }

    // Pre-seed with known models for endpoints that have them, otherwise empty.
    const matched = findMatchedEndpoint(url);
    const seedCatalog = (matched?.knownModels as AvailableModel[]) ?? [];

    advanceWithCatalog(seedCatalog, true);
  };

  const handleBack = () => {
    setCurrentStep(1);
    setValidationError(null);
    setSubmitError(null);
    setBackendConflicts(new Set());
  };

  const onSubmit = async (data: FormData) => {
    setSubmitError(null);
    setBackendConflicts(new Set());

    if (modelsState.deployments.length === 0) {
      setSubmitError(
        "Add at least one model before creating the endpoint.",
      );
      return;
    }

    if (conflictingAliases.size > 0) {
      setSubmitError(
        "Two deployments share the same alias. Please make all aliases unique.",
      );
      return;
    }

    const visibleModelNames = modelsState.deployments.map((d) => d.modelName);

    // Match the original behaviour: only send alias_mapping for entries where
    // the alias differs from the model_name. Trim first; fall back to model
    // name if the trimmed alias is empty so we never POST a blank string.
    const aliasMapping: Record<string, string> = {};
    for (const d of modelsState.deployments) {
      const trimmed = (d.alias ?? "").trim();
      const finalAlias = trimmed.length > 0 ? trimmed : d.modelName;
      if (finalAlias !== d.modelName) {
        aliasMapping[d.modelName] = finalAlias;
      }
    }

    const processedApiKey = data.apiKey?.trim()
      ? quoteApiKey
        ? `"${data.apiKey.trim()}"`
        : data.apiKey.trim()
      : undefined;

    const matched = findMatchedEndpoint(data.url);
    const skipFetch = discoveryWasSkipped || matched?.skipValidation === true;

    const createData: EndpointCreateRequest = {
      name: data.name.trim(),
      url: data.url.trim(),
      ...(data.description?.trim() && { description: data.description.trim() }),
      ...(processedApiKey && { api_key: processedApiKey }),
      model_filter: visibleModelNames,
      ...(Object.keys(aliasMapping).length > 0 && { alias_mapping: aliasMapping }),
      ...(data.authHeaderName?.trim() && {
        auth_header_name: data.authHeaderName.trim(),
      }),
      ...(data.authHeaderPrefix?.trim() && {
        auth_header_prefix: data.authHeaderPrefix.trim(),
      }),
      ...(skipFetch && { skip_fetch: true }),
    };

    try {
      await createEndpointMutation.mutateAsync(createData);
      onSuccess();
      onClose();
    } catch (err: any) {
      if (err.status === 409 || err.response?.status === 409) {
        const responseData = err.response?.data || err.data;

        if (responseData?.resource === "endpoint") {
          form.setError("name", {
            type: "endpoint_name_conflict",
            message: "endpoint_name_conflict",
          });
          setSubmitError("Please choose a different endpoint name.");
          return;
        }

        if (responseData?.conflicts) {
          const conflictAliases = responseData.conflicts.map(
            (c: any) => c.attempted_alias || c.alias,
          );
          setBackendConflicts(new Set(conflictAliases));
          setSubmitError(
            "Some model aliases already exist. Please rename the highlighted aliases.",
          );
        } else {
          setSubmitError(
            responseData?.message ||
              "A conflict occurred. Please check your input.",
          );
        }
      } else {
        setSubmitError(err.message || "Failed to create endpoint");
      }
    }
  };

  const canCreate =
    !!form.watch("name")?.trim() &&
    !createEndpointMutation.isPending &&
    modelsState.deployments.length > 0 &&
    backendConflicts.size === 0 &&
    conflictingAliases.size === 0;

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-2xl max-h-[90vh] flex flex-col [&>button]:hidden">
        <DialogHeader>
          <div className="flex items-center justify-between">
            <DialogTitle>Add Endpoint</DialogTitle>
            <Stepper currentStep={currentStep} />
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
              {currentStep === 1 && (
                <ConnectionStep
                  form={form}
                  validationState={validationState}
                  validationError={validationError}
                  urlPopoverOpen={urlPopoverOpen}
                  setUrlPopoverOpen={setUrlPopoverOpen}
                  advancedPopoverOpen={advancedPopoverOpen}
                  setAdvancedPopoverOpen={setAdvancedPopoverOpen}
                  showApiKey={showApiKey}
                  setShowApiKey={setShowApiKey}
                  quoteApiKey={quoteApiKey}
                  setQuoteApiKey={setQuoteApiKey}
                  onUrlChange={handleUrlChange}
                />
              )}

              {currentStep === 2 && (
                <ModelsStep
                  form={form}
                  modelsState={modelsState}
                  referencesByModelName={referencesByModelName}
                  conflictingAliases={conflictingAliases}
                  backendConflicts={backendConflicts}
                  catalog={catalog}
                  importedModelNames={importedModelNames}
                  onAddModel={handleAddModel}
                  onAliasChange={handleAliasChange}
                  onRemoveModel={handleRemoveModel}
                  submitError={submitError}
                />
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
              {validationState === "error" && (
                <Button
                  type="button"
                  variant="ghost"
                  onClick={handleContinueWithoutDiscovery}
                >
                  Continue without discovery
                </Button>
              )}
              <Button
                type="button"
                onClick={handleDiscoverModels}
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
                    <RefreshCw className="w-4 h-4 mr-2" />
                    Discover Models
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
                disabled={!canCreate}
              >
                {createEndpointMutation.isPending ? (
                  <>
                    <Loader2 className="w-4 h-4 mr-2 animate-spin" />
                    Creating Endpoint...
                  </>
                ) : conflictingAliases.size > 0 ? (
                  <>
                    <AlertCircle className="w-4 h-4 mr-2" />
                    Fix duplicate aliases
                  </>
                ) : backendConflicts.size > 0 ? (
                  <>
                    <AlertCircle className="w-4 h-4 mr-2" />
                    Resolve conflicts
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

// ---------------------------------------------------------------------------
// Step 1: Connection
// ---------------------------------------------------------------------------

interface ConnectionStepProps {
  form: ReturnType<typeof useForm<FormData>>;
  validationState: ValidationState;
  validationError: string | null;
  urlPopoverOpen: boolean;
  setUrlPopoverOpen: (v: boolean) => void;
  advancedPopoverOpen: boolean;
  setAdvancedPopoverOpen: (v: boolean) => void;
  showApiKey: boolean;
  setShowApiKey: (v: boolean) => void;
  quoteApiKey: boolean;
  setQuoteApiKey: (v: boolean) => void;
  onUrlChange: (url: string) => void;
}

const ConnectionStep: React.FC<ConnectionStepProps> = ({
  form,
  validationState,
  validationError,
  urlPopoverOpen,
  setUrlPopoverOpen,
  advancedPopoverOpen,
  setAdvancedPopoverOpen,
  showApiKey,
  setShowApiKey,
  quoteApiKey,
  setQuoteApiKey,
  onUrlChange,
}) => {
  const currentUrl = form.watch("url");
  const matchedEndpoint = findMatchedEndpoint(currentUrl);

  return (
    <div className="space-y-6">
      <FormField
        control={form.control}
        name="url"
        render={({ field }) => (
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
                    <span className="sr-only">Endpoint URL information</span>
                  </button>
                </HoverCardTrigger>
                <HoverCardContent className="w-80" sideOffset={5}>
                  <p className="text-sm text-muted-foreground">
                    The base URL is the url that you provide when using the
                    OpenAI client libraries. It might include a version
                    specifier after the root domain: for example,
                    https://api.openai.com/v1
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
                    onUrlChange(e.target.value);
                  }}
                />
                {field.value ? (
                  <button
                    type="button"
                    className="absolute right-0 top-0 h-full px-3 text-gray-500 hover:text-gray-700 transition-colors border-l"
                    onClick={() => {
                      form.setValue("url", "");
                      onUrlChange("");
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
                        <span className="sr-only">Select popular endpoint</span>
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

                              if (endpoint.name === "Snowflake SPCS Endpoint") {
                                form.setValue(
                                  "authHeaderPrefix",
                                  endpoint.authHeaderPrefix || "",
                                );
                                setQuoteApiKey(endpoint.quoteApiKey || false);
                              } else {
                                form.setValue("authHeaderPrefix", "");
                                setQuoteApiKey(false);
                              }

                              onUrlChange(endpoint.url);
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
                              <div className="text-xs text-gray-500 truncate">
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
        )}
      />

      <FormField
        control={form.control}
        name="apiKey"
        render={({ field }) => (
          <FormItem>
            <FormLabel>
              API Key{" "}
              {matchedEndpoint?.requiresApiKey ? "*" : "(optional)"}
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
        )}
      />

      <Popover open={advancedPopoverOpen} onOpenChange={setAdvancedPopoverOpen}>
        <PopoverTrigger asChild>
          <button
            type="button"
            className="flex items-center gap-2 text-sm font-medium text-gray-700 hover:text-gray-900 transition-colors group"
          >
            Advanced Configuration
            <ChevronDown
              className={
                "w-4 h-4 transition-transform group-hover:translate-y-px " +
                (advancedPopoverOpen ? "rotate-180" : "")
              }
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
                    <Input placeholder='"Authorization"' {...field} />
                  </FormControl>
                  <FormDescription>
                    The HTTP header name provided with upstream requests to
                    this endpoint.
                  </FormDescription>
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
                  <FormDescription>
                    The prefix before the API key header value. Default is
                    "Bearer " (with trailing space).
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
              Wrap the API key in double quotes. Useful for endpoints like
              Snowflake that require quoted tokens.
            </FormDescription>
          </div>
        </PopoverContent>
      </Popover>

      {validationState === "error" && (
        <div className="p-4 bg-red-50 border border-red-200 rounded-lg">
          <div className="flex items-center space-x-2">
            <AlertCircle className="w-5 h-5 text-red-600" />
            <p className="text-red-800 font-medium">Connection Failed</p>
          </div>
          <p className="text-red-700 text-sm mt-1">{validationError}</p>
          <p className="text-red-700 text-xs mt-2">
            You can still continue without discovery and add models manually.
          </p>
        </div>
      )}
    </div>
  );
};

// ---------------------------------------------------------------------------
// Step 2: Models
// ---------------------------------------------------------------------------

interface ModelsStepProps {
  form: ReturnType<typeof useForm<FormData>>;
  modelsState: ReturnType<typeof useEndpointModelsState>;
  referencesByModelName: Map<string, DeploymentReferences>;
  conflictingAliases: Set<string>;
  backendConflicts: Set<string>;
  catalog: AvailableModel[];
  importedModelNames: Set<string>;
  onAddModel: (modelName: string) => void;
  onAliasChange: (modelName: string, alias: string) => void;
  onRemoveModel: (modelName: string) => void;
  submitError: string | null;
}

const ModelsStep: React.FC<ModelsStepProps> = ({
  form,
  modelsState,
  referencesByModelName,
  conflictingAliases,
  backendConflicts,
  catalog,
  importedModelNames,
  onAddModel,
  onAliasChange,
  onRemoveModel,
  submitError,
}) => (
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
          {fieldState.error?.message !== "endpoint_name_conflict" && (
            <FormMessage />
          )}
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

    <div>
      <div className="flex items-center justify-between mb-3">
        <div>
          <p className="text-sm font-medium">Imported models</p>
          <p className="text-xs text-gray-500">
            {modelsState.deployments.length === 0
              ? "Add at least one model to create the endpoint"
              : `${modelsState.deployments.length} imported · click an alias to rename`}
          </p>
        </div>
        <AddModelPalette
          catalog={catalog}
          importedModelNames={importedModelNames}
          onAdd={onAddModel}
        />
      </div>

      <ImportedModelsTable
        deployments={modelsState.deployments}
        referencesByModelName={referencesByModelName}
        conflictingAliases={
          new Set([...conflictingAliases, ...backendConflicts])
        }
        onAliasChange={onAliasChange}
        onRemove={onRemoveModel}
      />
    </div>

    {form.formState.errors.name?.type === "endpoint_name_conflict" && (
      <div className="p-3 bg-red-50 border border-red-200 rounded-md">
        <div className="flex items-center space-x-2">
          <AlertCircle className="w-4 h-4 text-red-500 shrink-0" />
          <p className="text-sm text-red-700">
            <strong>Endpoint name conflict:</strong> Please choose a different
            display name above.
          </p>
        </div>
      </div>
    )}

    {conflictingAliases.size > 0 && (
      <div className="p-3 bg-orange-50 border border-orange-200 rounded-md">
        <div className="flex items-center space-x-2">
          <AlertCircle className="w-4 h-4 text-orange-500 shrink-0" />
          <p className="text-sm text-orange-700">
            <strong>Duplicate aliases detected:</strong>{" "}
            {[...conflictingAliases].join(", ")}. Each alias must be unique.
          </p>
        </div>
      </div>
    )}

    {backendConflicts.size > 0 && (
      <div className="p-3 bg-red-50 border border-red-200 rounded-md">
        <div className="flex items-center space-x-2">
          <AlertCircle className="w-4 h-4 text-red-500 shrink-0" />
          <p className="text-sm text-red-700">
            <strong>Model alias conflict:</strong> Please rename the highlighted
            aliases above.
          </p>
        </div>
      </div>
    )}

    {submitError && (
      <div className="p-4 bg-red-50 border border-red-200 rounded-lg">
        <p className="text-red-800 text-sm">{submitError}</p>
      </div>
    )}
  </div>
);

// ---------------------------------------------------------------------------
// Stepper
// ---------------------------------------------------------------------------

const Stepper: React.FC<{ currentStep: 1 | 2 }> = ({ currentStep }) => (
  <div className="flex items-center space-x-2">
    <StepBubble
      n={1}
      label="Connection"
      state={
        currentStep === 1 ? "active" : currentStep > 1 ? "done" : "pending"
      }
    />
    <div
      className={
        "w-12 h-0.5 " + (currentStep > 1 ? "bg-emerald-500" : "bg-gray-300")
      }
    />
    <StepBubble
      n={2}
      label="Models"
      state={currentStep === 2 ? "active" : "pending"}
    />
  </div>
);

const StepBubble: React.FC<{
  n: number;
  label: string;
  state: "active" | "done" | "pending";
}> = ({ n, label, state }) => {
  const text =
    state === "active"
      ? "text-gray-700 font-medium"
      : state === "done"
        ? "text-emerald-600 font-medium"
        : "text-gray-400";
  const bubble =
    state === "active"
      ? "border-gray-700 bg-gray-700 text-white"
      : state === "done"
        ? "border-emerald-500 bg-emerald-500 text-white"
        : "border-gray-300 text-gray-400";
  return (
    <div className={`flex items-center ${text}`}>
      <div
        className={`w-8 h-8 rounded-full flex items-center justify-center border-2 ${bubble}`}
      >
        {state === "done" ? <Check className="w-4 h-4" /> : n}
      </div>
      <span className="ml-2 text-sm">{label}</span>
    </div>
  );
};
