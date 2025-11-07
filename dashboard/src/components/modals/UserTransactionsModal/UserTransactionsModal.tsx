import { useState } from "react";
import { X, Plus } from "lucide-react";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "../../ui/dialog";
import { Button } from "../../ui/button";
import type { DisplayUser } from "../../../types/display";
import {TransactionHistory} from "@/components";
import { AddFundsModal } from "../AddCreditsModal";

interface UserTransactionsModalProps {
  isOpen: boolean;
  onClose: () => void;
  user: DisplayUser;
}

export function UserTransactionsModal({
  isOpen,
  onClose,
  user,
}: UserTransactionsModalProps) {
  const [isAddFundsModalOpen, setIsAddFundsModalOpen] = useState(false);

  const handleAddFundsSuccess = () => {
    // TransactionHistory will automatically refetch its data
  };

  return (
    <>
      <Dialog open={isOpen} onOpenChange={onClose}>
        <DialogContent className="!max-w-[50vw] max-h-[80vh] overflow-y-auto">
          <DialogHeader>
            <div className="flex items-center justify-between">
              <div className="flex-1">
                <DialogTitle className="text-2xl">Transaction History</DialogTitle>
                <p className="text-sm text-doubleword-neutral-600 mt-1">
                  Viewing transactions for <strong>{user.name}</strong> ({user.email})
                </p>
              </div>
              <div className="flex items-center gap-2">
                <Button
                  className="bg-blue-600 hover:bg-blue-700"
                  size="sm"
                  onClick={() => setIsAddFundsModalOpen(true)}
                >
                  <Plus className="w-4 h-4 mr-1" />
                  Add Funds
                </Button>
                <button
                  onClick={onClose}
                  className="text-doubleword-neutral-400 hover:text-doubleword-neutral-600 transition-colors"
                  aria-label="Close modal"
                >
                  <X className="w-5 h-5" />
                </button>
              </div>
            </div>
          </DialogHeader>

          <div className="mt-4">
            <TransactionHistory
              userId={user.id}
              showCard={false}
            />
          </div>
        </DialogContent>
      </Dialog>

      <AddFundsModal
        isOpen={isAddFundsModalOpen}
        onClose={() => setIsAddFundsModalOpen(false)}
        targetUser={user}
        onSuccess={handleAddFundsSuccess}
      />
    </>
  );
}
