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
import { Switch } from "../../ui/switch";
import { Input } from "../../ui/input";
import { Label } from "../../ui/label";
import {
  useModelCachePricing,
  useUpdateModelCachePricing,
  useDeleteModelCachePricing,
} from "../../../api/control-layer/hooks";
import { toast } from "sonner";

interface CachePricingModalProps {
  isOpen: boolean;
  modelId: string;
  modelName: string;
  onClose: () => void;
}

// Multipliers are decimal strings (precision-preserving, like the normal price fields);
// min-prefix is an integer string for the number input.
interface FormState {
  enabled: boolean;
  write_multiplier_5m: string;
  write_multiplier_1h: string;
  write_multiplier_24h: string;
  read_multiplier: string;
  min_prefix_tokens: string;
}

const EMPTY: FormState = {
  enabled: false,
  write_multiplier_5m: "",
  write_multiplier_1h: "",
  write_multiplier_24h: "",
  read_multiplier: "",
  min_prefix_tokens: "",
};

export const CachePricingModal: React.FC<CachePricingModalProps> = ({
  isOpen,
  modelId,
  modelName,
  onClose,
}) => {
  const { data: current, isLoading } = useModelCachePricing(modelId, {
    enabled: isOpen,
  });
  const updateMutation = useUpdateModelCachePricing();
  const deleteMutation = useDeleteModelCachePricing();
  const pending = updateMutation.isPending || deleteMutation.isPending;

  const [form, setForm] = useState<FormState>(EMPTY);

  // Initialise the form from the fetched config when it (re)loads or the modal opens.
  useEffect(() => {
    if (!isOpen) return;
    if (current?.enabled) {
      setForm({
        enabled: true,
        write_multiplier_5m: current.write_multiplier_5m ?? "",
        write_multiplier_1h: current.write_multiplier_1h ?? "",
        write_multiplier_24h: current.write_multiplier_24h ?? "",
        read_multiplier: current.read_multiplier ?? "",
        min_prefix_tokens:
          current.min_prefix_tokens != null
            ? String(current.min_prefix_tokens)
            : "",
      });
    } else {
      setForm(EMPTY);
    }
  }, [current, isOpen]);

  const set = (key: keyof FormState, value: string | boolean) =>
    setForm((prev) => ({ ...prev, [key]: value }));

  const handleSave = async () => {
    try {
      if (form.enabled) {
        // Blank field → omit, so the backend fills it from the global default.
        const dec = (s: string) => (s.trim() === "" ? undefined : s.trim());
        await updateMutation.mutateAsync({
          modelId,
          data: {
            write_multiplier_5m: dec(form.write_multiplier_5m),
            write_multiplier_1h: dec(form.write_multiplier_1h),
            write_multiplier_24h: dec(form.write_multiplier_24h),
            read_multiplier: dec(form.read_multiplier),
            min_prefix_tokens:
              form.min_prefix_tokens.trim() === ""
                ? undefined
                : parseInt(form.min_prefix_tokens, 10),
          },
        });
        toast.success("Cache pricing updated");
      } else if (current?.enabled) {
        // Toggled off → disable.
        await deleteMutation.mutateAsync(modelId);
        toast.success("Cache pricing disabled");
      }
      onClose();
    } catch (error) {
      if (import.meta.env.DEV) {
        console.error("Failed to update cache pricing:", error);
      }
      toast.error("Failed to update cache pricing. Please try again.");
    }
  };

  const numberField = (
    label: string,
    key: keyof FormState,
    placeholder: string,
    step = "0.01",
  ) => (
    <div>
      <label className="text-xs text-gray-500">{label}</label>
      <Input
        type="number"
        step={step}
        min="0"
        value={form[key] as string}
        onChange={(e) => set(key, e.target.value)}
        placeholder={placeholder}
        disabled={pending}
      />
    </div>
  );

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-[500px]">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <DollarSign className="h-5 w-5" />
            Cache Pricing for {modelName}
          </DialogTitle>
          <DialogDescription>
            Anthropic-style prompt-cache pricing: write multipliers per TTL tier
            (5m / 1h / 24h), the read multiplier, and the minimum cacheable
            prefix. Blank fields use the global defaults.
          </DialogDescription>
        </DialogHeader>

        <div className="py-4 space-y-6">
          {isLoading ? (
            <div className="flex items-center justify-center py-8">
              <div className="w-8 h-8 border-4 border-gray-300 border-t-blue-600 rounded-full animate-spin" />
            </div>
          ) : (
            <>
              <div className="flex items-center justify-between">
                <Label className="text-sm font-medium">
                  Enable cache pricing
                </Label>
                <Switch
                  checked={form.enabled}
                  onCheckedChange={(checked) => set("enabled", checked)}
                  disabled={pending}
                />
              </div>

              {form.enabled && (
                <div className="space-y-4 pl-4 border-l-2 border-gray-200">
                  <div>
                    <Label className="text-xs text-gray-600 font-medium block mb-2">
                      Write multipliers (× base input price)
                    </Label>
                    <div className="space-y-2">
                      {numberField(
                        "5-minute",
                        "write_multiplier_5m",
                        "e.g. 1.25",
                      )}
                      {numberField("1-hour", "write_multiplier_1h", "e.g. 2.0")}
                      {numberField(
                        "24-hour",
                        "write_multiplier_24h",
                        "e.g. 2.5",
                      )}
                    </div>
                  </div>
                  {numberField("Read multiplier", "read_multiplier", "e.g. 0.1")}
                  {numberField(
                    "Minimum prefix tokens",
                    "min_prefix_tokens",
                    "e.g. 1024",
                    "1",
                  )}
                </div>
              )}
            </>
          )}
        </div>

        <DialogFooter>
          <Button
            type="button"
            variant="outline"
            onClick={onClose}
            disabled={pending}
          >
            Cancel
          </Button>
          <Button type="button" onClick={handleSave} disabled={pending}>
            {pending ? "Saving..." : "Save Changes"}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};
