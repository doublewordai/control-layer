import { useState, useEffect } from "react";
import {
  useUser,
  useAddFunds,
  useTransactions,
} from "@/api/control-layer";
import { toast } from "sonner";
import { useMemo } from "react";
import { useSettings } from "@/contexts";
import { generateDummyTransactions } from "@/components/features/cost-management/demoTransactions.ts";
import {
  TransactionHistory
} from "@/components/features/cost-management/CostManagement/TransactionHistory.tsx";
import type { Transaction } from "@/api/control-layer";

export function CostManagement() {
  const { isFeatureEnabled, settings } = useSettings();
  const isDemoMode = isFeatureEnabled("demo");

  // Get user's transactions
  const [localTransactions, setLocalTransactions] = useState<Transaction[]>([]);

  // Fetch current user (includes balance)
  const { data: user, isLoading: isLoadingUser, refetch: refetchUser } = useUser("current");
  const addFundsMutation = useAddFunds();
  const {
    data: transactionsData,
    isLoading: isLoadingTransactions,
    refetch: refetchTransactions,
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

  // Handle return from payment provider
  useEffect(() => {
    if (isDemoMode) return;

    const urlParams = new URLSearchParams(window.location.search);
    const paymentComplete = urlParams.get('payment_complete');

    if (paymentComplete === 'true') {
      // Show success message
      toast.success('Payment completed! Your balance has been updated.');

      // Refetch user data and transactions to get latest balance
      refetchUser();
      refetchTransactions();

      // Clean up URL
      window.history.replaceState({}, '', window.location.pathname);
    }
  }, [isDemoMode, refetchUser, refetchTransactions]);

  const handleAddFunds = async () => {
    if (isDemoMode) {
      // Demo mode: Add transaction locally
      const fundAmount = 100.00;
      const newBalance = currentBalance + fundAmount;
      const newTransaction: Transaction = {
        id: `demo-${Date.now()}`,
        user_id: user?.id || "",
        transaction_type: "admin_grant",
        amount: fundAmount,
        balance_after: newBalance,
        previous_transaction_id: displayTransactions[0]?.id,
        source_id: "DEMO_GIFT",
        description: "Funds purchase - Demo top up",
        created_at: new Date().toISOString(),
      };
      setLocalTransactions([newTransaction, ...localTransactions]);
      toast.success(`Added $${fundAmount.toFixed(2)}`);
    } else {
      // API mode: Redirect to payment provider
      const paymentProviderUrl = settings.paymentProviderUrl;

      // Build callback URL to return to this page
      const callbackUrl = `${window.location.origin}${window.location.pathname}?payment_complete=true`;

      // Redirect to payment provider with callback URL
      const redirectUrl = `${paymentProviderUrl}?callback=${encodeURIComponent(callbackUrl)}`;
      window.location.href = redirectUrl;
    }
  };

  // Only show Add Funds button if in demo mode or payment provider is configured
  const canAddFunds = isDemoMode || !!settings.paymentProviderUrl;

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
          onAddFunds={canAddFunds ? handleAddFunds : undefined}
          isAddingFunds={addFundsMutation.isPending}
        />
      </div>
    </div>
  );
}
