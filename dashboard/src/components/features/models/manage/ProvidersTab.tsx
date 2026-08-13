import React, { useState } from "react";
import {
  Plus,
  Edit,
  Trash2,
  Info,
  AlertCircle,
  ToggleLeft,
  ToggleRight,
  GitMerge,
  ArrowDown,
  Shuffle,
  Server,
  GripVertical,
  Shield,
  ShieldOff,
  Zap,
  ZapOff,
} from "lucide-react";
import {
  DndContext,
  closestCenter,
  KeyboardSensor,
  PointerSensor,
  useSensor,
  useSensors,
  type DragEndEvent,
} from "@dnd-kit/core";
import {
  SortableContext,
  sortableKeyboardCoordinates,
  useSortable,
  verticalListSortingStrategy,
} from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";
import { useQueryClient } from "@tanstack/react-query";
import {
  useModelComponents,
  useAddModelComponent,
  useUpdateModelComponent,
  useUpdateModelComponentLayout,
  useRemoveModelComponent,
  useUpdateModel,
  useConfig,
  type Model,
  type ModelComponent,
  type LoadBalancingStrategy,
} from "../../../../api/control-layer";
import { ModelCombobox } from "../../../ui/model-combobox";
import { queryKeys } from "../../../../api/control-layer/keys";
import { ApiError } from "../../../../api/control-layer/errors";
import {
  groupPriorityTiers,
  moveProviderToTier,
  parseRoutingInteger,
} from "./priorityTiers";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "../../../ui/card";
import { Badge } from "../../../ui/badge";
import { Button } from "../../../ui/button";
import {
  HoverCard,
  HoverCardContent,
  HoverCardTrigger,
} from "../../../ui/hover-card";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "../../../ui/dialog";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../../ui/select";
import { Input } from "../../../ui/input";
import { Switch } from "../../../ui/switch";
import { Label } from "../../../ui/label";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "../../../ui/tooltip";
import { buildFallbackUpdatePayload } from "../../../../utils/fallbackStatusCodes";

interface ProvidersTabProps {
  model: Model;
  canManage: boolean;
}

// Component for displaying a single provider row
const ProviderRow: React.FC<{
  component: ModelComponent;
  totalWeight: number;
  priorityIndex?: number; // 1-based index for priority mode
  isPriorityMode: boolean;
  isLast?: boolean; // For priority mode arrow display
  onEdit: () => void;
  onRemove: () => void;
  onToggle: () => void;
  onToggleTrusted?: () => void;
  onToggleAdapter?: () => void;
  canManage: boolean;
  isUpdating: boolean;
  strictModeEnabled?: boolean;
  dragHandleProps?: {
    attributes: React.HTMLAttributes<HTMLElement>;
    listeners: Record<string, unknown> | undefined;
  };
  isDragging?: boolean;
  isAnyDragging?: boolean;
}> = ({
  component,
  totalWeight,
  priorityIndex,
  isPriorityMode,
  isLast,
  onEdit,
  onRemove,
  onToggle,
  onToggleTrusted,
  onToggleAdapter,
  canManage,
  isUpdating,
  strictModeEnabled,
  dragHandleProps,
  isDragging,
  isAnyDragging,
}) => {
  // Disabled components get 0% traffic, enabled ones share proportionally
  const percentage =
    component.enabled && totalWeight > 0
      ? ((component.weight / totalWeight) * 100).toFixed(1)
      : 0;

  return (
    <div className={`flex flex-col group ${isDragging ? "opacity-90" : ""}`}>
      <div className="flex items-center">
        {/* Priority cascade indicator */}
        {isPriorityMode && (
          <div className="flex items-center justify-center mr-4 w-8">
            <div
              className={`w-8 h-8 rounded-full flex items-center justify-center text-sm font-medium transition-all
                ${canManage && dragHandleProps ? "cursor-grab active:cursor-grabbing" : ""}
                ${isDragging ? "shadow-lg scale-110 ring-2 ring-blue-400" : ""}
                ${
                  priorityIndex === 1
                    ? "bg-blue-100 text-blue-700"
                    : "bg-gray-100 text-gray-600"
                }`}
              {...(canManage && dragHandleProps ? dragHandleProps.attributes : {})}
              {...(canManage && dragHandleProps ? dragHandleProps.listeners : {})}
              aria-label={
                canManage && dragHandleProps
                  ? `Move ${component.model.alias}, currently in tier ${priorityIndex}`
                  : undefined
              }
              title={
                canManage && dragHandleProps
                  ? `Move ${component.model.alias}, currently in tier ${priorityIndex}`
                  : undefined
              }
            >
              {/* Show number by default, grip icon on hover when draggable */}
              {canManage && dragHandleProps ? (
                <>
                  <span className="group-hover:hidden">{priorityIndex}</span>
                  <GripVertical className="h-4 w-4 hidden group-hover:block" />
                </>
              ) : (
                priorityIndex
              )}
            </div>
          </div>
        )}

        <div
          className={`flex-1 min-w-0 flex items-center justify-between p-4 rounded-lg border overflow-hidden transition-shadow ${
            isDragging ? "shadow-lg border-blue-300" : ""
          } ${
            component.enabled
              ? "bg-white border-gray-200"
              : "bg-gray-50 border-gray-200 opacity-60"
          }`}
        >
          <div className="flex items-center gap-4 min-w-0 flex-1 overflow-hidden">
            <div className="min-w-0 flex-1 overflow-hidden">
              <div className="flex items-center gap-2 min-w-0">
                <p className="font-medium text-gray-900 truncate min-w-0 flex-shrink">
                  {component.model.alias}
                </p>
                {isPriorityMode && priorityIndex === 1 && component.enabled && (
                  <Badge
                    variant="outline"
                    className="text-xs text-blue-600 border-blue-200 bg-blue-50 shrink-0"
                  >
                    Primary
                  </Badge>
                )}
                {!component.enabled && (
                  <Badge
                    variant="outline"
                    className="text-xs text-gray-500 shrink-0"
                  >
                    Disabled
                  </Badge>
                )}
              </div>
              {component.model.endpoint?.name && (
                <div className="flex items-center gap-1 mt-0.5 min-w-0">
                  <Server className="h-3 w-3 text-gray-400 shrink-0" />
                  <p className="text-sm text-gray-500 truncate min-w-0">
                    {component.model.endpoint.name}
                  </p>
                </div>
              )}
            </div>

            {/* Weight is global in weighted mode and relative within a priority tier. */}
            <div className="flex items-center gap-3 shrink-0">
                <div className="text-right">
                  <p className="font-medium text-gray-900">
                    {component.weight}
                  </p>
                  <p className="text-xs text-gray-500">{percentage}%</p>
                </div>
                <div className="w-20 h-2 bg-gray-200 rounded-full overflow-hidden">
                  <div
                    className="h-full bg-blue-500 rounded-full transition-all duration-300"
                    style={{ width: `${percentage}%` }}
                  />
                </div>
            </div>
          </div>

          {canManage && (
            <div className="flex items-center gap-1 ml-4">
              <Button
                variant="ghost"
                size="icon"
                onClick={onToggle}
                disabled={isUpdating}
                className="h-8 w-8"
                title={
                  component.enabled
                    ? "Disable hosted model"
                    : "Enable hosted model"
                }
              >
                {component.enabled ? (
                  <ToggleRight className="h-4 w-4 text-green-600" />
                ) : (
                  <ToggleLeft className="h-4 w-4 text-gray-400" />
                )}
              </Button>
              {strictModeEnabled && onToggleTrusted && (
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Button
                      variant="ghost"
                      size="icon"
                      onClick={onToggleTrusted}
                      disabled={isUpdating}
                      className="h-8 w-8"
                    >
                      {component.model.trusted ? (
                        <Shield className="h-4 w-4 text-blue-600 fill-blue-100" />
                      ) : (
                        <ShieldOff className="h-4 w-4 text-gray-400" />
                      )}
                    </Button>
                  </TooltipTrigger>
                  <TooltipContent side="top">
                    {component.model.trusted
                      ? "Trusted — bypasses error sanitization"
                      : "Untrusted — errors are sanitized"}
                  </TooltipContent>
                </Tooltip>
              )}
              {strictModeEnabled && onToggleAdapter && (
                <Tooltip>
                  <TooltipTrigger asChild>
                    <Button
                      variant="ghost"
                      size="icon"
                      onClick={onToggleAdapter}
                      disabled={isUpdating}
                      className="h-8 w-8"
                    >
                      {(component.model.open_responses_adapter ?? true) ? (
                        <Zap className="h-4 w-4 text-yellow-500 fill-yellow-100" />
                      ) : (
                        <ZapOff className="h-4 w-4 text-gray-400" />
                      )}
                    </Button>
                  </TooltipTrigger>
                  <TooltipContent side="top">
                    {(component.model.open_responses_adapter ?? true)
                      ? "Responses API adapter enabled"
                      : "Responses API adapter disabled"}
                  </TooltipContent>
                </Tooltip>
              )}
              <Button
                variant="ghost"
                size="icon"
                onClick={onEdit}
                disabled={isUpdating}
                className="h-8 w-8"
                title={
                  isPriorityMode
                    ? `Edit tier and weight for ${component.model.alias}`
                    : `Edit weight for ${component.model.alias}`
                }
                aria-label={
                  isPriorityMode
                    ? `Edit tier and weight for ${component.model.alias}`
                    : `Edit weight for ${component.model.alias}`
                }
              >
                <Edit className="h-4 w-4" />
              </Button>
              <Button
                variant="ghost"
                size="icon"
                onClick={onRemove}
                disabled={isUpdating}
                className="h-8 w-8 text-red-600 hover:text-red-700 hover:bg-red-50"
                title="Remove hosted model"
              >
                <Trash2 className="h-4 w-4" />
              </Button>
            </div>
          )}
        </div>
      </div>

      {/* Arrow connector to next item - extends behind cards with negative margins, hidden when any item is dragging */}
      {isPriorityMode && !isLast && !isAnyDragging && (
        <div className="flex h-12 -my-4">
          <div className="flex justify-center w-8 mr-4">
            <div className="flex flex-col items-center h-full">
              <div className="w-px flex-1 bg-gray-300" />
              <ArrowDown className="h-3 w-3 text-gray-400 shrink-0 bg-white" />
              <div className="w-px flex-1 bg-gray-300" />
            </div>
          </div>
        </div>
      )}
    </div>
  );
};

// Sortable wrapper for ProviderRow
const SortableProviderRow: React.FC<{
  id: string;
  component: ModelComponent;
  totalWeight: number;
  priorityIndex?: number;
  isPriorityMode: boolean;
  isLast?: boolean;
  onEdit: () => void;
  onRemove: () => void;
  onToggle: () => void;
  onToggleTrusted?: () => void;
  onToggleAdapter?: () => void;
  canManage: boolean;
  isUpdating: boolean;
  strictModeEnabled?: boolean;
  isAnyDragging?: boolean;
}> = (props) => {
  const {
    attributes,
    listeners,
    setNodeRef,
    transform,
    transition,
    isDragging,
  } = useSortable({ id: props.id });

  // Use Translate instead of Transform to avoid skewing
  const style: React.CSSProperties = {
    transform: CSS.Translate.toString(transform),
    transition,
    zIndex: isDragging ? 50 : undefined,
    position: isDragging ? "relative" : undefined,
  };

  return (
    <div ref={setNodeRef} style={style}>
      <ProviderRow
        {...props}
        dragHandleProps={{ attributes, listeners }}
        isDragging={isDragging}
        isAnyDragging={props.isAnyDragging}
      />
    </div>
  );
};

// Add Provider Modal
const AddProviderModal: React.FC<{
  open: boolean;
  onClose: () => void;
  modelId: string;
  existingComponentIds: string[];
  existingComponents: ModelComponent[];
  isPriorityMode: boolean;
  strictModeEnabled: boolean;
}> = ({
  open,
  onClose,
  modelId,
  existingComponentIds,
  existingComponents,
  isPriorityMode,
  strictModeEnabled,
}) => {
  const [selectedModel, setSelectedModel] = useState<Model | null>(null);
  const [weight, setWeight] = useState<string>("50");
  const [priorityTier, setPriorityTier] = useState<string>(() => {
    const maxTier = Math.max(-1, ...existingComponents.map((item) => item.sort_order));
    return String(maxTier + 2);
  });
  const [trusted, setTrusted] = useState<string>("untrusted");

  const addMutation = useAddModelComponent();
  const updateModelMutation = useUpdateModel();
  const parsedWeight = parseRoutingInteger(weight, 1, 100);
  const parsedPriorityTier = parseRoutingInteger(priorityTier, 1, 10000);

  React.useEffect(() => {
    if (open && isPriorityMode) {
      const maxTier = Math.max(
        -1,
        ...existingComponents.map((item) => item.sort_order),
      );
      setPriorityTier(String(maxTier + 2));
    }
  }, [existingComponents, isPriorityMode, open]);

  // Excludes composite models (a virtual model can't include another virtual
  // as a component) and any models already added to this composite. The
  // combobox handles searching/filtering across the full model list.
  const filterAvailableModels = React.useCallback(
    (m: Model) => !m.is_composite && !existingComponentIds.includes(m.id),
    [existingComponentIds],
  );

  const handleSubmit = async () => {
    if (
      !selectedModel ||
      parsedWeight === null ||
      (isPriorityMode && parsedPriorityTier === null)
    ) {
      return;
    }

    try {
      await addMutation.mutateAsync({
        modelId,
        data: {
          deployed_model_id: selectedModel.id,
          weight: parsedWeight,
          ...(isPriorityMode
            ? { sort_order: (parsedPriorityTier ?? 1) - 1 }
            : {}),
        },
      });
      // Set trusted flag if strict mode is enabled
      if (strictModeEnabled) {
        await updateModelMutation.mutateAsync({
          id: selectedModel.id,
          data: { trusted: trusted === "trusted" },
        });
      }
      onClose();
      setSelectedModel(null);
      setWeight("50");
      setPriorityTier("1");
      setTrusted("untrusted");
    } catch {
      // Error handled by mutation
    }
  };

  const handleClose = () => {
    onClose();
    setSelectedModel(null);
    setWeight("50");
    setTrusted("untrusted");
  };

  return (
    <Dialog open={open} onOpenChange={handleClose}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Add Hosted Model</DialogTitle>
          <DialogDescription>
            Select a hosted model to add to this virtual model.
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-4 py-4">
          <div className="space-y-2">
            <label className="text-sm font-medium text-gray-700">
              Select Model
            </label>
            <ModelCombobox
              value={selectedModel}
              onValueChange={setSelectedModel}
              placeholder="Choose a hosted model…"
              searchPlaceholder="Search by alias, model name, or endpoint…"
              emptyMessage="No matching hosted models. (Already-added or virtual models are hidden.)"
              filterFn={filterAvailableModels}
              queryOptions={{ accessible: false, is_composite: false }}
              className="w-full"
            />
          </div>

          {isPriorityMode && (
            <div className="space-y-2">
              <label
                htmlFor="new-provider-tier"
                className="text-sm font-medium text-gray-700"
              >
                Priority tier
              </label>
              <Input
                id="new-provider-tier"
                type="number"
                min="1"
                max="10000"
                value={priorityTier}
                onChange={(event) => setPriorityTier(event.target.value)}
              />
              <p className="text-xs text-gray-500">
                Providers in the same tier share traffic by weighted least
                connections.
              </p>
            </div>
          )}

          {/* Weight applies globally or within a priority tier. */}
          <div className="space-y-2">
              <div className="flex items-center gap-1">
                <label
                  htmlFor="new-provider-weight"
                  className="text-sm font-medium text-gray-700"
                >
                  Weight
                </label>
                <HoverCard openDelay={100} closeDelay={50}>
                  <HoverCardTrigger asChild>
                    <Info className="h-3 w-3 text-gray-400 hover:text-gray-600" />
                  </HoverCardTrigger>
                  <HoverCardContent className="w-64" sideOffset={5}>
                    <p className="text-sm text-muted-foreground">
                      Weight determines the proportion of traffic this hosted
                      model receives relative to others.
                    </p>
                  </HoverCardContent>
                </HoverCard>
              </div>
              <Input
                id="new-provider-weight"
                type="number"
                min="1"
                max="100"
                value={weight}
                onChange={(e) => setWeight(e.target.value)}
                placeholder="1-100"
              />
              <p className="text-xs text-gray-500">Value from 1 to 100.</p>
          </div>

          {/* Trust level - only in strict mode */}
          {strictModeEnabled && (
            <div className="space-y-2">
              <div className="flex items-center gap-1">
                <label className="text-sm font-medium text-gray-700">
                  Trust Level
                </label>
                <HoverCard openDelay={100} closeDelay={50}>
                  <HoverCardTrigger asChild>
                    <Info className="h-3 w-3 text-gray-400 hover:text-gray-600 cursor-help" />
                  </HoverCardTrigger>
                  <HoverCardContent className="w-64" sideOffset={5}>
                    <p className="text-sm text-muted-foreground">
                      Trusted providers bypass error sanitization, returning full
                      error details from the upstream provider. Untrusted
                      providers have their errors sanitized.
                    </p>
                  </HoverCardContent>
                </HoverCard>
              </div>
              <Select value={trusted} onValueChange={setTrusted}>
                <SelectTrigger>
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="untrusted">
                    <div className="flex items-center gap-2">
                      <ShieldOff className="h-4 w-4 text-gray-400" />
                      <span>Untrusted</span>
                    </div>
                  </SelectItem>
                  <SelectItem value="trusted">
                    <div className="flex items-center gap-2">
                      <Shield className="h-4 w-4 text-blue-600" />
                      <span>Trusted</span>
                    </div>
                  </SelectItem>
                </SelectContent>
              </Select>
            </div>
          )}
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={handleClose}>
            Cancel
          </Button>
          <Button
            onClick={handleSubmit}
            disabled={
              !selectedModel ||
              addMutation.isPending ||
              parsedWeight === null ||
              (isPriorityMode && parsedPriorityTier === null)
            }
          >
            {addMutation.isPending ? (
              <>
                <div className="w-4 h-4 border-2 border-white border-t-transparent rounded-full animate-spin mr-2" />
                Adding...
              </>
            ) : (
              "Add Hosted Model"
            )}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};

// Edit Weight Modal
const EditWeightModal: React.FC<{
  open: boolean;
  onClose: () => void;
  component: ModelComponent | null;
  modelId: string;
  components: ModelComponent[];
  isPriorityMode: boolean;
}> = ({ open, onClose, component, modelId, components, isPriorityMode }) => {
  const [weight, setWeight] = useState<string>(
    component?.weight?.toString() || "50",
  );
  const [priorityTier, setPriorityTier] = useState<string>(
    String((component?.sort_order ?? 0) + 1),
  );

  const updateComponentMutation = useUpdateModelComponent();
  const updateLayoutMutation = useUpdateModelComponentLayout();
  const parsedWeight = parseRoutingInteger(weight, 1, 100);
  const parsedPriorityTier = parseRoutingInteger(priorityTier, 1, 10000);

  // Update weight when component changes
  React.useEffect(() => {
    if (component) {
      setWeight(component.weight.toString());
      setPriorityTier(String(component.sort_order + 1));
    }
  }, [component]);

  const handleSubmit = async () => {
    if (
      !component ||
      parsedWeight === null ||
      (isPriorityMode && parsedPriorityTier === null)
    ) {
      return;
    }

    try {
      if (isPriorityMode) {
        await updateLayoutMutation.mutateAsync({
          modelId,
          components: components.map((item) =>
            item.model.id === component.model.id
              ? {
                  ...item,
                  weight: parsedWeight,
                  sort_order: (parsedPriorityTier ?? 1) - 1,
                }
              : item,
          ),
        });
      } else {
        await updateComponentMutation.mutateAsync({
          modelId,
          componentModelId: component.model.id,
          data: { weight: parsedWeight },
        });
      }
      onClose();
    } catch (error) {
      // A conflict means the component set changed while the dialog was open.
      // Close the stale editor; the mutation refreshes the component query.
      if (error instanceof ApiError && error.status === 409) {
        onClose();
      }
    }
  };

  return (
    <Dialog open={open} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-sm">
        <DialogHeader>
          <DialogTitle>Edit Routing</DialogTitle>
          <DialogDescription>
            Adjust the weight for{" "}
            {component?.model.alias || component?.model.id}
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-4 py-4">
          {isPriorityMode && (
            <div className="space-y-2">
              <label
                htmlFor="edit-provider-tier"
                className="text-sm font-medium text-gray-700"
              >
                Priority tier
              </label>
              <Input
                id="edit-provider-tier"
                type="number"
                min="1"
                max="10000"
                value={priorityTier}
                onChange={(event) => setPriorityTier(event.target.value)}
              />
              <p className="text-xs text-gray-500">
                Use the same number to pool providers in one tier.
              </p>
            </div>
          )}
          <div className="space-y-2">
            <div className="flex items-center gap-1">
              <label
                htmlFor="edit-provider-weight"
                className="text-sm font-medium text-gray-700"
              >
                Weight
              </label>
              <HoverCard openDelay={100} closeDelay={50}>
                <HoverCardTrigger asChild>
                  <Info className="h-3 w-3 text-gray-400 hover:text-gray-600" />
                </HoverCardTrigger>
                <HoverCardContent className="w-64" sideOffset={5}>
                  <p className="text-sm text-muted-foreground">
                    Weight determines the proportion of traffic this hosted
                    model receives relative to others.
                  </p>
                </HoverCardContent>
              </HoverCard>
            </div>
            <Input
              id="edit-provider-weight"
              type="number"
              min="1"
              max="100"
              value={weight}
              onChange={(e) => setWeight(e.target.value)}
              placeholder="1-100"
            />
            <p className="text-xs text-gray-500">Value from 1 to 100.</p>
          </div>
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={onClose}>
            Cancel
          </Button>
          <Button
            onClick={handleSubmit}
            disabled={
              updateComponentMutation.isPending ||
              updateLayoutMutation.isPending ||
              parsedWeight === null ||
              (isPriorityMode && parsedPriorityTier === null)
            }
          >
            {updateComponentMutation.isPending || updateLayoutMutation.isPending ? (
              <>
                <div className="w-4 h-4 border-2 border-white border-t-transparent rounded-full animate-spin mr-2" />
                Saving...
              </>
            ) : (
              "Save"
            )}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};

// Confirm Remove Dialog
const ConfirmRemoveDialog: React.FC<{
  open: boolean;
  onClose: () => void;
  onConfirm: () => void;
  component: ModelComponent | null;
  isRemoving: boolean;
}> = ({ open, onClose, onConfirm, component, isRemoving }) => {
  return (
    <Dialog open={open} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-sm">
        <DialogHeader>
          <DialogTitle>Remove Hosted Model</DialogTitle>
          <DialogDescription>
            Are you sure you want to remove{" "}
            <strong>{component?.model.alias || component?.model.id}</strong>{" "}
            from this virtual model?
          </DialogDescription>
        </DialogHeader>

        <DialogFooter>
          <Button variant="outline" onClick={onClose} disabled={isRemoving}>
            Cancel
          </Button>
          <Button
            variant="destructive"
            onClick={onConfirm}
            disabled={isRemoving}
          >
            {isRemoving ? (
              <>
                <div className="w-4 h-4 border-2 border-white border-t-transparent rounded-full animate-spin mr-2" />
                Removing...
              </>
            ) : (
              "Remove"
            )}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};

// Edit Routing Configuration Modal
const EditRoutingModal: React.FC<{
  open: boolean;
  onClose: () => void;
  model: Model;
}> = ({ open, onClose, model }) => {
  const [strategy, setStrategy] = useState<LoadBalancingStrategy>(
    model.lb_strategy || "weighted_random",
  );
  const [fallbackEnabled, setFallbackEnabled] = useState(
    model.fallback?.enabled ?? false,
  );
  const [fallbackOnRateLimit, setFallbackOnRateLimit] = useState(
    model.fallback?.on_rate_limit ?? false,
  );
  const [fallbackOn429, setFallbackOn429] = useState(
    model.fallback?.on_status?.includes(429) ?? false,
  );
  const [fallbackOn499, setFallbackOn499] = useState(
    model.fallback?.on_status?.includes(499) ?? false,
  );
  const [fallbackOn404, setFallbackOn404] = useState(
    model.fallback?.on_status?.includes(404) ?? false,
  );
  const [fallbackOn5xx, setFallbackOn5xx] = useState(
    model.fallback?.on_status?.some((s) => s >= 500 && s < 600) ?? false,
  );
  const [withReplacement, setWithReplacement] = useState(
    model.fallback?.with_replacement ?? false
  );
  const [maxAttempts, setMaxAttempts] = useState<number | null>(
    model.fallback?.max_attempts ?? null
  );

  const updateMutation = useUpdateModel();

  // Reset form when model changes
  React.useEffect(() => {
    setStrategy(model.lb_strategy || "weighted_random");
    setFallbackEnabled(model.fallback?.enabled ?? false);
    setFallbackOnRateLimit(model.fallback?.on_rate_limit ?? false);
    setFallbackOn429(model.fallback?.on_status?.includes(429) ?? false);
    setFallbackOn499(model.fallback?.on_status?.includes(499) ?? false);
    setFallbackOn404(model.fallback?.on_status?.includes(404) ?? false);
    setFallbackOn5xx(
      model.fallback?.on_status?.some((s) => s >= 500 && s < 600) ?? false,
    );
    setWithReplacement(model.fallback?.with_replacement ?? false);
    setMaxAttempts(model.fallback?.max_attempts ?? null);
  }, [model]);

  const handleSubmit = async () => {
    const fallbackUpdate = buildFallbackUpdatePayload({
      fallbackEnabled,
      fallbackOnRateLimit,
      originalStatuses: model.fallback?.on_status ?? [],
      on429: fallbackOn429,
      on499: fallbackOn499,
      on404: fallbackOn404,
      on5xx: fallbackOn5xx,
      withReplacement,
      maxAttempts,
    });

    try {
      await updateMutation.mutateAsync({
        id: model.id,
        data: {
          lb_strategy: strategy,
          ...fallbackUpdate,
        },
      });
      onClose();
    } catch {
      // Error handled by mutation
    }
  };

  return (
    <Dialog open={open} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle>Routing Configuration</DialogTitle>
          <DialogDescription>
            Configure how requests are distributed across hosted models.
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-6 py-4">
          {/* Load Balancing Strategy */}
          <div className="space-y-3">
            <Label className="text-sm font-medium">
              Load Balancing Strategy
            </Label>
            <Select
              value={strategy}
              onValueChange={(v) => setStrategy(v as LoadBalancingStrategy)}
            >
              <SelectTrigger>
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="weighted_random">
                  <div className="flex items-center gap-2">
                    <Shuffle className="h-4 w-4" />
                    <span>Weighted Distribution</span>
                  </div>
                </SelectItem>
                <SelectItem value="priority">
                  <div className="flex items-center gap-2">
                    <ArrowDown className="h-4 w-4" />
                    <span>Priority Failover</span>
                  </div>
                </SelectItem>
              </SelectContent>
            </Select>
            <p className="text-xs text-gray-500">
              {strategy === "weighted_random"
                ? "Requests use weighted least connections across hosted models."
                : "Requests go to the highest-priority hosted model; others are fallbacks."}
            </p>
          </div>

          {/* Automatic Failover Toggle */}
          <div className="space-y-4">
            <div className="flex items-center justify-between">
              <div>
                <Label className="text-sm font-medium">
                  Automatic Failover
                </Label>
                <p className="text-xs text-gray-500">
                  {strategy === "priority"
                    ? "Exhaust the current tier, then try the next tier on failure"
                    : "Select from remaining hosted models on failure"}
                </p>
              </div>
              <Switch
                checked={fallbackEnabled}
                onCheckedChange={setFallbackEnabled}
              />
            </div>

            {/* Fallback Triggers - only shown when enabled */}
            {fallbackEnabled && (
              <div className="pl-4 border-l-2 border-gray-200 space-y-3">
                <p className="text-xs text-gray-500 uppercase tracking-wide">
                  Failover on:
                </p>

                <div className="flex items-center justify-between">
                  <div>
                    <Label className="text-sm">Gateway rate limit</Label>
                    <p className="text-xs text-gray-500">
                      When this hosted model's RPS or concurrency limits are
                      exceeded
                    </p>
                  </div>
                  <Switch
                    checked={fallbackOnRateLimit}
                    onCheckedChange={setFallbackOnRateLimit}
                  />
                </div>

                <div className="flex items-center justify-between">
                  <div>
                    <Label className="text-sm">Provider rate limit (429)</Label>
                    <p className="text-xs text-gray-500">
                      When the provider returns rate limit errors
                    </p>
                  </div>
                  <Switch
                    checked={fallbackOn429}
                    onCheckedChange={setFallbackOn429}
                  />
                </div>

                <div className="flex items-center justify-between">
                  <div>
                    <Label className="text-sm">Request cancelled (499)</Label>
                    <p className="text-xs text-gray-500">
                      When the provider cancels an in-flight request
                    </p>
                  </div>
                  <Switch
                    checked={fallbackOn499}
                    onCheckedChange={setFallbackOn499}
                  />
                </div>

                <div className="flex items-center justify-between">
                  <div>
                    <Label className="text-sm">Model unavailable (404)</Label>
                    <p className="text-xs text-gray-500">
                      When the provider returns 404 — e.g. a self-hosted model
                      that isn't currently live
                    </p>
                  </div>
                  <Switch
                    checked={fallbackOn404}
                    onCheckedChange={setFallbackOn404}
                  />
                </div>

                <div className="flex items-center justify-between">
                  <div>
                    <Label className="text-sm">Server errors (5xx)</Label>
                    <p className="text-xs text-gray-500">
                      When the provider returns server errors
                    </p>
                  </div>
                  <Switch
                    checked={fallbackOn5xx}
                    onCheckedChange={setFallbackOn5xx}
                  />
                </div>

                {strategy === "weighted_random" && (
                  <>
                    <div className="border-t pt-3 mt-1" />

                    <div className="flex items-center justify-between">
                      <div>
                        <Label className="text-sm">Sample with replacement</Label>
                        <p className="text-xs text-gray-500">
                          Allow retrying the same provider, weighted by probability
                        </p>
                      </div>
                      <Switch
                        checked={withReplacement}
                        onCheckedChange={setWithReplacement}
                      />
                    </div>

                    <div>
                      <Label className="text-sm">Max failover attempts</Label>
                      <p className="text-xs text-gray-500 mb-1">
                        Defaults to the number of hosted models
                      </p>
                      <Input
                        type="number"
                        min={1}
                        value={maxAttempts ?? ""}
                        onChange={(e) => {
                          const val = e.target.value;
                          if (val === "") {
                            setMaxAttempts(null);
                          } else {
                            const num = Number(val);
                            if (Number.isFinite(num) && num >= 1) {
                              setMaxAttempts(Math.floor(num));
                            }
                          }
                        }}
                        placeholder="Default"
                        className="w-24"
                      />
                    </div>
                  </>
                )}
              </div>
            )}
          </div>
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={onClose}>
            Cancel
          </Button>
          <Button onClick={handleSubmit} disabled={updateMutation.isPending}>
            {updateMutation.isPending ? (
              <>
                <div className="w-4 h-4 border-2 border-white border-t-transparent rounded-full animate-spin mr-2" />
                Saving...
              </>
            ) : (
              "Save Changes"
            )}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};

export const ProvidersTab: React.FC<ProvidersTabProps> = ({
  model,
  canManage,
}) => {
  const [showAddModal, setShowAddModal] = useState(false);
  const [showRoutingModal, setShowRoutingModal] = useState(false);
  const [editingComponent, setEditingComponent] =
    useState<ModelComponent | null>(null);
  const [removingComponent, setRemovingComponent] =
    useState<ModelComponent | null>(null);
  const [isAnyDragging, setIsAnyDragging] = useState(false);

  const queryClient = useQueryClient();

  const {
    data: components,
    isLoading,
    error,
  } = useModelComponents(model.id, {
    enabled: model.is_composite === true,
  });

  const updateComponentMutation = useUpdateModelComponent();
  const updateLayoutMutation = useUpdateModelComponentLayout();
  const removeMutation = useRemoveModelComponent();
  const updateModelMutation = useUpdateModel();

  const { data: configData } = useConfig();
  const strictModeEnabled = configData?.onwards?.strict_mode ?? false;

  const isPriorityMode = model.lb_strategy === "priority";

  const totalWeight =
    components?.reduce((sum, c) => (c.enabled ? sum + c.weight : sum), 0) || 0;

  const priorityTierWeights = React.useMemo(() => {
    const weights = new Map<number, number>();
    for (const component of components ?? []) {
      if (component.enabled) {
        weights.set(
          component.sort_order,
          (weights.get(component.sort_order) ?? 0) + component.weight,
        );
      }
    }
    return weights;
  }, [components]);

  // Sort components by sort_order asc for priority mode display
  const sortedComponents = React.useMemo(() => {
    if (!components) return [];
    if (isPriorityMode) {
      return groupPriorityTiers(components).flatMap((tier) => tier.providers);
    }
    return components;
  }, [components, isPriorityMode]);

  // Drag and drop sensors
  const sensors = useSensors(
    useSensor(PointerSensor, {
      activationConstraint: {
        distance: 8,
      },
    }),
    useSensor(KeyboardSensor, {
      coordinateGetter: sortableKeyboardCoordinates,
    }),
  );

  // Handle drag end - update sort_order to reflect new order
  const handleDragEnd = async (event: DragEndEvent) => {
    const { active, over } = event;

    if (
      !over ||
      active.id === over.id ||
      !sortedComponents.length ||
      !components
    ) {
      return;
    }

    const oldIndex = sortedComponents.findIndex(
      (c) => c.model.id === active.id,
    );
    const newIndex = sortedComponents.findIndex((c) => c.model.id === over.id);

    if (oldIndex === -1 || newIndex === -1) return;

    const targetTier = sortedComponents[newIndex].sort_order;

    // Optimistically update the cache immediately for smooth UX
    const queryKey = queryKeys.models.components(model.id);
    const previousComponents =
      queryClient.getQueryData<ModelComponent[]>(queryKey);

    const optimisticComponents = moveProviderToTier(
      components,
      String(active.id),
      targetTier,
    );

    queryClient.setQueryData(queryKey, optimisticComponents);

    try {
      await updateLayoutMutation.mutateAsync({
        modelId: model.id,
        components: optimisticComponents,
      });
    } catch (error) {
      if (error instanceof ApiError && error.status === 409) {
        // Never restore the stale optimistic snapshot after an exact-set
        // conflict. Refetch the server's current component set instead.
        await queryClient.invalidateQueries({ queryKey });
      } else {
        queryClient.setQueryData(queryKey, previousComponents);
      }
    }
  };

  const handleToggle = async (component: ModelComponent) => {
    await updateComponentMutation.mutateAsync({
      modelId: model.id,
      componentModelId: component.model.id,
      data: { enabled: !component.enabled },
    });
  };

  const handleToggleTrusted = async (component: ModelComponent) => {
    await updateModelMutation.mutateAsync({
      id: component.model.id,
      data: { trusted: !component.model.trusted },
    });
  };

  const handleToggleAdapter = async (component: ModelComponent) => {
    await updateModelMutation.mutateAsync({
      id: component.model.id,
      data: { open_responses_adapter: !(component.model.open_responses_adapter ?? true) },
    });
  };

  const handleRemove = async () => {
    if (!removingComponent) return;

    try {
      await removeMutation.mutateAsync({
        modelId: model.id,
        componentModelId: removingComponent.model.id,
      });
      setRemovingComponent(null);
    } catch {
      // Error handled by mutation
    }
  };

  if (!model.is_composite) {
    return (
      <Card className="p-0 gap-0 rounded-lg">
        <CardContent className="px-6 py-12 text-center">
          <p className="text-gray-500">
            This is not a virtual model. Hosted models are only available for
            virtual models.
          </p>
        </CardContent>
      </Card>
    );
  }

  if (isLoading) {
    return (
      <Card className="p-0 gap-0 rounded-lg">
        <CardContent className="px-6 py-12">
          <div className="flex items-center justify-center">
            <div
              className="animate-spin rounded-full h-8 w-8 border-b-2 border-gray-600"
              aria-label="Loading"
            />
          </div>
        </CardContent>
      </Card>
    );
  }

  if (error) {
    return (
      <Card className="p-0 gap-0 rounded-lg">
        <CardContent className="px-6 py-12 text-center">
          <AlertCircle className="h-8 w-8 text-red-500 mx-auto mb-2" />
          <p className="text-red-600">Failed to load hosted models</p>
          <p className="text-sm text-gray-500 mt-1">
            {(error as Error).message}
          </p>
        </CardContent>
      </Card>
    );
  }

  // Build fallback triggers list
  const fallbackTriggers: string[] = [];
  if (model.fallback?.enabled) {
    // Gateway rate limit = when this gateway's configured rate limits are hit
    if (model.fallback.on_rate_limit) {
      fallbackTriggers.push("Gateway rate limit exceeded");
    }
    if (model.fallback.on_status && model.fallback.on_status.length > 0) {
      const has429 = model.fallback.on_status.includes(429);
      const has499 = model.fallback.on_status.includes(499);
      const has404 = model.fallback.on_status.includes(404);
      const serverErrors = model.fallback.on_status.filter((s) => s >= 500);
      const otherErrors = model.fallback.on_status.filter(
        (s) => s < 500 && s !== 429 && s !== 499 && s !== 404,
      );
      // Upstream 429 = when the provider returns rate limit errors
      if (has429) {
        fallbackTriggers.push("Provider rate limit (429)");
      }
      if (has499) {
        fallbackTriggers.push("Request cancelled (499)");
      }
      // Upstream 404 = e.g. a self-hosted model that isn't currently live
      if (has404) {
        fallbackTriggers.push("Model unavailable (404)");
      }
      if (serverErrors.length > 0) {
        fallbackTriggers.push(`Server errors (${serverErrors.join(", ")})`);
      }
      if (otherErrors.length > 0) {
        fallbackTriggers.push(`Status codes: ${otherErrors.join(", ")}`);
      }
    }
  }

  return (
    <>
      <div className="space-y-6">
        {/* Routing Strategy Card */}
        <Card className="p-0 gap-0 rounded-lg">
          <CardHeader className="px-6 pt-5 pb-4">
            <div className="flex items-center justify-between">
              <div>
                <CardTitle className="flex items-center gap-2">
                  {isPriorityMode ? (
                    <ArrowDown className="h-5 w-5" />
                  ) : (
                    <Shuffle className="h-5 w-5" />
                  )}
                  {isPriorityMode
                    ? "Priority Failover"
                    : "Weighted Distribution"}
                </CardTitle>
                <CardDescription className="line-clamp-2">
                  {isPriorityMode
                    ? "Requests use weighted least connections within the highest available tier, then fail over to lower tiers."
                    : "Requests use weighted least connections across providers."}
                </CardDescription>
              </div>
              {canManage && (
                <Button
                  variant="outline"
                  size="sm"
                  onClick={() => setShowRoutingModal(true)}
                  className="gap-1"
                >
                  <Edit className="h-4 w-4" />
                  Configure
                </Button>
              )}
            </div>
          </CardHeader>
          <CardContent className="px-6 pb-6 pt-0">
            <div className="space-y-4">
              {/* Fallback Configuration */}
              <div className="p-4 bg-gray-50 rounded-lg">
                <div className="flex items-center gap-2 mb-2">
                  <GitMerge className="h-4 w-4 text-gray-500" />
                  <p className="text-sm font-medium text-gray-700">
                    Automatic Failover
                  </p>
                  <HoverCard openDelay={100} closeDelay={50}>
                    <HoverCardTrigger asChild>
                      <Info className="h-3 w-3 text-gray-400 hover:text-gray-600 cursor-help" />
                    </HoverCardTrigger>
                    <HoverCardContent className="w-80" sideOffset={5}>
                      <div className="space-y-2 text-sm text-muted-foreground">
                        <p>
                          When enabled, failed requests automatically failover
                          to another hosted model.
                          {isPriorityMode
                            ? " In priority mode, peers in the current tier are tried before traffic spills to the next tier."
                            : " In weighted mode, this means selecting from the remaining hosted models."}
                        </p>
                        <p>
                          <strong>Gateway rate limit:</strong> The hosted
                          model's configured RPS or concurrency limits were
                          exceeded.
                        </p>
                        <p>
                          <strong>Provider rate limit (429):</strong> The
                          upstream provider returned a rate limit error.
                        </p>
                        <p>
                          <strong>Server errors (5xx):</strong> The upstream
                          provider returned a server error.
                        </p>
                      </div>
                    </HoverCardContent>
                  </HoverCard>
                </div>

                {model.fallback?.enabled ? (
                  <div className="space-y-2">
                    <Badge
                      variant="outline"
                      className="bg-green-50 text-green-700 border-green-200"
                    >
                      Enabled
                    </Badge>
                    {fallbackTriggers.length > 0 && (
                      <div className="text-sm text-gray-600 space-y-1 mt-2">
                        <p className="text-xs text-gray-500 uppercase tracking-wide">
                          Failover on:
                        </p>
                        {fallbackTriggers.map((trigger, i) => (
                          <p key={i} className="flex items-center gap-2">
                            <span className="w-1.5 h-1.5 rounded-full bg-gray-400" />
                            {trigger}
                          </p>
                        ))}
                      </div>
                    )}
                    <p className="text-xs text-gray-500 mt-2">
                      {isPriorityMode
                        ? "On failure, tries peers in the current tier before the next tier."
                        : "On failure, selects from remaining hosted models by weighted least connections."}
                    </p>
                  </div>
                ) : (
                  <div>
                    <Badge
                      variant="outline"
                      className="bg-gray-100 text-gray-600 border-gray-200"
                    >
                      Disabled
                    </Badge>
                    <p className="text-xs text-gray-500 mt-2">
                      Requests will not automatically retry on failure.
                    </p>
                  </div>
                )}
              </div>
            </div>
          </CardContent>
        </Card>

        {/* Hosted Models List */}
        <Card className="p-0 gap-0 rounded-lg">
          <CardHeader className="px-6 pt-5 pb-4">
            <div className="flex items-center justify-between">
              <div>
                <CardTitle>
                  {isPriorityMode ? "Hosted Model Priority" : "Hosted Models"}
                </CardTitle>
                <CardDescription>
                  {sortedComponents.length} hosted model
                  {sortedComponents.length !== 1 ? "s" : ""}{" "}
                  {isPriorityMode
                    ? `across numbered failover tiers${canManage ? " — drag a provider onto another tier to pool them" : ""}`
                    : "configured"}
                </CardDescription>
              </div>
              {canManage && (
                <Button
                  size="sm"
                  onClick={() => setShowAddModal(true)}
                  className="gap-1"
                >
                  <Plus className="h-4 w-4" />
                  Add Hosted Model
                </Button>
              )}
            </div>
          </CardHeader>
          <CardContent className="px-6 pb-6 pt-0">
            {!components || components.length === 0 ? (
              <div className="text-center py-8">
                <GitMerge className="h-12 w-12 text-gray-300 mx-auto mb-3" />
                <p className="text-gray-500 mb-4">
                  No hosted models configured
                </p>
                {canManage && (
                  <Button
                    variant="outline"
                    onClick={() => setShowAddModal(true)}
                  >
                    <Plus className="h-4 w-4 mr-2" />
                    Add your first hosted model
                  </Button>
                )}
              </div>
            ) : canManage && isPriorityMode ? (
              <DndContext
                sensors={sensors}
                collisionDetection={closestCenter}
                onDragStart={() => setIsAnyDragging(true)}
                onDragEnd={(event) => {
                  setIsAnyDragging(false);
                  handleDragEnd(event);
                }}
                onDragCancel={() => setIsAnyDragging(false)}
              >
                <SortableContext
                  items={sortedComponents.map((c) => c.model.id)}
                  strategy={verticalListSortingStrategy}
                >
                  <div className="space-y-0">
                    {sortedComponents.map((component, index) => (
                      <React.Fragment key={component.model.id}>
                        {(index === 0 ||
                          sortedComponents[index - 1].sort_order !==
                            component.sort_order) && (
                          <div className="mt-4 first:mt-0 rounded-t-md border border-b-0 bg-gray-50 px-3 py-2">
                            <p className="text-sm font-medium text-gray-700">
                              Tier {component.sort_order + 1}
                            </p>
                            <p className="text-xs text-gray-500">
                              Providers in this tier share traffic by weighted
                              least connections.
                            </p>
                          </div>
                        )}
                        <SortableProviderRow
                          id={component.model.id}
                          component={component}
                          totalWeight={priorityTierWeights.get(component.sort_order) ?? 0}
                          priorityIndex={component.sort_order + 1}
                          isPriorityMode={true}
                          isLast={
                            index === sortedComponents.length - 1 ||
                            sortedComponents[index + 1].sort_order === component.sort_order
                          }
                          onEdit={() => setEditingComponent(component)}
                          onRemove={() => setRemovingComponent(component)}
                          onToggle={() => handleToggle(component)}
                          onToggleTrusted={() => handleToggleTrusted(component)}
                          onToggleAdapter={() => handleToggleAdapter(component)}
                          canManage={canManage}
                          isUpdating={
                            updateComponentMutation.isPending ||
                            updateLayoutMutation.isPending ||
                            removeMutation.isPending
                          }
                          strictModeEnabled={strictModeEnabled}
                          isAnyDragging={isAnyDragging}
                        />
                      </React.Fragment>
                    ))}
                  </div>
                </SortableContext>
              </DndContext>
            ) : (
              <div className={isPriorityMode ? "space-y-0" : "space-y-3"}>
                {sortedComponents.map((component, index) => (
                  <ProviderRow
                    key={component.model.id}
                    component={component}
                    totalWeight={
                      isPriorityMode
                        ? priorityTierWeights.get(component.sort_order) ?? 0
                        : totalWeight
                    }
                    priorityIndex={isPriorityMode ? component.sort_order + 1 : undefined}
                    isPriorityMode={isPriorityMode}
                    isLast={
                      index === sortedComponents.length - 1 ||
                      sortedComponents[index + 1].sort_order === component.sort_order
                    }
                    onEdit={() => setEditingComponent(component)}
                    onRemove={() => setRemovingComponent(component)}
                    onToggle={() => handleToggle(component)}
                    onToggleTrusted={() => handleToggleTrusted(component)}
                    onToggleAdapter={() => handleToggleAdapter(component)}
                    canManage={canManage}
                    isUpdating={
                      updateComponentMutation.isPending ||
                      removeMutation.isPending
                    }
                    strictModeEnabled={strictModeEnabled}
                  />
                ))}
              </div>
            )}
          </CardContent>
        </Card>
      </div>

      {/* Modals */}
      <AddProviderModal
        open={showAddModal}
        onClose={() => setShowAddModal(false)}
        modelId={model.id}
        existingComponentIds={components?.map((c) => c.model.id) || []}
        existingComponents={components ?? []}
        isPriorityMode={isPriorityMode}
        strictModeEnabled={strictModeEnabled}
      />

      <EditWeightModal
        open={editingComponent !== null}
        onClose={() => setEditingComponent(null)}
        component={editingComponent}
        modelId={model.id}
        components={components ?? []}
        isPriorityMode={isPriorityMode}
      />

      <ConfirmRemoveDialog
        open={removingComponent !== null}
        onClose={() => setRemovingComponent(null)}
        onConfirm={handleRemove}
        component={removingComponent}
        isRemoving={removeMutation.isPending}
      />

      <EditRoutingModal
        open={showRoutingModal}
        onClose={() => setShowRoutingModal(false)}
        model={model}
      />
    </>
  );
};

export default ProvidersTab;
