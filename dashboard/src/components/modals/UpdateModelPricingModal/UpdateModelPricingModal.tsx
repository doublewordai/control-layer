import React, { useState, useEffect, useMemo } from "react";
import { DollarSign } from "lucide-react";
import { Button } from "../../ui/button";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
  DialogDescription,
} from "../../ui/dialog";
import { ModelTariffTable } from "../../features/models/ModelTariffTable";
import { useModel, useUpdateModel } from "../../../api/control-layer/hooks";
import type { TariffDefinition } from "../../../api/control-layer/types";
import { toast } from "sonner";

interface UpdateModelPricingModalProps {
  isOpen: boolean;
  modelId: string;
  modelName: string;
  onClose: () => void;
}

export const UpdateModelPricingModal: React.FC<
  UpdateModelPricingModalProps
> = ({ isOpen, modelId, modelName, onClose }) => {
  // Fetch model with tariffs included
  const { data: model, isLoading: isLoadingModel } = useModel(modelId, {
    include: "pricing",
  });

  // Mutation
  const updateModel = useUpdateModel();

  // Local state for tariff changes
  const [pendingTariffs, setPendingTariffs] = useState<
    TariffDefinition[] | null
  >(null);

  // Memoize tariffs to avoid creating new array reference on every render
  const currentTariffs = useMemo(() => model?.tariffs || [], [model?.tariffs]);

  // Reset pending changes when modal closes
  useEffect(() => {
    if (!isOpen) {
      setPendingTariffs(null);
    }
  }, [isOpen]);

  const handleTariffsChange = (tariffs: TariffDefinition[]) => {
    setPendingTariffs(tariffs);
  };

  const handleSave = async () => {
    if (pendingTariffs === null) {
      // No changes, just close
      onClose();
      return;
    }

    try {
      await updateModel.mutateAsync({
        id: modelId,
        data: {
          tariffs: pendingTariffs,
        },
      });
      onClose();
    } catch (error) {
      // Only log detailed errors in development to avoid leaking server info
      if (import.meta.env.DEV) {
        console.error("Failed to update model tariffs:", error);
      }
      toast.error("Failed to update pricing. Please try again.");
    }
  };

  const hasChanges = pendingTariffs !== null;

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-[900px] max-h-[80vh] overflow-y-auto">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <DollarSign className="h-5 w-5" />
            Manage Pricing Tariffs for {modelName}
          </DialogTitle>
          <DialogDescription>
            Configure pricing tiers for different API key purposes. You can set
            different rates for realtime, batch, and playground usage.
          </DialogDescription>
        </DialogHeader>

        <div className="py-4">
          {isLoadingModel ? (
            <div className="flex items-center justify-center py-8">
              <div className="w-8 h-8 border-4 border-gray-300 border-t-blue-600 rounded-full animate-spin" />
            </div>
          ) : (
            <ModelTariffTable
              tariffs={currentTariffs}
              onChange={handleTariffsChange}
              isLoading={updateModel.isPending}
            />
          )}
        </div>

        <DialogFooter>
          <Button
            type="button"
            variant="outline"
            onClick={onClose}
            disabled={updateModel.isPending}
          >
            Cancel
          </Button>
          <Button
            type="button"
            onClick={handleSave}
            disabled={updateModel.isPending || !hasChanges}
          >
            {updateModel.isPending ? "Saving..." : "Save Changes"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};
