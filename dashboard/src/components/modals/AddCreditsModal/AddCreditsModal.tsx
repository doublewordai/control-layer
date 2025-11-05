import { useState } from "react";
import { X } from "lucide-react";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "../../ui/dialog";
import { Button } from "../../ui/button";
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

    const amountNum = parseFloat(amount);
    if (isNaN(amountNum) || amountNum <= 0) {
      toast.error("Please enter a valid amount");
      return;
    }

    try {
      const result = await addFundsMutation.mutateAsync({
        user_id: targetUser.id,
        amount: amountNum,
        description: description || `Funds gift from ${currentUser?.display_name || currentUser?.username || "admin"}`,
      });

      toast.success(`Successfully added $${result.amount.toFixed(2)} to ${targetUser.name}`);
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
          <div className="flex items-center justify-between">
            <DialogTitle className="text-2xl">Add Funds</DialogTitle>
            <button
              onClick={onClose}
              className="text-doubleword-neutral-400 hover:text-doubleword-neutral-600 transition-colors"
              aria-label="Close modal"
            >
              <X className="w-5 h-5" />
            </button>
          </div>
        </DialogHeader>

        <form onSubmit={handleSubmit} className="space-y-4 mt-4">
          <div>
            <p className="text-sm text-doubleword-neutral-600 mb-4">
              You are about to add funds to{" "}
              <strong>{targetUser.name}</strong> ({targetUser.email})
            </p>
          </div>

          <div>
            <label
              htmlFor="amount"
              className="block text-sm font-medium text-doubleword-neutral-700 mb-1"
            >
              Amount (USD)
            </label>
            <input
              id="amount"
              type="number"
              min="0"
              step="0.01"
              value={amount}
              onChange={(e) => setAmount(e.target.value)}
              className="w-full px-3 py-2 border border-doubleword-neutral-300 rounded-md focus:outline-none focus:ring-2 focus:ring-blue-500"
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
            <textarea
              id="description"
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              className="w-full px-3 py-2 border border-doubleword-neutral-300 rounded-md focus:outline-none focus:ring-2 focus:ring-blue-500"
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
              className="bg-blue-600 hover:bg-blue-700"
              disabled={addFundsMutation.isPending}
            >
              {addFundsMutation.isPending ? "Adding..." : "Add Funds"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}
