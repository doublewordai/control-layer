import { useState } from "react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "../../ui/dialog";
import { Button } from "../../ui/button";
import { Input } from "../../ui/input";
import { Textarea } from "../../ui/textarea";
import { Tabs, TabsList, TabsTrigger } from "../../ui/tabs";
import { useAddFunds, useUser } from "../../../api/control-layer/hooks";
import { toast } from "sonner";
import type { DisplayUser } from "../../../types/display";
import type { AdminTransactionType } from "../../../api/control-layer/types";
import { AlertBox } from "@/components/ui/alert-box";

interface AddFundsModalProps {
  isOpen: boolean;
  onClose: () => void;
  targetUser: DisplayUser;
  onSuccess?: () => void;
}

type FundsMode = "add" | "remove";

const MODE_TO_TRANSACTION_TYPE: Record<FundsMode, AdminTransactionType> = {
  add: "admin_grant",
  remove: "admin_removal",
};

/**
 * Admin dialog for adjusting a user's credit balance in either direction.
 *
 * "Add" creates an `admin_grant`; "Remove" creates an `admin_removal`. The
 * amount is always entered as a positive number and the mode decides the sign.
 */
export function AddFundsModal({
  isOpen,
  onClose,
  targetUser,
  onSuccess,
}: AddFundsModalProps) {
  const [mode, setMode] = useState<FundsMode>("add");
  const [amount, setAmount] = useState<string>("10.00");
  const [description, setDescription] = useState<string>("");
  const addFundsMutation = useAddFunds();
  const { data: currentUser } = useUser("current");
  const [error, setError] = useState<string | null>(null);

  const targetLabel = targetUser.display_name || targetUser.email;
  const isRemove = mode === "remove";
  const verb = isRemove ? "remove" : "add";

  const handleModeChange = (value: string) => {
    setMode(value as FundsMode);
    setError(null);
  };

  // The parent keeps this component mounted and only toggles `isOpen`, so
  // every close path must reset the form. Otherwise cancelling after picking
  // "Remove" would silently reopen in removal mode next time.
  const resetForm = () => {
    setMode("add");
    setAmount("10.00");
    setDescription("");
    setError(null);
  };

  const handleClose = () => {
    resetForm();
    onClose();
  };

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();

    // Guard: Ensure currentUser is loaded
    if (!currentUser?.id) {
      setError(`Unable to ${verb} funds. Please try refreshing the page.`);
      return;
    }

    const amountNum = parseFloat(amount);
    if (isNaN(amountNum) || amountNum <= 0) {
      setError("Please enter a valid amount");
      return;
    }

    const adminName =
      currentUser.display_name || currentUser.username || "admin";

    try {
      const result = await addFundsMutation.mutateAsync({
        user_id: targetUser.id,
        source_id: `${currentUser.id}_${crypto.randomUUID()}`,
        amount: amountNum,
        transaction_type: MODE_TO_TRANSACTION_TYPE[mode],
        description:
          description ||
          (isRemove
            ? `Funds removed by ${adminName}`
            : `Funds gift from ${adminName}`),
      });

      const sentAmount = Number(result.amount).toFixed(2);

      toast.success(
        isRemove
          ? `Successfully removed $${sentAmount} from ${targetLabel}`
          : `Successfully added $${sentAmount} to ${targetLabel}`,
      );
      onSuccess?.();
      handleClose();
    } catch (error) {
      setError(`Failed to ${verb} funds. Please try again.`);
      console.error(`Failed to ${verb} funds:`, error);
    }
  };

  return (
    <Dialog open={isOpen} onOpenChange={(open) => !open && handleClose()}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle className="text-2xl">
            {isRemove ? "Remove from Credit Balance" : "Add to Credit Balance"}
          </DialogTitle>
          <DialogDescription>
            You are about to {verb} funds {isRemove ? "from" : "to"}{" "}
            <strong>{targetLabel}</strong>
            {targetUser.display_name && ` (${targetUser.email})`}
          </DialogDescription>
        </DialogHeader>

        <AlertBox variant="error" className="mb-4">
          {error}
        </AlertBox>

        <form onSubmit={handleSubmit} className="space-y-4 mt-4">
          <Tabs value={mode} onValueChange={handleModeChange}>
            <TabsList className="w-full" aria-label="Adjustment type">
              <TabsTrigger value="add" className="flex-1">
                Add funds
              </TabsTrigger>
              <TabsTrigger value="remove" className="flex-1">
                Remove funds
              </TabsTrigger>
            </TabsList>
          </Tabs>

          <div>
            <label
              htmlFor="amount"
              className="block text-sm font-medium text-doubleword-neutral-700 mb-1"
            >
              Amount (USD)
            </label>
            <Input
              id="amount"
              type="number"
              min="0"
              step="0.01"
              value={amount}
              onChange={(e) => setAmount(e.target.value)}
              placeholder="10.00"
              required
            />
            {isRemove && (
              <p className="mt-1 text-xs text-doubleword-neutral-500">
                This amount will be deducted from the current balance.
              </p>
            )}
          </div>

          <div>
            <label
              htmlFor="description"
              className="block text-sm font-medium text-doubleword-neutral-700 mb-1"
            >
              Description (optional)
            </label>
            <Textarea
              id="description"
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              placeholder="Enter description"
              rows={3}
            />
          </div>

          <div className="flex gap-3 justify-end pt-4">
            <Button
              type="button"
              variant="outline"
              onClick={handleClose}
              disabled={addFundsMutation.isPending}
            >
              Cancel
            </Button>
            <Button type="submit" disabled={addFundsMutation.isPending}>
              {addFundsMutation.isPending
                ? isRemove
                  ? "Removing..."
                  : "Adding..."
                : isRemove
                  ? "Remove from Credit Balance"
                  : "Add to Credit Balance"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}
