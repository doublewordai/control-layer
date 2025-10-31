import { Plus } from "lucide-react";
import { Card } from "../../../ui/card";
import { Button } from "../../../ui/button";
import { useState } from "react";
import {
  useCreditBalance,
  useAddCredits, useTransactions,
} from "@/api/control-layer";
import { toast } from "sonner";
import { useMemo } from "react";
import {useSettings} from "@/contexts";
import {useAuth} from "@/contexts/auth";
import {generateDummyTransactions} from "@/components/features/cost-management/demoTransactions.ts";
import {
  type Transaction,
  TransactionHistory
} from "@/components/features/cost-management/CostManagement/TransactionHistory.tsx";

export function CostManagement() {
  const { isFeatureEnabled } = useSettings();
  const isDemoMode = isFeatureEnabled("demo");
  const { user } = useAuth();

  // Get user's transactions
  const [localTransactions, setLocalTransactions] = useState<Transaction[]>([]);

  // API mode hooks (only fetch when not in demo mode)
  const { data: balanceData, isLoading: isLoadingBalance } = useCreditBalance();
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
      // API mode: use data from API
      return transactionsData?.transactions || [];
    }
  }, [isDemoMode, user?.id, transactionsData]);

  // Use local transactions if any have been added, otherwise use fetched transactions
  const displayTransactions = localTransactions.length > 0
      ? [...localTransactions, ...transactions]
      : transactions;

  const currentBalance = isDemoMode
    ? displayTransactions[0]?.balance_after || 0
    : balanceData?.balance || 0;
  const isLoading = !isDemoMode && (isLoadingBalance || isLoadingTransactions);

  const formatCredits = (amount: number) => {
    return new Intl.NumberFormat("en-US").format(amount);
  };

  const handleAddCredits = async () => {
    if (isDemoMode) {
      // Demo mode: Add transaction locally
      const creditAmount = 1000;
      const newBalance = currentBalance + creditAmount;
      const newTransaction: Transaction = {
        id: `demo-${Date.now()}`,
        type: "credit",
        amount: creditAmount,
        description: "Credit purchase - Demo top up",
        timestamp: new Date().toISOString(),
        balance_after: newBalance,
        user_id: user?.id,
      };
      setLocalTransactions([newTransaction, ...localTransactions]);
      toast.success(`Added ${creditAmount} credits`);
    } else {
      // API mode: Call the add credits endpoint
      try {
        const result = await addCreditsMutation.mutateAsync({
          amount: 1000,
          description: "Credit purchase - Top up",
        });
        toast.success(`Added ${result.transaction.amount} credits`);
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

        {isLoading ? (
          <Card className="p-8 text-center mb-8">
            <div className="animate-spin rounded-full h-12 w-12 border-b-2 border-doubleword-accent-blue mx-auto mb-4"></div>
            <p className="text-doubleword-neutral-600">Loading...</p>
          </Card>
        ) : (
          <>

        {/* Current Balance Card */}
        <Card className="mb-8 p-6 bg-gradient-to-br from-blue-50 to-indigo-50 border-blue-200">
          <div className="flex items-center justify-between">
            <div>
              <p className="text-sm text-doubleword-neutral-600 mb-1">
                Current Balance
              </p>
              <div className="flex items-baseline gap-2">
                <h2 className="text-4xl font-bold text-doubleword-neutral-900">
                  {formatCredits(currentBalance)}
                </h2>
                <span className="text-lg text-doubleword-neutral-600">
                  credits
                </span>
              </div>
            </div>
            <Button
              className="bg-blue-600 hover:bg-blue-700"
              size="lg"
              onClick={handleAddCredits}
              disabled={addCreditsMutation.isPending}
            >
              <Plus className="w-5 h-5 mr-2" />
              {addCreditsMutation.isPending ? "Adding..." : "Add Credits"}
            </Button>
          </div>
        </Card>

        {/* Transaction History */}
        <TransactionHistory
          transactions={displayTransactions}
          isLoading={isLoading}
        />
        </>
        )}
      </div>
    </div>
  );
}
