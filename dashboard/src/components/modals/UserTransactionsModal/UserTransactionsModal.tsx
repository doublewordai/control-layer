import { useState } from "react";
import { X, Plus } from "lucide-react";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "../../ui/dialog";
import { Button } from "../../ui/button";
import { useTransactions } from "../../../api/control-layer/hooks";
import { useSettings } from "../../../contexts";
import type { DisplayUser } from "../../../types/display";
import {TransactionHistory} from "@/components";
import {generateDummyTransactions} from "@/components/features/cost-management/demoTransactions.ts";
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
  const { isFeatureEnabled } = useSettings();
  const isDemoMode = isFeatureEnabled("demo");
  const [isAddFundsModalOpen, setIsAddFundsModalOpen] = useState(false);

  const {
    data: transactionsData,
    isLoading: isLoadingTransactions,
    refetch: refetchTransactions,
  } = useTransactions({ userId: user.id });


  const transactions = transactionsData || (isDemoMode ? (generateDummyTransactions().filter((t) => t.user_id === user.id)) : []);
  const isLoading = !isDemoMode && isLoadingTransactions;

  // Get balance from user (API mode) or from latest transaction (demo mode)
  const balance = isDemoMode
    ? transactions[0]?.balance_after || user.credit_balance || 0
    : user.credit_balance || 0;

  const handleAddFundsSuccess = () => {
    refetchTransactions();
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
                {!isDemoMode && (
                  <Button
                    className="bg-blue-600 hover:bg-blue-700"
                    size="sm"
                    onClick={() => setIsAddFundsModalOpen(true)}
                  >
                    <Plus className="w-4 h-4 mr-1" />
                    Add Funds
                  </Button>
                )}
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
              transactions={transactions}
              balance={balance}
              isLoading={isLoading}
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
