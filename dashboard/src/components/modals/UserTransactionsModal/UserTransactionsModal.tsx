import { useState } from "react";
import { Plus } from "lucide-react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "../../ui/dialog";
import { Button } from "../../ui/button";
import type { DisplayUser } from "../../../types/display";
import { TransactionHistory } from "../../features/cost-management/CostManagement/TransactionHistory";
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
        <DialogContent className="max-w-[50vw]! max-h-[80vh] overflow-y-auto [&>button]:hidden">
          <DialogHeader>
            <div className="flex items-center justify-between">
              <div>
                <DialogTitle className="text-2xl">
                  Transaction History
                </DialogTitle>
                <DialogDescription>
                  Viewing transactions for <strong>{user.name}</strong> (
                  {user.email})
                </DialogDescription>
              </div>

              <Button
                className="bg-blue-600 hover:bg-blue-700"
                size="sm"
                onClick={() => setIsAddFundsModalOpen(true)}
              >
                <Plus className="w-4 h-4 mr-1" />
                Add Funds
              </Button>
            </div>
          </DialogHeader>

          <div className="mt-4">
            <TransactionHistory userId={user.id} showCard={false} />
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
