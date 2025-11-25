import React, { useState, useEffect } from "react";
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
import { Input } from "../../ui/input";
import { Label } from "../../ui/label";

interface UpdateModelPricingModalProps {
  isOpen: boolean;
  modelName: string;
  currentPricing?: {
    input_price_per_token?: number | null;
    output_price_per_token?: number | null;
  };
  onSubmit: (pricing: {
    input_price_per_token?: number;
    output_price_per_token?: number;
  }) => void;
  onClose: () => void;
  isLoading?: boolean;
}

export const UpdateModelPricingModal: React.FC<
  UpdateModelPricingModalProps
> = ({
  isOpen,
  modelName,
  currentPricing,
  onSubmit,
  onClose,
  isLoading = false,
}) => {
  const [inputPrice, setInputPrice] = useState<string>("");
  const [outputPrice, setOutputPrice] = useState<string>("");
  const [errors, setErrors] = useState<{
    input?: string;
    output?: string;
  }>({});

  // Initialize with current pricing when modal opens (convert to per million)
  useEffect(() => {
    if (isOpen && currentPricing) {
      setInputPrice(
        currentPricing.input_price_per_token
          ? (currentPricing.input_price_per_token * 1000000).toString()
          : ""
      );
      setOutputPrice(
        currentPricing.output_price_per_token
          ? (currentPricing.output_price_per_token * 1000000).toString()
          : ""
      );
    } else if (isOpen && !currentPricing) {
      setInputPrice("");
      setOutputPrice("");
    }
  }, [isOpen, currentPricing]);

  const validatePricing = () => {
    const newErrors: { input?: string; output?: string } = {};

    if (inputPrice && (isNaN(Number(inputPrice)) || Number(inputPrice) < 0)) {
      newErrors.input = "Must be a valid positive number";
    }

    if (
      outputPrice &&
      (isNaN(Number(outputPrice)) || Number(outputPrice) < 0)
    ) {
      newErrors.output = "Must be a valid positive number";
    }

    setErrors(newErrors);
    return Object.keys(newErrors).length === 0;
  };

  const handleSubmit = () => {
    if (!validatePricing()) {
      return;
    }

    const pricing: {
      input_price_per_token?: number;
      output_price_per_token?: number;
    } = {};

    // Only include fields that have values (convert from per million to per token)
    if (inputPrice && inputPrice.trim() !== "") {
      pricing.input_price_per_token = Number(inputPrice) / 1000000;
    }

    if (outputPrice && outputPrice.trim() !== "") {
      pricing.output_price_per_token = Number(outputPrice) / 1000000;
    }

    onSubmit(pricing);
  };

  const handleClose = () => {
    if (!isLoading) {
      setInputPrice("");
      setOutputPrice("");
      setErrors({});
      onClose();
    }
  };

  return (
    <Dialog open={isOpen} onOpenChange={handleClose}>
      <DialogContent className="sm:max-w-[500px]">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <DollarSign className="h-5 w-5" />
            Update Pricing for {modelName}
          </DialogTitle>
          <DialogDescription>
            Set the upstream price per million tokens for input and output
          </DialogDescription>
        </DialogHeader>

        <div className="grid gap-4 py-4">
          <div className="grid gap-2">
            <Label htmlFor="input-price">Input Price per Million Tokens</Label>
            <Input
              id="input-price"
              type="text"
              placeholder="e.g., 3.00"
              value={inputPrice}
              onChange={(e) => setInputPrice(e.target.value)}
              className={errors.input ? "border-red-500" : ""}
              disabled={isLoading}
            />
            {errors.input && (
              <p className="text-sm text-red-500">{errors.input}</p>
            )}
            <p className="text-sm text-gray-500">
              Price in dollars per million input tokens
            </p>
          </div>

          <div className="grid gap-2">
            <Label htmlFor="output-price">Output Price per Million Tokens</Label>
            <Input
              id="output-price"
              type="text"
              placeholder="e.g., 15.00"
              value={outputPrice}
              onChange={(e) => setOutputPrice(e.target.value)}
              className={errors.output ? "border-red-500" : ""}
              disabled={isLoading}
            />
            {errors.output && (
              <p className="text-sm text-red-500">{errors.output}</p>
            )}
            <p className="text-sm text-gray-500">
              Price in dollars per million output tokens
            </p>
          </div>
        </div>

        <DialogFooter>
          <Button
            type="button"
            variant="outline"
            onClick={handleClose}
            disabled={isLoading}
          >
            Cancel
          </Button>
          <Button type="button" onClick={handleSubmit} disabled={isLoading}>
            {isLoading ? (
              <>
                <div className="w-4 h-4 border-2 border-white border-t-transparent rounded-full animate-spin mr-2" />
                Saving...
              </>
            ) : (
              "Save Pricing"
            )}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};
