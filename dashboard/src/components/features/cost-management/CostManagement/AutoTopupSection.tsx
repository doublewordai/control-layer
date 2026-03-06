import { useState, useEffect } from "react";
import { Switch } from "@/components/ui/switch";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { useUpdateUser } from "@/api/control-layer";
import { toast } from "sonner";
import { formatDollars } from "@/utils/money";
import type { User } from "@/api/control-layer";

interface AutoTopupSectionProps {
  user: User;
  userId: string;
  onSetupRequired: () => void;
  onSuccess: () => void;
}

export function AutoTopupSection({
  user,
  userId,
  onSetupRequired,
  onSuccess,
}: AutoTopupSectionProps) {
  const isEnabled =
    user.auto_topup_amount != null && user.auto_topup_threshold != null;
  const hasPaymentMethod = user.has_auto_topup_payment_method;

  const [isEditing, setIsEditing] = useState(false);
  const [threshold, setThreshold] = useState(
    user.auto_topup_threshold?.toString() ?? "5.00",
  );
  const [amount, setAmount] = useState(
    user.auto_topup_amount?.toString() ?? "25.00",
  );

  const updateUserMutation = useUpdateUser();

  useEffect(() => {
    if (user.auto_topup_threshold != null)
      setThreshold(user.auto_topup_threshold.toString());
    if (user.auto_topup_amount != null)
      setAmount(user.auto_topup_amount.toString());
  }, [user.auto_topup_threshold, user.auto_topup_amount]);

  const handleToggle = async (checked: boolean) => {
    if (checked) {
      if (!hasPaymentMethod) {
        onSetupRequired();
        return;
      }
      const parsedThreshold = parseFloat(threshold);
      const thresholdNum = Number.isFinite(parsedThreshold) ? parsedThreshold : 5.0;
      const parsedAmount = parseFloat(amount);
      const amountNum = Number.isFinite(parsedAmount) ? parsedAmount : 25.0;
      try {
        await updateUserMutation.mutateAsync({
          id: userId,
          data: {
            auto_topup_threshold: thresholdNum,
            auto_topup_amount: amountNum,
          },
        });
        toast.success("Auto top-up enabled");
        onSuccess();
      } catch (err) {
        console.error("Failed to enable auto top-up:", err);
        toast.error("Failed to enable auto top-up");
      }
    } else {
      try {
        await updateUserMutation.mutateAsync({
          id: userId,
          data: { auto_topup_threshold: null, auto_topup_amount: null },
        });
        toast.success("Auto top-up disabled");
        setIsEditing(false);
        onSuccess();
      } catch (err) {
        console.error("Failed to disable auto top-up:", err);
        toast.error("Failed to disable auto top-up");
      }
    }
  };

  const handleSave = async () => {
    const thresholdNum = parseFloat(threshold);
    const amountNum = parseFloat(amount);
    if (isNaN(thresholdNum) || thresholdNum < 0) {
      toast.error("Please enter a valid threshold ($0 or more)");
      return;
    }
    if (isNaN(amountNum) || amountNum <= 0) {
      toast.error("Please enter a valid amount greater than $0");
      return;
    }
    try {
      await updateUserMutation.mutateAsync({
        id: userId,
        data: {
          auto_topup_threshold: thresholdNum,
          auto_topup_amount: amountNum,
        },
      });
      toast.success(
        `Auto top-up updated: ${formatDollars(amountNum)} when below ${formatDollars(thresholdNum)}`,
      );
      setIsEditing(false);
      onSuccess();
    } catch (err) {
      console.error("Failed to update auto top-up settings:", err);
      toast.error("Failed to update auto top-up settings");
    }
  };

  // Compact inline: switch + label, then either status text or edit fields
  return (
    <div className="flex items-center gap-2">
      <Switch
        checked={isEnabled}
        onCheckedChange={handleToggle}
        disabled={updateUserMutation.isPending}
        aria-label="Toggle auto top-up"
      />
      <span className="text-sm text-doubleword-neutral-600 whitespace-nowrap">
        Auto Top-Up
      </span>

      {isEnabled && !isEditing && (
        <button
          onClick={() => setIsEditing(true)}
          className="text-xs text-doubleword-neutral-500 hover:text-doubleword-neutral-900 whitespace-nowrap transition-colors"
        >
          <span className="font-mono">
            {formatDollars(user.auto_topup_amount!)}
          </span>
          {" below "}
          <span className="font-mono">
            {formatDollars(user.auto_topup_threshold!)}
          </span>
        </button>
      )}

      {isEnabled && isEditing && (
        <>
          <div className="flex items-center gap-1">
            <span className="text-xs text-doubleword-neutral-400">$</span>
            <Input
              type="number"
              min="1"
              step="5"
              value={amount}
              onChange={(e) => setAmount(e.target.value)}
              aria-label="Top-up amount"
              className="w-16 h-7 text-xs font-mono px-1.5 [appearance:textfield] [&::-webkit-outer-spin-button]:appearance-none [&::-webkit-inner-spin-button]:appearance-none"
            />
          </div>
          <span className="text-xs text-doubleword-neutral-400">below</span>
          <div className="flex items-center gap-1">
            <span className="text-xs text-doubleword-neutral-400">$</span>
            <Input
              type="number"
              min="0"
              step="1"
              value={threshold}
              onChange={(e) => setThreshold(e.target.value)}
              aria-label="Threshold amount"
              className="w-16 h-7 text-xs font-mono px-1.5 [appearance:textfield] [&::-webkit-outer-spin-button]:appearance-none [&::-webkit-inner-spin-button]:appearance-none"
            />
          </div>
          <Button
            variant="ghost"
            size="sm"
            onClick={() => {
              setThreshold(user.auto_topup_threshold?.toString() ?? "5.00");
              setAmount(user.auto_topup_amount?.toString() ?? "25.00");
              setIsEditing(false);
            }}
            className="h-7 px-1.5 text-xs text-doubleword-neutral-400"
          >
            Cancel
          </Button>
          <Button
            size="sm"
            onClick={handleSave}
            disabled={updateUserMutation.isPending}
            className="h-7 px-2 text-xs"
          >
            {updateUserMutation.isPending ? "..." : "Save"}
          </Button>
        </>
      )}
    </div>
  );
}
