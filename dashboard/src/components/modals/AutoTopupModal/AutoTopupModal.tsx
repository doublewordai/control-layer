import { useState, useEffect, useCallback } from "react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  useUpdateUser,
  useCreateAutoTopupCheckout,
  useProcessAutoTopupSetup,
} from "@/api/control-layer";
import { toast } from "sonner";
import { formatDollars } from "@/utils/money";
import { CheckCircle2, XCircle, Loader2 } from "lucide-react";

const LOCAL_STORAGE_KEY = "autoTopupSetup";

interface AutoTopupModalProps {
  isOpen: boolean;
  onClose: () => void;
  autoTopupAmount: number | null;
  autoTopupThreshold: number | null;
  userId: string;
  onSuccess?: () => void;
  /** Stripe redirect state: undefined=no redirect, "fail"=cancelled, anything else=session ID */
  autoTopupId?: string;
}

type ModalMode = "setup" | "processing" | "cancelled" | "manage";

function getMode(
  autoTopupId: string | undefined,
  autoTopupAmount: number | null,
  autoTopupThreshold: number | null,
): ModalMode {
  if (autoTopupId === "fail") return "cancelled";
  if (autoTopupId && autoTopupId !== "fail") return "processing";
  if (autoTopupAmount != null && autoTopupThreshold != null) return "manage";
  return "setup";
}

export function AutoTopupModal({
  isOpen,
  onClose,
  autoTopupAmount,
  autoTopupThreshold,
  userId,
  onSuccess,
  autoTopupId,
}: AutoTopupModalProps) {
  const mode = getMode(autoTopupId, autoTopupAmount, autoTopupThreshold);

  const [threshold, setThreshold] = useState(
    autoTopupThreshold?.toString() ?? "5.00",
  );
  const [amount, setAmount] = useState(
    autoTopupAmount?.toString() ?? "25.00",
  );

  const updateUserMutation = useUpdateUser();
  const checkoutMutation = useCreateAutoTopupCheckout();
  const processSetupMutation = useProcessAutoTopupSetup();

  // Reset fields when modal opens
  const handleOpenChange = (open: boolean) => {
    if (open) {
      setThreshold(autoTopupThreshold?.toString() ?? "5.00");
      setAmount(autoTopupAmount?.toString() ?? "25.00");
    } else {
      onClose();
    }
  };

  // Processing mode: call backend to verify session and save settings
  const processAutoTopup = useCallback(() => {
    if (mode !== "processing" || !autoTopupId) return;

    const stored = localStorage.getItem(LOCAL_STORAGE_KEY);
    if (!stored) {
      toast.error("Auto top-up setup data not found. Please try again.");
      return;
    }

    let parsed: { threshold: number; amount: number };
    try {
      parsed = JSON.parse(stored);
    } catch {
      toast.error("Invalid auto top-up setup data. Please try again.");
      return;
    }

    processSetupMutation.mutate(
      {
        sessionId: autoTopupId,
        threshold: parsed.threshold,
        amount: parsed.amount,
      },
      {
        onSuccess: () => {
          localStorage.removeItem(LOCAL_STORAGE_KEY);
          onSuccess?.();
        },
        onError: () => {
          toast.error("Failed to enable auto top-up. Please try again.");
        },
      },
    );
  }, [mode, autoTopupId, processSetupMutation, onSuccess]);

  useEffect(() => {
    if (isOpen && mode === "processing") {
      processAutoTopup();
    }
    // Only run when modal opens in processing mode
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [isOpen, mode]);

  // Setup mode: store settings and redirect to Stripe
  const handleSetupPayment = async () => {
    const thresholdNum = parseFloat(threshold);
    const amountNum = parseFloat(amount);
    if (isNaN(thresholdNum) || thresholdNum < 0) {
      toast.error("Please enter a valid threshold ($0 or more).");
      return;
    }
    if (isNaN(amountNum) || amountNum <= 0) {
      toast.error("Please enter a valid amount greater than $0.");
      return;
    }

    // Store settings in localStorage for after Stripe redirect
    localStorage.setItem(
      LOCAL_STORAGE_KEY,
      JSON.stringify({ threshold: thresholdNum, amount: amountNum }),
    );

    try {
      const data = await checkoutMutation.mutateAsync();
      if (data.url) {
        window.location.href = data.url;
      } else {
        toast.error("Failed to get checkout URL");
      }
    } catch (err) {
      console.error("Failed to start payment setup:", err);
      toast.error("Failed to start payment setup. Please try again.");
    }
  };

  // Manage mode: update settings via user update
  const handleUpdate = async () => {
    const thresholdNum = parseFloat(threshold);
    const amountNum = parseFloat(amount);
    if (isNaN(thresholdNum) || thresholdNum < 0) {
      toast.error("Please enter a valid threshold ($0 or more).");
      return;
    }
    if (isNaN(amountNum) || amountNum <= 0) {
      toast.error("Please enter a valid amount greater than $0.");
      return;
    }
    try {
      await updateUserMutation.mutateAsync({
        id: userId,
        data: {
          auto_topup_amount: amountNum,
          auto_topup_threshold: thresholdNum,
        },
      });
      toast.success(
        `Auto top-up updated: ${formatDollars(amountNum)} when balance drops below ${formatDollars(thresholdNum)}`,
      );
      onSuccess?.();
      onClose();
    } catch (err) {
      console.error("Failed to update auto top-up settings:", err);
      toast.error("Failed to update auto top-up settings.");
    }
  };

  const handleDisable = async () => {
    try {
      await updateUserMutation.mutateAsync({
        id: userId,
        data: { auto_topup_amount: null, auto_topup_threshold: null },
      });
      toast.success("Auto top-up disabled");
      onSuccess?.();
      onClose();
    } catch (err) {
      console.error("Failed to disable auto top-up:", err);
      toast.error("Failed to disable auto top-up.");
    }
  };

  return (
    <Dialog open={isOpen} onOpenChange={handleOpenChange}>
      <DialogContent className="sm:max-w-[50vw]">
        {mode === "processing" && (
          <>
            <DialogHeader>
              <DialogTitle className="flex items-center gap-2">
                {processSetupMutation.isPending ? (
                  <>
                    <Loader2 className="h-6 w-6 animate-spin text-blue-500" />
                    Setting Up Auto Top-Up
                  </>
                ) : processSetupMutation.isError ? (
                  <>
                    <XCircle className="h-6 w-6 text-red-500" />
                    Setup Failed
                  </>
                ) : (
                  <>
                    <CheckCircle2 className="h-6 w-6 text-green-500" />
                    Auto Top-Up Enabled
                  </>
                )}
              </DialogTitle>
              <DialogDescription>
                {processSetupMutation.isPending
                  ? "Verifying your payment method and saving your settings..."
                  : processSetupMutation.isError
                    ? "Failed to enable auto top-up. Your payment method was set up but the settings could not be saved. Please try again."
                    : "Your auto top-up has been configured successfully. Your account will be topped up automatically when your balance runs low."}
              </DialogDescription>
            </DialogHeader>
            <div className="flex justify-end gap-2 mt-4">
              <Button onClick={onClose}>Close</Button>
            </div>
          </>
        )}

        {mode === "cancelled" && (
          <>
            <DialogHeader>
              <DialogTitle className="flex items-center gap-2">
                <XCircle className="h-6 w-6 text-yellow-500" />
                Setup Cancelled
              </DialogTitle>
              <DialogDescription>
                Auto top-up setup was cancelled. No payment method has been
                configured. You can try again whenever you&apos;re ready.
              </DialogDescription>
            </DialogHeader>
            <div className="flex justify-end gap-2 mt-4">
              <Button variant="outline" onClick={onClose}>
                Close
              </Button>
            </div>
          </>
        )}

        {(mode === "setup" || mode === "manage") && (
          <>
            <DialogHeader>
              <DialogTitle>Auto Top-Up</DialogTitle>
              <DialogDescription>
                {mode === "manage"
                  ? "Manage your auto top-up settings. Your account will be automatically charged when your balance drops below the threshold."
                  : "Set up automatic balance replenishment. When your balance drops below the threshold, your account will be charged the top-up amount."}
              </DialogDescription>
            </DialogHeader>

            <div className="mt-4 space-y-6">
              {mode === "manage" && (
                <div className="rounded-lg border border-gray-200 bg-gray-50 p-4">
                  <p className="text-sm text-gray-600">
                    Currently: top up by{" "}
                    <span className="font-semibold text-gray-900">
                      {formatDollars(autoTopupAmount!)}
                    </span>{" "}
                    when balance drops below{" "}
                    <span className="font-semibold text-gray-900">
                      {formatDollars(autoTopupThreshold!)}
                    </span>
                  </p>
                </div>
              )}

              <div className="space-y-2">
                <Label htmlFor="topupThreshold">
                  When balance drops below (USD)
                </Label>
                <div className="flex items-center gap-2">
                  <span className="text-sm text-gray-500">$</span>
                  <Input
                    id="topupThreshold"
                    type="number"
                    min="0"
                    step="1"
                    value={threshold}
                    onChange={(e) => setThreshold(e.target.value)}
                    placeholder="5.00"
                    className="max-w-[200px] [appearance:textfield] [&::-webkit-outer-spin-button]:appearance-none [&::-webkit-inner-spin-button]:appearance-none"
                  />
                </div>
              </div>

              <div className="space-y-2">
                <Label htmlFor="topupAmount">Top up by (USD)</Label>
                <div className="flex items-center gap-2">
                  <span className="text-sm text-gray-500">$</span>
                  <Input
                    id="topupAmount"
                    type="number"
                    min="1"
                    step="5"
                    value={amount}
                    onChange={(e) => setAmount(e.target.value)}
                    placeholder="25.00"
                    className="max-w-[200px] [appearance:textfield] [&::-webkit-outer-spin-button]:appearance-none [&::-webkit-inner-spin-button]:appearance-none"
                  />
                </div>
              </div>
            </div>

            <div className="flex items-center justify-between mt-6">
              <div>
                {mode === "manage" && (
                  <Button
                    variant="ghost"
                    size="sm"
                    onClick={handleDisable}
                    disabled={updateUserMutation.isPending}
                    className="text-red-600 hover:text-red-700 hover:bg-red-50"
                  >
                    Disable Auto Top-Up
                  </Button>
                )}
              </div>
              <div className="flex gap-2">
                <Button
                  variant="outline"
                  onClick={onClose}
                  disabled={
                    updateUserMutation.isPending || checkoutMutation.isPending
                  }
                >
                  Cancel
                </Button>
                {mode === "setup" ? (
                  <Button
                    onClick={handleSetupPayment}
                    disabled={checkoutMutation.isPending}
                  >
                    {checkoutMutation.isPending
                      ? "Redirecting..."
                      : "Configure Payment"}
                  </Button>
                ) : (
                  <Button
                    onClick={handleUpdate}
                    disabled={updateUserMutation.isPending}
                  >
                    {updateUserMutation.isPending ? "Saving..." : "Update"}
                  </Button>
                )}
              </div>
            </div>
          </>
        )}
      </DialogContent>
    </Dialog>
  );
}
