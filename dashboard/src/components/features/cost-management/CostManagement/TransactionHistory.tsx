import { TrendingDown, TrendingUp, Filter, X, Plus, DollarSign } from "lucide-react";
import { Card } from "../../../ui/card.tsx";
import { Button } from "@/components";
import { useState, useMemo } from "react";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../../ui/select.tsx";
import { DateTimeRangeSelector } from "../../../ui/date-time-range-selector.tsx";
import type { Transaction } from "@/api/control-layer";
import {
  useUserBalance,
  useTransactions,
} from "@/api/control-layer";
import { useSettings } from "@/contexts";

export interface TransactionHistoryProps {
  userId: string;
  showCard?: boolean;
  onAddFunds?: () => void;
  isAddingFunds?: boolean;
}

export function TransactionHistory({
  userId,
  showCard = true,
  onAddFunds,
  isAddingFunds = false,
}: TransactionHistoryProps) {
  const { isFeatureEnabled } = useSettings();
  const isDemoMode = isFeatureEnabled("demo");

  // Fetch balance and transactions
  const { data: balance = 0, isLoading: isLoadingBalance } = useUserBalance(userId);

  // Only pass userId if it's not "current" (which defaults to current user)
  const transactionsQuery = userId === "current" ? undefined : { userId };
  const {
    data: transactionsData,
    isLoading: isLoadingTransactions,
  } = useTransactions(transactionsQuery);

  // Get transactions - use fetched data in both demo and API mode
  // In demo mode, MSW returns data from transactions.json
  const transactions = useMemo<Transaction[]>(() => {
    return transactionsData || [];
  }, [transactionsData]);

  // Calculate current balance (in demo mode, use latest transaction balance)
  const currentBalance = isDemoMode && transactions.length > 0
    ? transactions[0]?.balance_after || balance
    : balance;

  const isLoading = !isDemoMode && (isLoadingBalance || isLoadingTransactions);

  // Filter states
  const [transactionType, setTransactionType] = useState<string>("all");
  const [dateRange, setDateRange] = useState<{ from: Date; to: Date } | undefined>();
  // Apply filters
  const filteredTransactions = useMemo(() => {
    let filtered = [...transactions];

    // Filter by transaction type
    if (transactionType !== "all") {
      // Map UI filter values to backend transaction types
      if (transactionType === "credit") {
        filtered = filtered.filter((t) =>
          t.transaction_type === "admin_grant" || t.transaction_type === "purchase"
        );
      } else if (transactionType === "debit") {
        filtered = filtered.filter((t) =>
          t.transaction_type === "admin_removal" || t.transaction_type === "usage"
        );
      }
    }

    // Filter by date range
    if (dateRange?.from && dateRange?.to) {
      filtered = filtered.filter((t) => {
        const transactionDate = new Date(t.created_at);
        return transactionDate >= dateRange.from && transactionDate <= dateRange.to;
      });
    }

    return filtered;
  }, [transactions, transactionType, dateRange]);

  const hasActiveFilters =
    transactionType !== "all" ||
    dateRange !== undefined;

  const clearFilters = () => {
    setTransactionType("all");
    setDateRange(undefined);
  };

  const handleTransactionTypeChange = (value: string) => {
    setTransactionType(value);
  };

  const formatDate = (isoString: string) => {
    const date = new Date(isoString);
    return new Intl.DateTimeFormat("en-US", {
      month: "short",
      day: "numeric",
      year: "numeric",
      hour: "2-digit",
      minute: "2-digit",
    }).format(date);
  };

  const formatDollars = (amount: number) => {
    return new Intl.NumberFormat("en-US", {
      style: "currency",
      currency: "USD",
    }).format(amount);
  };

  const content = (
    <>
      {/* Current Balance Card */}
      <Card className="p-6 bg-gradient-to-br from-blue-50 to-indigo-50 border-blue-200 mb-6">
        <h2 className="text-xl font-semibold text-doubleword-neutral-900 mb-4">
          Current Balance
        </h2>
        <div className="flex items-center justify-between">
          <div className="flex items-baseline gap-2">
            <span className="text-4xl font-bold text-doubleword-neutral-900">
              {formatDollars(currentBalance)}
            </span>
          </div>
          {onAddFunds && (
            <Button
              className="bg-blue-600 hover:bg-blue-700"
              size="lg"
              onClick={onAddFunds}
              disabled={isAddingFunds}
            >
              <Plus className="w-5 h-5 mr-2" />
              {isAddingFunds ? "Adding..." : "Add Funds"}
            </Button>
          )}
        </div>
      </Card>

      {/* Transaction History Card */}
      <Card className="p-6">
        <h2 className="text-xl font-semibold text-doubleword-neutral-900 mb-4">
          Transaction History
        </h2>

        {/* Filters */}
        <div className="mb-4 space-y-4">
          <div className="flex flex-wrap items-center gap-4">
            <div className="flex items-center gap-2">
              <Filter className="w-4 h-4 text-doubleword-neutral-600" />
              <span className="text-sm font-medium text-doubleword-neutral-700">
                Filters:
              </span>
            </div>

            {/* Transaction Type Filter */}
            <div className="flex items-center gap-2">
              <label className="text-sm text-doubleword-neutral-600 whitespace-nowrap">
                Type:
              </label>
              <Select value={transactionType} onValueChange={handleTransactionTypeChange}>
                <SelectTrigger className="w-[150px]">
                  <SelectValue placeholder="All types" />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="all">All types</SelectItem>
                  <SelectItem value="credit">Deposits only</SelectItem>
                  <SelectItem value="debit">Withdrawals only</SelectItem>
                </SelectContent>
              </Select>
            </div>

            {/* Date Range Filter */}
            <div className="flex items-center gap-2">
              <label className="text-sm text-doubleword-neutral-600 whitespace-nowrap">
                Date Range:
              </label>
              <DateTimeRangeSelector
                value={dateRange}
                onChange={setDateRange}
              />
            </div>

            {hasActiveFilters && (
              <Button
                variant="ghost"
                size="sm"
                onClick={clearFilters}
                className="h-8 px-3 text-xs"
              >
                <X className="w-3 h-3 mr-1" />
                Clear filters
              </Button>
            )}
          </div>

          {/* Filter Status */}
          {hasActiveFilters && (
            <div className="text-sm text-doubleword-neutral-600">
              Showing {filteredTransactions.length} of {transactions.length}{" "}
              transactions
            </div>
          )}
        </div>

        <div className="space-y-2">
          {filteredTransactions.map((transaction) => {
          // Determine if transaction is a credit (adds money) or debit (removes money)
          const isCredit =
            transaction.transaction_type === "admin_grant" ||
            transaction.transaction_type === "purchase";

          return (
            <div
              key={transaction.id}
              className="flex items-center justify-between p-4 border border-doubleword-neutral-200 rounded-lg hover:bg-gray-50 transition-colors"
            >
              <div className="flex items-center gap-4 flex-1">
                <div
                  className={`p-2 rounded-full ${
                    isCredit ? "bg-green-100" : "bg-red-100"
                  }`}
                >
                  {isCredit ? (
                    <TrendingUp className="w-5 h-5 text-green-600" />
                  ) : (
                    <TrendingDown className="w-5 h-5 text-red-600" />
                  )}
                </div>
                <div className="flex-1">
                  <p className="font-medium text-doubleword-neutral-900">
                    {transaction.description || "No description"}
                  </p>
                  <p className="text-sm text-doubleword-neutral-600">
                    {formatDate(transaction.created_at)}
                  </p>
                </div>
              </div>
              <div className="text-right">
                <p
                  className={`font-semibold ${
                    isCredit ? "text-green-600" : "text-red-600"
                  }`}
                >
                  {isCredit ? "+" : "-"}
                  {formatDollars(transaction.amount)}
                </p>
                <p className="text-sm text-doubleword-neutral-600">
                  Balance: {formatDollars(transaction.balance_after)}
                </p>
              </div>
            </div>
          );
        })}
        </div>

        {filteredTransactions.length === 0 && (
          <div className="text-center py-12">
            <DollarSign className="w-12 h-12 text-doubleword-neutral-300 mx-auto mb-3" />
            <p className="text-doubleword-neutral-600">
              {hasActiveFilters
                ? "No transactions match your filters"
                : "No transactions yet"}
            </p>
            {hasActiveFilters && (
              <Button
                variant="outline"
                size="sm"
                onClick={clearFilters}
                className="mt-4"
              >
                Clear filters
              </Button>
            )}
          </div>
        )}
      </Card>
    </>
  );

  if (isLoading) {
    return (
      <Card className="p-8 text-center">
        <div className="animate-spin rounded-full h-12 w-12 border-b-2 border-doubleword-accent-blue mx-auto mb-4"></div>
        <p className="text-doubleword-neutral-600">Loading transactions...</p>
      </Card>
    );
  }

  if (showCard) {
    return <Card className="p-6">{content}</Card>;
  }

  return <div>{content}</div>;
}