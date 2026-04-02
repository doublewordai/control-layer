import { useState, useEffect } from "react";
import { Switch } from "@/components/ui/switch";
import { Input } from "@/components/ui/input";
import { Button } from "@/components/ui/button";
import { useEnableAutoTopup, useDisableAutoTopup } from "@/api/control-layer";
import { useCreateBillingPortalSession } from "@/api/control-layer";
import { toast } from "sonner";
import { formatDollars } from "@/utils/money";
import type { User } from "@/api/control-layer";

interface AutoTopupSectionProps {
  user: User;
  onSuccess: () => void;
}

export function AutoTopupSection({
  user,
  onSuccess,
}: AutoTopupSectionProps) {
  const hasPaymentMethod = user.has_auto_topup_payment_method;
  const isEnabled =
    user.auto_topup_amount != null &&
    user.auto_topup_threshold != null &&
    hasPaymentMethod;

  const [isEditing, setIsEditing] = useState(false);
  const [threshold, setThreshold] = useState(
    user.auto_topup_threshold?.toString() ?? "5.00",
  );
  const [amount, setAmount] = useState(
    user.auto_topup_amount?.toString() ?? "25.00",
  );
  const [monthlyLimit, setMonthlyLimit] = useState(
    user.auto_topup_monthly_limit?.toString() ?? "",
  );

  const enableAutoTopupMutation = useEnableAutoTopup();
  const disableAutoTopupMutation = useDisableAutoTopup();
  const billingPortalMutation = useCreateBillingPortalSession();

  const isPending =
    enableAutoTopupMutation.isPending ||
    disableAutoTopupMutation.isPending ||
    billingPortalMutation.isPending;

  useEffect(() => {
    if (user.auto_topup_threshold != null)
      setThreshold(user.auto_topup_threshold.toString());
    if (user.auto_topup_amount != null)
      setAmount(user.auto_topup_amount.toString());
    if (user.auto_topup_monthly_limit != null)
      setMonthlyLimit(user.auto_topup_monthly_limit.toString());
    else setMonthlyLimit("");
  }, [
    user.auto_topup_threshold,
    user.auto_topup_amount,
    user.auto_topup_monthly_limit,
  ]);

  const parseMonthlyLimit = (): number | null => {
    const parsed = parseFloat(monthlyLimit);
    return Number.isFinite(parsed) && parsed > 0 ? parsed : null;
  };

  const handleToggle = async (checked: boolean) => {
    if (checked) {
      const parsedThreshold = parseFloat(threshold);
      const thresholdNum = Number.isFinite(parsedThreshold)
        ? parsedThreshold
        : 5.0;
      const parsedAmount = parseFloat(amount);
      const amountNum = Number.isFinite(parsedAmount) ? parsedAmount : 25.0;

      try {
        const result = await enableAutoTopupMutation.mutateAsync({
          threshold: thresholdNum,
          amount: amountNum,
          monthlyLimit: parseMonthlyLimit(),
        });

        if (result.has_payment_method) {
          toast.success("Auto top-up enabled");
          onSuccess();
        } else if (result.needs_billing_portal) {
          // No card on file -- redirect to billing portal to add one
          const portal = await billingPortalMutation.mutateAsync();
          window.location.href = portal.url;
        }
      } catch (err) {
        console.error("Failed to enable auto top-up:", err);
        toast.error("Failed to enable auto top-up");
      }
    } else {
      try {
        await disableAutoTopupMutation.mutateAsync();
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
    const limitNum = parseMonthlyLimit();
    try {
      await enableAutoTopupMutation.mutateAsync({
        threshold: thresholdNum,
        amount: amountNum,
        monthlyLimit: limitNum,
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
        disabled={isPending}
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
          {user.auto_topup_monthly_limit != null && (
            <>
              {" · max "}
              <span className="font-mono">
                {formatDollars(user.auto_topup_monthly_limit)}
              </span>
              {"/mo"}
            </>
          )}
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
          <span className="text-xs text-doubleword-neutral-400">limit</span>
          <div className="flex items-center gap-1">
            <span className="text-xs text-doubleword-neutral-400">$</span>
            <Input
              type="number"
              min="0"
              step="10"
              value={monthlyLimit}
              onChange={(e) => setMonthlyLimit(e.target.value)}
              placeholder="none"
              aria-label="Monthly limit"
              className="w-16 h-7 text-xs font-mono px-1.5 [appearance:textfield] [&::-webkit-outer-spin-button]:appearance-none [&::-webkit-inner-spin-button]:appearance-none"
            />
            <span className="text-xs text-doubleword-neutral-400">/mo</span>
          </div>
          <Button
            variant="ghost"
            size="sm"
            onClick={() => {
              setThreshold(user.auto_topup_threshold?.toString() ?? "5.00");
              setAmount(user.auto_topup_amount?.toString() ?? "25.00");
              setMonthlyLimit(
                user.auto_topup_monthly_limit?.toString() ?? "",
              );
              setIsEditing(false);
            }}
            className="h-7 px-1.5 text-xs text-doubleword-neutral-400"
          >
            Cancel
          </Button>
          <Button
            size="sm"
            onClick={handleSave}
            disabled={isPending}
            className="h-7 px-2 text-xs"
          >
            {isPending ? "..." : "Save"}
          </Button>
        </>
      )}
    </div>
  );
}
