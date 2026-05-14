import React, { useCallback, useEffect, useMemo, useState } from "react";
import {
  Server,
  Check,
  AlertCircle,
  Loader2,
  ChevronDown,
  Eye,
  EyeOff,
} from "lucide-react";
import { useForm } from "react-hook-form";
import { zodResolver } from "@hookform/resolvers/zod";
import * as z from "zod";
import { toast } from "sonner";
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
import { Button } from "../../ui/button";
import { Popover, PopoverContent, PopoverTrigger } from "../../ui/popover";
import {
  useValidateEndpoint,
  useUpdateEndpoint,
  useModels,
  dwctlApi,
} from "../../../api/control-layer";
import { useServerPagination } from "../../../hooks/useServerPagination";
import type {
  EndpointValidateRequest,
  AvailableModel,
  EndpointUpdateRequest,
  Endpoint,
  Model,
} from "../../../api/control-layer/types";
import { AddModelPalette } from "./AddModelPalette";
import { ImportedModelsTable } from "./ImportedModelsTable";
import { RemoveModelDialog } from "./RemoveModelDialog";
import {
  useEndpointModelsState,
  type ImportedDeployment,
} from "./useEndpointModelsState";
import {
  buildReferenceIndex,
  emptyReferences,
  hasUserConfiguredReferences,
  lookupReferences,
  type DeploymentReferences,
} from "./references";

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
  // ----- Step 1 (Connection) state -----
  const [validationState, setValidationState] =
    useState<ValidationState>("idle");
  const [validationError, setValidationError] = useState<string | null>(null);
  const [urlChanged, setUrlChanged] = useState(false);
  const [advancedPopoverOpen, setAdvancedPopoverOpen] = useState(false);
  const [showApiKey, setShowApiKey] = useState(false);
  const [currentStep, setCurrentStep] = useState<1 | 2>(1);

  // ----- Step 2 (Models) state -----
  const [catalog, setCatalog] = useState<AvailableModel[]>([]);
  const [allModels, setAllModels] = useState<Model[]>([]);
  const [pendingRemoval, setPendingRemoval] = useState<{
    modelName: string;
    references: DeploymentReferences;
  } | null>(null);
  const [backendConflicts, setBackendConflicts] = useState<Set<string>>(
    new Set(),
  );
  const [submitError, setSubmitError] = useState<string | null>(null);
  // True while we loop all deployment pages at submit time to rebuild the
  // authoritative model_filter. Distinct from the per-page query loading flag.
  const [isBuildingFilter, setIsBuildingFilter] = useState(false);

  const validateEndpointMutation = useValidateEndpoint();
  const updateEndpointMutation = useUpdateEndpoint();

  // Server-driven pagination for the current endpoint's deployments. URL
  // params are prefixed so they don't collide with the underlying Endpoints
  // page state, and cleared on close.
  const pagination = useServerPagination({
    paramPrefix: "endpointModels",
    defaultPageSize: 10,
  });

  // Per-page fetch of this endpoint's deployments. Driven by the pagination
  // hook above; only enabled once we reach step 2 so we don't waste a request
  // while the user is still on the connection step.
  const deploymentsQuery = useModels({
    endpoint: endpoint.id.toString(),
    skip: pagination.queryParams.skip,
    limit: pagination.queryParams.limit,
    enabled: isOpen && currentStep === 2,
  });

  const currentPageInitial = useMemo(
    () =>
      (deploymentsQuery.data?.data ?? []).map((m) => ({
        modelName: m.model_name,
        alias: m.alias,
      })),
    [deploymentsQuery.data],
  );

  const totalDeployments = deploymentsQuery.data?.total_count ?? 0;

  // Pull every model in the org so we can compute references for each deployment.
  // include=components is essential — virtual models reference us via their
  // component list. Gated on `isOpen` so we don't fetch while the modal is
  // mounted-but-hidden; the result is cached, so reopening the modal is cheap.
  // limit=100 (backend max) covers most orgs; orgs with >100 models will see
  // incomplete reference warnings — a paginating fetch would resolve this.
  const allModelsQuery = useModels({
    include: "components",
    limit: 100,
    enabled: isOpen,
  });

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

  // Initialize on open. Deployments themselves are fetched lazily by
  // `deploymentsQuery` once we reach step 2.
  useEffect(() => {
    if (!isOpen || !endpoint) return;

    form.reset({
      url: endpoint.url,
      apiKey: "",
      name: endpoint.name,
      description: endpoint.description || "",
      authHeaderName: "",
      authHeaderPrefix: "",
    });

    setValidationState("idle");
    setValidationError(null);
    setUrlChanged(false);
    setCurrentStep(1);
    setBackendConflicts(new Set());
    setSubmitError(null);
    setPendingRemoval(null);
    setIsBuildingFilter(false);
  }, [isOpen, endpoint, form]);

  // Strip the pagination URL params when the modal closes. Cancel/X/ESC
  // all flow through `isOpen` toggling false, so a single effect catches
  // every close path. handleClear is memoized so it's a stable dep.
  const clearPagination = pagination.handleClear;
  useEffect(() => {
    if (!isOpen) clearPagination();
  }, [isOpen, clearPagination]);

  // Track all models for reference computation (separate query, may load after open)
  useEffect(() => {
    if (allModelsQuery.data) {
      setAllModels(allModelsQuery.data.data);
    }
  }, [allModelsQuery.data]);

  const modelsState = useEndpointModelsState(currentPageInitial);

  // Build the reference index once per `allModels` change. Per-deployment
  // lookups against this index are O(1), so the references map below is cheap
  // to recompute on every render even during alias-input keystrokes.
  const referenceIndex = useMemo(
    () => buildReferenceIndex(allModels),
    [allModels],
  );

  const referencesByModelName = useMemo(() => {
    const map = new Map<string, DeploymentReferences>();
    for (const d of modelsState.deployments) {
      map.set(
        d.modelName,
        lookupReferences(referenceIndex, endpoint.id, d.modelName),
      );
    }
    return map;
  }, [modelsState.deployments, referenceIndex, endpoint.id]);

  // Local conflict detection. Mirrors the backend's `LOWER(alias)` uniqueness
  // constraint and the `.trim()` we apply on submit, so that "Foo"/"foo" and
  // "foo"/" foo" are caught before save instead of producing 409s.
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

  const handleUrlChange = (newUrl: string) => {
    const isChanged = newUrl.trim() !== endpoint.url;
    setUrlChanged(isChanged);

    if (isChanged && validationState === "success") {
      setValidationState("idle");
    }

    if (validationError) {
      setValidationError(null);
      setValidationState(isChanged ? "idle" : "success");
    }
  };

  const handleDiscoverModels = async () => {
    const url = form.getValues("url");

    if (!url.trim()) {
      form.setError("url", { message: "Please enter a URL" });
      return;
    }

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
        setCatalog(result.models.data);
        setValidationState("success");
        setUrlChanged(false);
        setCurrentStep(2);
      } else {
        setCatalog([]);
        setValidationError(result.error || "Unknown validation error");
        setValidationState("error");
      }
    } catch (err) {
      setCatalog([]);
      setValidationError(
        err instanceof Error ? err.message : "Failed to test connection",
      );
      setValidationState("error");
    }
  };

  const handleContinueWithoutDiscovery = () => {
    setCatalog([]);
    setValidationError(null);
    setValidationState("idle");
    setCurrentStep(2);
  };

  const handleBack = () => {
    setCurrentStep(1);
    setValidationError(null);
    setSubmitError(null);
    setBackendConflicts(new Set());
  };

  // backendConflicts holds *aliases* (not modelNames) returned by the server's
  // 409 response. Any user edit that could change the alias set invalidates
  // them, so we clear the whole set on add/remove/alias-edit and let the next
  // save round-trip surface fresh ones.
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
    const refs =
      referencesByModelName.get(modelName) ?? emptyReferences();

    // The deployment's own implicit Standard Model wrapper is always present
    // and isn't a "user-configured" dependency — only warn when there are
    // additional wrappers, virtual model components, or traffic rules.
    if (!hasUserConfiguredReferences(refs)) {
      const undo = modelsState.removeModel(modelName);
      invalidateBackendConflicts();
      toast(`Removed ${modelName}`, {
        action: {
          label: "Undo",
          onClick: undo,
        },
      });
      return;
    }

    setPendingRemoval({ modelName, references: refs });
  };

  const confirmRemoval = () => {
    if (!pendingRemoval) return;
    modelsState.removeModel(pendingRemoval.modelName);
    invalidateBackendConflicts();
    setPendingRemoval(null);
  };

  const onSubmit = async (data: FormData) => {
    if (urlChanged && validationState !== "success") {
      setSubmitError(
        "Please test the endpoint connection after changing the URL",
      );
      return;
    }

    setSubmitError(null);
    setBackendConflicts(new Set());

    if (conflictingAliases.size > 0) {
      setSubmitError(
        "Two deployments share the same alias. Please make all aliases unique.",
      );
      return;
    }

    // Server pagination means we only hold one page of deployments in memory.
    // To build the authoritative model_filter we have to read every page now.
    // This is a small cost paid once on save; typical endpoints have tens of
    // deployments, not thousands.
    setIsBuildingFilter(true);
    const allServerDeployments: { modelName: string; alias: string }[] = [];
    try {
      const PAGE_LIMIT = 100;
      let skip = 0;
      while (true) {
        const page = await dwctlApi.models.list({
          endpoint: endpoint.id.toString(),
          skip,
          limit: PAGE_LIMIT,
        });
        for (const m of page.data) {
          allServerDeployments.push({
            modelName: m.model_name,
            alias: m.alias,
          });
        }
        skip += PAGE_LIMIT;
        if (skip >= page.total_count || page.data.length === 0) break;
      }
    } catch (err: any) {
      setIsBuildingFilter(false);
      setSubmitError(
        err?.message || "Failed to load endpoint deployments before saving",
      );
      return;
    }
    setIsBuildingFilter(false);

    // Reconcile staged deltas against the full server state:
    //  - keep every server deployment that wasn't staged for removal
    //  - apply alias edits (from staged.aliases) where present
    //  - append session adds that don't already exist server-side
    const removedSet = new Set(modelsState.removedModelNames);
    const aliasEdits = modelsState.aliasEdits;
    const serverNameSet = new Set(
      allServerDeployments.map((d) => d.modelName),
    );

    const finalModelFilter: string[] = [];
    const aliasMapping: Record<string, string> = {};

    const finalizeAlias = (raw: string | undefined, fallback: string) => {
      const trimmed = (raw ?? "").trim();
      return trimmed.length > 0 ? trimmed : fallback;
    };

    for (const { modelName, alias: serverAlias } of allServerDeployments) {
      if (removedSet.has(modelName)) continue;
      finalModelFilter.push(modelName);
      aliasMapping[modelName] = finalizeAlias(
        aliasEdits[modelName] ?? serverAlias,
        modelName,
      );
    }

    for (const added of modelsState.addedDeployments) {
      if (serverNameSet.has(added.modelName)) continue;
      finalModelFilter.push(added.modelName);
      aliasMapping[added.modelName] = finalizeAlias(added.alias, added.modelName);
    }

    const updateData: EndpointUpdateRequest = {
      name: data.name.trim(),
      url: data.url.trim(),
      ...(data.description?.trim() && { description: data.description.trim() }),
      ...(data.apiKey?.trim() && { api_key: data.apiKey.trim() }),
      // Always send model_filter — passing an empty array means "no models"
      // which matches the user's edits. If the user removed everything, that's
      // intentional.
      model_filter: finalModelFilter,
      alias_mapping: aliasMapping,
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
      pagination.handleClear();
      onSuccess();
      onClose();
    } catch (err: any) {
      if (err.status === 409 || err.response?.status === 409) {
        const responseData = err.response?.data || err.data;

        if (responseData?.resource === "endpoint") {
          form.setError("name", {
            type: "endpoint_name_conflict",
            message: "Endpoint name already exists.",
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
        setSubmitError(err.message || "Failed to update endpoint");
      }
    }
  };

  // Note: we deliberately don't gate on `deploymentsQuery.isLoading` here.
  // The submit handler does its own all-pages fetch to build model_filter,
  // so even if the user clicks before the first page lands, the reconcile
  // still uses authoritative server state.
  const canSave =
    !!form.watch("name")?.trim() &&
    !updateEndpointMutation.isPending &&
    !isBuildingFilter &&
    validationState !== "testing" &&
    (!urlChanged || validationState === "success") &&
    backendConflicts.size === 0 &&
    conflictingAliases.size === 0;

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-2xl max-h-[90vh] overflow-y-auto [&>button]:hidden">
        <DialogHeader>
          <div className="flex items-center justify-between">
            <DialogTitle>Edit Endpoint</DialogTitle>
            <Stepper currentStep={currentStep} />
          </div>
          <DialogDescription>Adjust endpoint settings</DialogDescription>
        </DialogHeader>

        <Form {...form}>
          <form onSubmit={form.handleSubmit(onSubmit)} className="space-y-6">
            {currentStep === 1 && (
              <ConnectionStep
                form={form}
                endpoint={endpoint}
                urlChanged={urlChanged}
                showApiKey={showApiKey}
                setShowApiKey={setShowApiKey}
                advancedPopoverOpen={advancedPopoverOpen}
                setAdvancedPopoverOpen={setAdvancedPopoverOpen}
                handleUrlChange={handleUrlChange}
                validationState={validationState}
                validationError={validationError}
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
                pagination={{
                  page: pagination.page,
                  pageSize: pagination.pageSize,
                  totalItems: totalDeployments,
                  onPageChange: pagination.handlePageChange,
                }}
                isLoading={deploymentsQuery.isLoading || deploymentsQuery.isFetching}
              />
            )}
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
                  !form.watch("url")?.trim() ||
                  validationState === "testing"
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
              <div className="flex-1 flex items-center justify-center text-xs text-gray-500">
                {modelsState.hasChanges && (
                  <ChangeSummary
                    added={modelsState.addedModelNames.length}
                    removed={modelsState.removedModelNames.length}
                    aliasEdits={
                      modelsState.changeCount -
                      modelsState.addedModelNames.length -
                      modelsState.removedModelNames.length
                    }
                  />
                )}
              </div>
              <Button
                onClick={() => form.handleSubmit(onSubmit)()}
                disabled={!canSave}
              >
                {isBuildingFilter ? (
                  <>
                    <Loader2 className="w-4 h-4 mr-2 animate-spin" />
                    Loading deployments...
                  </>
                ) : updateEndpointMutation.isPending ? (
                  <>
                    <Loader2 className="w-4 h-4 mr-2 animate-spin" />
                    Updating...
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
                    Update Endpoint
                  </>
                )}
              </Button>
            </>
          )}
        </DialogFooter>
      </DialogContent>

      <RemoveModelDialog
        modelName={pendingRemoval?.modelName ?? null}
        references={pendingRemoval?.references ?? emptyReferences()}
        onConfirm={confirmRemoval}
        onCancel={() => setPendingRemoval(null)}
      />
    </Dialog>
  );
};

// ---------------------------------------------------------------------------
// Step 1: Connection
// ---------------------------------------------------------------------------

interface ConnectionStepProps {
  form: ReturnType<typeof useForm<FormData>>;
  endpoint: Endpoint;
  urlChanged: boolean;
  showApiKey: boolean;
  setShowApiKey: (v: boolean) => void;
  advancedPopoverOpen: boolean;
  setAdvancedPopoverOpen: (v: boolean) => void;
  handleUrlChange: (url: string) => void;
  validationState: ValidationState;
  validationError: string | null;
}

const ConnectionStep: React.FC<ConnectionStepProps> = ({
  form,
  endpoint,
  urlChanged,
  showApiKey,
  setShowApiKey,
  advancedPopoverOpen,
  setAdvancedPopoverOpen,
  handleUrlChange,
  validationState,
  validationError,
}) => (
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

    <FormField
      control={form.control}
      name="apiKey"
      render={({ field }) => (
        <FormItem>
          <FormLabel>
            API Key (optional)
            {endpoint.requires_api_key && (
              <span className="text-xs text-gray-500 ml-2">
                Leave empty to keep existing key
              </span>
            )}
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
                <p className="text-xs text-gray-500">
                  The HTTP header name provided with upstream requests to this
                  endpoint.
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
                  The prefix before the API key header value. Default is
                  "Bearer " (with trailing space).
                </p>
              </FormItem>
            )}
          />
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
  pagination: {
    page: number;
    pageSize: number;
    totalItems: number;
    onPageChange: (page: number) => void;
  };
  isLoading: boolean;
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
  pagination,
  isLoading,
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
            {(() => {
              // Pending count = (server total - removals) + session adds.
              // Approximate; the authoritative number is computed at submit.
              const pending =
                Math.max(0, pagination.totalItems - modelsState.removedModelNames.length) +
                modelsState.addedDeployments.length;
              return pending === 0
                ? "No models imported yet"
                : `${pending} imported · click an alias to rename`;
            })()}
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
        pagination={pagination}
        isLoading={isLoading}
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
// Tiny presentational pieces
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

const ChangeSummary: React.FC<{
  added: number;
  removed: number;
  aliasEdits: number;
}> = ({ added, removed, aliasEdits }) => {
  const parts: string[] = [];
  if (added) parts.push(`${added} added`);
  if (removed) parts.push(`${removed} removed`);
  if (aliasEdits) parts.push(`${aliasEdits} alias edit${aliasEdits === 1 ? "" : "s"}`);
  return <span>{parts.join(" · ")}</span>;
};

// Avoid a stray unused-import warning when the build picks up only types.
export type { ImportedDeployment };
