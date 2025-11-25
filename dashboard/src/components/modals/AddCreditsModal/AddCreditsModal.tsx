import { useState } from "react";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "../../ui/dialog";
import { Button } from "../../ui/button";
import { Input } from "../../ui/input";
import { Textarea } from "../../ui/textarea";
import { useAddFunds, useUser } from "../../../api/control-layer/hooks";
import { toast } from "sonner";
import type { DisplayUser } from "../../../types/display";

interface AddFundsModalProps {
  isOpen: boolean;
  onClose: () => void;
  targetUser: DisplayUser;
  onSuccess?: () => void;
}

export function AddFundsModal({
  isOpen,
  onClose,
  targetUser,
  onSuccess,
}: AddFundsModalProps) {
  const [amount, setAmount] = useState<string>("10.00");
  const [description, setDescription] = useState<string>("");
  const addFundsMutation = useAddFunds();
  const { data: currentUser } = useUser("current");

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();

    // Guard: Ensure currentUser is loaded
    if (!currentUser?.id) {
      toast.error("Unable to add funds. Please try refreshing the page.");
      return;
    }

    const amountNum = parseFloat(amount);
    if (isNaN(amountNum) || amountNum <= 0) {
      toast.error("Please enter a valid amount");
      return;
    }

    try {
      const result = await addFundsMutation.mutateAsync({
        user_id: targetUser.id,
        source_id: `${currentUser.id}_${crypto.randomUUID()}`,
        amount: amountNum,
        description:
          description ||
          `Funds gift from ${currentUser.display_name || currentUser.username || "admin"}`,
      });

      const sentAmount = Number(result.amount).toFixed(2);

      toast.success(`Successfully added $${sentAmount} to ${targetUser.name}`);
      onSuccess?.();
      onClose();

      // Reset form
      setAmount("10.00");
      setDescription("");
    } catch (error) {
      toast.error("Failed to add funds. Please try again.");
      console.error("Failed to add funds:", error);
    }
  };

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle className="text-2xl">Add to Credit Balance</DialogTitle>
        </DialogHeader>

        <form onSubmit={handleSubmit} className="space-y-4 mt-4">
          <div>
            <p className="text-sm text-doubleword-neutral-600 mb-4">
              You are about to add funds to <strong>{targetUser.name}</strong> (
              {targetUser.email})
            </p>
          </div>

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
              onClick={onClose}
              disabled={addFundsMutation.isPending}
            >
              Cancel
            </Button>
            <Button
              type="submit"
              disabled={addFundsMutation.isPending}
            >
              {addFundsMutation.isPending ? "Adding..." : "Add to Credit Balance"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}
