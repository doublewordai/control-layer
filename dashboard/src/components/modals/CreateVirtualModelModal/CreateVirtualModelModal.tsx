import React, { useState, useEffect } from "react";
import { useNavigate } from "react-router-dom";
import { useCreateModel } from "../../../api/control-layer";
import type {
  VirtualModelCreate,
  LoadBalancingStrategy,
  ModelType,
} from "../../../api/control-layer";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
  DialogDescription,
} from "../../ui/dialog";
import { Button } from "../../ui/button";
import { AlertBox } from "../../ui/alert-box";
import { Label } from "../../ui/label";
import { Input } from "../../ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../ui/select";
import { Checkbox } from "../../ui/checkbox";

interface CreateVirtualModelModalProps {
  isOpen: boolean;
  onClose: () => void;
  onSuccess?: () => void;
}

const DEFAULT_FALLBACK_STATUS_CODES = [429, 500, 502, 503, 504];

export const CreateVirtualModelModal: React.FC<
  CreateVirtualModelModalProps
> = ({ isOpen, onClose, onSuccess }) => {
  const navigate = useNavigate();
  const [formData, setFormData] = useState({
    model_name: "",
    alias: "",
    description: "",
    model_type: "" as ModelType | "",
    lb_strategy: "weighted_random" as LoadBalancingStrategy,
    fallback_enabled: true,
    fallback_on_rate_limit: true,
    fallback_on_status: DEFAULT_FALLBACK_STATUS_CODES,
    sanitize_responses: false,
  });
  const [error, setError] = useState<string | null>(null);

  const createModelMutation = useCreateModel();

  // Reset form when modal opens
  useEffect(() => {
    if (isOpen) {
      setFormData({
        model_name: "",
        alias: "",
        description: "",
        model_type: "",
        lb_strategy: "weighted_random",
        fallback_enabled: true,
        fallback_on_rate_limit: true,
        fallback_on_status: DEFAULT_FALLBACK_STATUS_CODES,
        sanitize_responses: false,
      });
      setError(null);
    }
  }, [isOpen]);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!formData.model_name.trim()) {
      setError("Model name is required");
      return;
    }

    setError(null);

    const payload: VirtualModelCreate = {
      type: "composite", // API expects "composite" for backwards compatibility
      model_name: formData.model_name.trim(),
      alias: formData.alias.trim() || undefined,
      description: formData.description.trim() || undefined,
      model_type: formData.model_type || undefined,
      lb_strategy: formData.lb_strategy,
      fallback_enabled: formData.fallback_enabled,
      fallback_on_rate_limit: formData.fallback_on_rate_limit,
      fallback_on_status: formData.fallback_on_status,
      sanitize_responses: formData.sanitize_responses,
    };

    try {
      const createdModel = await createModelMutation.mutateAsync(payload);

      onSuccess?.();
      onClose();

      // Navigate to the model detail page to add hosted models
      navigate(`/models/${createdModel.id}?tab=providers`);
    } catch (err) {
      setError(
        err instanceof Error ? err.message : "Failed to create virtual model",
      );
    }
  };

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Create Virtual Model</DialogTitle>
          <DialogDescription>
            A virtual model routes requests across multiple hosted models with
            load balancing and automatic failover.
          </DialogDescription>
        </DialogHeader>

        <AlertBox variant="error" className="mb-4">
          {error}
        </AlertBox>

        <form
          id="create-virtual-model-form"
          onSubmit={handleSubmit}
          className="space-y-4"
        >
          {/* Model Name */}
          <div>
            <Label htmlFor="model_name">Model Name *</Label>
            <Input
              type="text"
              id="model_name"
              value={formData.model_name}
              onChange={(e) =>
                setFormData({ ...formData, model_name: e.target.value })
              }
              placeholder="e.g., gpt-4-balanced"
              disabled={createModelMutation.isPending}
              className="mt-1"
              required
            />
            <p className="text-xs text-muted-foreground mt-1">
              Internal name for the model
            </p>
          </div>

          {/* Alias */}
          <div>
            <Label htmlFor="alias">Alias</Label>
            <Input
              type="text"
              id="alias"
              value={formData.alias}
              onChange={(e) =>
                setFormData({ ...formData, alias: e.target.value })
              }
              placeholder={formData.model_name || "Same as model name"}
              disabled={createModelMutation.isPending}
              className="mt-1"
            />
            <p className="text-xs text-muted-foreground mt-1">
              The name users will use in API requests
            </p>
          </div>

          {/* Description */}
          <div>
            <Label htmlFor="description">Description</Label>
            <textarea
              id="description"
              value={formData.description}
              onChange={(e) =>
                setFormData({ ...formData, description: e.target.value })
              }
              rows={2}
              className="flex min-h-[60px] w-full rounded-md border border-input bg-transparent px-3 py-2 text-sm shadow-xs placeholder:text-muted-foreground focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring disabled:cursor-not-allowed disabled:opacity-50 mt-1"
              placeholder="Optional description"
              disabled={createModelMutation.isPending}
            />
          </div>

          {/* Model Type */}
          <div>
            <Label htmlFor="model_type">Model Type</Label>
            <Select
              value={formData.model_type}
              onValueChange={(value) =>
                setFormData({ ...formData, model_type: value as ModelType })
              }
              disabled={createModelMutation.isPending}
            >
              <SelectTrigger className="mt-1" id="model_type">
                <SelectValue placeholder="Select type (optional)" />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="CHAT">Chat</SelectItem>
                <SelectItem value="EMBEDDINGS">Embeddings</SelectItem>
                <SelectItem value="RERANKER">Reranker</SelectItem>
              </SelectContent>
            </Select>
          </div>

          {/* Load Balancing Strategy */}
          <div>
            <Label htmlFor="lb_strategy">Load Balancing Strategy</Label>
            <Select
              value={formData.lb_strategy}
              onValueChange={(value) =>
                setFormData({
                  ...formData,
                  lb_strategy: value as LoadBalancingStrategy,
                })
              }
              disabled={createModelMutation.isPending}
            >
              <SelectTrigger className="mt-1" id="lb_strategy">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="weighted_random">Weighted Random</SelectItem>
                <SelectItem value="priority">Priority</SelectItem>
              </SelectContent>
            </Select>
            <p className="text-xs text-muted-foreground mt-1">
              {formData.lb_strategy === "weighted_random"
                ? "Distributes traffic across hosted models based on their weights"
                : "Routes to highest-weight hosted model first, failing over to next"}
            </p>
          </div>

          {/* Response Sanitization */}
          <div className="flex items-center space-x-2">
            <Checkbox
              id="sanitize_responses"
              checked={formData.sanitize_responses}
              onCheckedChange={(checked) =>
                setFormData({ ...formData, sanitize_responses: !!checked })
              }
              disabled={createModelMutation.isPending}
            />
            <Label htmlFor="sanitize_responses" className="font-normal">
              Sanitize responses (filter out third party fields from OpenAI
              compatible responses)
            </Label>
          </div>

          {/* Fallback Settings */}
          <div className="border rounded-md p-4 space-y-3">
            <div className="font-medium text-sm">Fallback Settings</div>

            <div className="flex items-center space-x-2">
              <Checkbox
                id="fallback_enabled"
                checked={formData.fallback_enabled}
                onCheckedChange={(checked) =>
                  setFormData({ ...formData, fallback_enabled: !!checked })
                }
                disabled={createModelMutation.isPending}
              />
              <Label htmlFor="fallback_enabled" className="font-normal">
                Enable fallback to other hosted models on failure
              </Label>
            </div>

            <div className="flex items-center space-x-2">
              <Checkbox
                id="fallback_on_rate_limit"
                checked={formData.fallback_on_rate_limit}
                onCheckedChange={(checked) =>
                  setFormData({
                    ...formData,
                    fallback_on_rate_limit: !!checked,
                  })
                }
                disabled={
                  createModelMutation.isPending || !formData.fallback_enabled
                }
              />
              <Label htmlFor="fallback_on_rate_limit" className="font-normal">
                Fall back when rate limited (429)
              </Label>
            </div>

            <div>
              <Label className="font-normal text-sm">
                Fallback on status codes
              </Label>
              <Input
                type="text"
                value={formData.fallback_on_status.join(", ")}
                onChange={(e) => {
                  const codes = e.target.value
                    .split(",")
                    .map((s) => parseInt(s.trim(), 10))
                    .filter((n) => !isNaN(n) && n >= 100 && n < 600);
                  setFormData({ ...formData, fallback_on_status: codes });
                }}
                placeholder="429, 500, 502, 503, 504"
                disabled={
                  createModelMutation.isPending || !formData.fallback_enabled
                }
                className="mt-1"
              />
              <p className="text-xs text-muted-foreground mt-1">
                Comma-separated HTTP status codes that trigger fallback
              </p>
            </div>
          </div>
        </form>

        <DialogFooter>
          <Button
            type="button"
            variant="outline"
            onClick={onClose}
            disabled={createModelMutation.isPending}
          >
            Cancel
          </Button>
          <Button
            type="submit"
            form="create-virtual-model-form"
            disabled={
              createModelMutation.isPending || !formData.model_name.trim()
            }
          >
            {createModelMutation.isPending
              ? "Creating..."
              : "Create & Add Hosted Models"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};
