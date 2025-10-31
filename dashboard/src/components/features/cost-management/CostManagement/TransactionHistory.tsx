import { DollarSign, TrendingDown, TrendingUp, Filter, X } from "lucide-react";
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

export interface Transaction {
  id: string;
  type: "credit" | "debit";
  amount: number;
  description: string;
  timestamp: string;
  balance_after: number;
  model?: string; // Optional model name for debit transactions
  user_id?: string; // Optional user ID for user-specific transactions
}

export interface TransactionHistoryProps {
  transactions: Transaction[];
  isLoading?: boolean;
  showCard?: boolean;
}

export function TransactionHistory({
  transactions,
  isLoading = false,
  showCard = true,
}: TransactionHistoryProps) {
  // Filter states
  const [selectedModel, setSelectedModel] = useState<string>("all");
  const [transactionType, setTransactionType] = useState<string>("all");
  const [dateRange, setDateRange] = useState<{ from: Date; to: Date } | undefined>();

  // Extract unique models from transactions
  const availableModels = useMemo(() => {
    const models = new Set<string>();
    transactions.forEach((t) => {
      if (t.model) {
        models.add(t.model);
      }
    });
    return Array.from(models).sort();
  }, [transactions]);

  // Apply filters
  const filteredTransactions = useMemo(() => {
    let filtered = [...transactions];

    // Filter by transaction type
    if (transactionType !== "all") {
      filtered = filtered.filter((t) => t.type === transactionType);
    }

    // Filter by model
    if (selectedModel !== "all") {
      filtered = filtered.filter((t) => t.model === selectedModel);
    }

    // Filter by date range
    if (dateRange?.from && dateRange?.to) {
      filtered = filtered.filter((t) => {
        const transactionDate = new Date(t.timestamp);
        return transactionDate >= dateRange.from && transactionDate <= dateRange.to;
      });
    }

    return filtered;
  }, [transactions, selectedModel, transactionType, dateRange]);

  const hasActiveFilters =
    selectedModel !== "all" ||
    transactionType !== "all" ||
    dateRange !== undefined;

  const clearFilters = () => {
    setSelectedModel("all");
    setTransactionType("all");
    setDateRange(undefined);
  };

  // Reset model filter when switching to credit transactions (since they don't have models)
  const handleTransactionTypeChange = (value: string) => {
    setTransactionType(value);
    if (value === "credit") {
      setSelectedModel("all");
    }
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

  const formatCredits = (amount: number) => {
    return new Intl.NumberFormat("en-US").format(amount);
  };

  const content = (
    <>
      <div className="flex items-center justify-between mb-6">
        <div className="flex items-center gap-2">
          <DollarSign className="w-5 h-5 text-doubleword-neutral-600" />
          <h2 className="text-xl font-semibold text-doubleword-neutral-900">
            Transaction History
          </h2>
        </div>
      </div>

      {/* Filters */}
      <div className="mb-6 space-y-4">
        <div className="flex items-center gap-2">
          <Filter className="w-4 h-4 text-doubleword-neutral-600" />
          <h3 className="text-sm font-medium text-doubleword-neutral-700">
            Filters
          </h3>
          {hasActiveFilters && (
            <Button
              variant="ghost"
              size="sm"
              onClick={clearFilters}
              className="h-7 px-2 text-xs"
            >
              <X className="w-3 h-3 mr-1" />
              Clear filters
            </Button>
          )}
        </div>

        <div className="flex flex-wrap gap-4">
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
                <SelectItem value="credit">Credits only</SelectItem>
                <SelectItem value="debit">Debits only</SelectItem>
              </SelectContent>
            </Select>
          </div>

          {/* Model Filter */}
          <div className="flex items-center gap-2">
            <label className={`text-sm whitespace-nowrap ${
              transactionType === "credit"
                ? "text-doubleword-neutral-400"
                : "text-doubleword-neutral-600"
            }`}>
              Model:
            </label>
            <Select
              value={selectedModel}
              onValueChange={setSelectedModel}
              disabled={transactionType === "credit"}
            >
              <SelectTrigger className="w-[200px]">
                <SelectValue placeholder="All models" />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="all">All models</SelectItem>
                {availableModels.map((model) => (
                  <SelectItem key={model} value={model}>
                    {model}
                  </SelectItem>
                ))}
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
        {filteredTransactions.map((transaction) => (
          <div
            key={transaction.id}
            className="flex items-center justify-between p-4 border border-doubleword-neutral-200 rounded-lg hover:bg-gray-50 transition-colors"
          >
            <div className="flex items-center gap-4 flex-1">
              <div
                className={`p-2 rounded-full ${
                  transaction.type === "credit"
                    ? "bg-green-100"
                    : "bg-red-100"
                }`}
              >
                {transaction.type === "credit" ? (
                  <TrendingUp className="w-5 h-5 text-green-600" />
                ) : (
                  <TrendingDown className="w-5 h-5 text-red-600" />
                )}
              </div>
              <div className="flex-1">
                <p className="font-medium text-doubleword-neutral-900">
                  {transaction.description}
                </p>
                <p className="text-sm text-doubleword-neutral-600">
                  {formatDate(transaction.timestamp)}
                </p>
              </div>
            </div>
            <div className="text-right">
              <p
                className={`font-semibold ${
                  transaction.type === "credit"
                    ? "text-green-600"
                    : "text-red-600"
                }`}
              >
                {transaction.type === "credit" ? "+" : "-"}
                {formatCredits(transaction.amount)}
              </p>
              <p className="text-sm text-doubleword-neutral-600">
                Balance: {formatCredits(transaction.balance_after)}
              </p>
            </div>
          </div>
        ))}
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