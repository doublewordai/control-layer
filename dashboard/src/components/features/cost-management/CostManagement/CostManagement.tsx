import { useState } from "react";
import {
  useUser,
  useAddCredits,
  useTransactions,
} from "@/api/control-layer";
import { toast } from "sonner";
import { useMemo } from "react";
import { useSettings } from "@/contexts";
import { generateDummyTransactions } from "@/components/features/cost-management/demoTransactions.ts";
import {
  type Transaction,
  TransactionHistory
} from "@/components/features/cost-management/CostManagement/TransactionHistory.tsx";

export function CostManagement() {
  const { isFeatureEnabled } = useSettings();
  const isDemoMode = isFeatureEnabled("demo");

  // Get user's transactions
  const [localTransactions, setLocalTransactions] = useState<Transaction[]>([]);

  // Fetch current user (includes balance)
  const { data: user, isLoading: isLoadingUser } = useUser("current");
  const addCreditsMutation = useAddCredits();
  const {
    data: transactionsData,
    isLoading: isLoadingTransactions,
  } = useTransactions();

  // Get transactions based on mode
  const transactions = useMemo<Transaction[]>(() => {
    if (isDemoMode) {
      // Demo mode: filter transactions by current user
      if (!user?.id) return [];
      const allTransactions = generateDummyTransactions();
      return allTransactions
        .filter((t) => t.user_id === user.id)
        .reverse(); // Most recent first
    } else {
      // API mode: use data from API (now returns array directly)
      return transactionsData || [];
    }
  }, [isDemoMode, user?.id, transactionsData]);

  // Use local transactions if any have been added, otherwise use fetched transactions
  const displayTransactions = localTransactions.length > 0
      ? [...localTransactions, ...transactions]
      : transactions;

  const currentBalance = isDemoMode
    ? displayTransactions[0]?.balance_after || user?.credit_balance || 0
    : user?.credit_balance || 0;
  const isLoading = !isDemoMode && (isLoadingUser || isLoadingTransactions);
  console.log("userbalance", user)


  const handleAddCredits = async () => {
    if (isDemoMode) {
      // Demo mode: Add transaction locally
      const creditAmount = 1000;
      const newBalance = currentBalance + creditAmount;
      const newTransaction: Transaction = {
        id: `demo-${Date.now()}`,
        user_id: user?.id || "",
        transaction_type: "admin_grant",
        amount: creditAmount,
        balance_after: newBalance,
        previous_transaction_id: displayTransactions[0]?.id,
        source_id: "DEMO_GIFT",
        description: "Credit purchase - Demo top up",
        created_at: new Date().toISOString(),
      };
      setLocalTransactions([newTransaction, ...localTransactions]);
      toast.success(`Added ${creditAmount} credits`);
    } else {
      // API mode: Call the add credits endpoint
      try {
        if (!user?.id) {
          toast.error("User not found. Please refresh and try again.");
          return;
        }

        const result = await addCreditsMutation.mutateAsync({
          user_id: user.id,
          amount: 1000,
          description: "Credit purchase - Top up",
        });
        toast.success(`Added ${result.amount} credits`);
      } catch (error) {
        toast.error("Failed to add credits. Please try again.");
        console.error("Failed to add credits:", error);
      }
    }
  };

  return (
    <div className="p-8">
      <div className="max-w-7xl mx-auto">
        <div className="flex items-center justify-between mb-8">
          <div>
            <h1 className="text-3xl font-bold text-doubleword-neutral-900 mb-2">
              Cost Management
            </h1>
            <p className="text-doubleword-neutral-600">
              Monitor your credit balance and transaction history
            </p>
          </div>
        </div>

        <TransactionHistory
          transactions={displayTransactions}
          balance={currentBalance}
          isLoading={isLoading}
          onAddCredits={handleAddCredits}
          isAddingCredits={addCreditsMutation.isPending}
        />
      </div>
    </div>
  );
}
