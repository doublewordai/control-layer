import { X } from "lucide-react";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "../../ui/dialog";
import { useTransactions } from "../../../api/control-layer/hooks";
import { useSettings } from "../../../contexts";
import type { DisplayUser } from "../../../types/display";
import {TransactionHistory} from "@/components";
import {generateDummyTransactions} from "@/components/features/cost-management/demoTransactions.ts";

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

  const {
    data: transactionsData,
    isLoading: isLoadingTransactions,
  } = useTransactions({ userId: user.id });


  const transactions = transactionsData?.transactions || (isDemoMode? (generateDummyTransactions().filter((t) => t.user_id === user.id)) : []);
  const isLoading = !isDemoMode && isLoadingTransactions;

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="max-w-5xl max-h-[80vh] overflow-y-auto">
        <DialogHeader>
          <div className="flex items-center justify-between">
            <div>
              <DialogTitle className="text-2xl">Transaction History</DialogTitle>
              <p className="text-sm text-doubleword-neutral-600 mt-1">
                Viewing transactions for <strong>{user.name}</strong> ({user.email})
              </p>
            </div>
            <button
              onClick={onClose}
              className="text-doubleword-neutral-400 hover:text-doubleword-neutral-600 transition-colors"
              aria-label="Close modal"
            >
              <X className="w-5 h-5" />
            </button>
          </div>
        </DialogHeader>

        <div className="mt-4">
          <TransactionHistory
            transactions={transactions}
            isLoading={isLoading}
            showCard={false}
          />
        </div>
      </DialogContent>
    </Dialog>
  );
}
