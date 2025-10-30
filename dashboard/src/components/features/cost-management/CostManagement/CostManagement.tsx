import { DollarSign, Plus, TrendingDown, TrendingUp, Filter, X } from "lucide-react";
import { Card } from "../../../ui/card";
import { Button } from "../../../ui/button";
import { useState, useMemo } from "react";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../../ui/select";
import { DateTimeRangeSelector } from "../../../ui/date-time-range-selector";
import { useSettings } from "../../../../contexts";
import {
  useCreditBalance,
  useTransactions,
  useAddCredits,
} from "../../../../api/control-layer/hooks";
import { toast } from "sonner";

export interface Transaction {
  id: string;
  type: "credit" | "debit";
  amount: number;
  description: string;
  timestamp: string;
  balance_after: number;
  model?: string; // Optional model name for debit transactions
}

// Dummy data for transactions
const generateDummyTransactions = (): Transaction[] => {
  return [
    {
      id: "1",
      type: "credit",
      amount: 10000,
      description: "Initial credit purchase",
      timestamp: new Date(Date.now() - 30 * 24 * 60 * 60 * 1000).toISOString(),
      balance_after: 10000,
    },
    {
      id: "2",
      type: "debit",
      amount: 450,
      description: "Model execution: gpt-4-turbo (Chat completion)",
      timestamp: new Date(Date.now() - 28 * 24 * 60 * 60 * 1000).toISOString(),
      balance_after: 9550,
      model: "gpt-4-turbo",
    },
    {
      id: "3",
      type: "debit",
      amount: 125,
      description: "Model execution: claude-3-sonnet (Chat completion)",
      timestamp: new Date(Date.now() - 25 * 24 * 60 * 60 * 1000).toISOString(),
      balance_after: 9425,
      model: "claude-3-sonnet",
    },
    {
      id: "4",
      type: "credit",
      amount: 5000,
      description: "Credit purchase - Top up",
      timestamp: new Date(Date.now() - 20 * 24 * 60 * 60 * 1000).toISOString(),
      balance_after: 14425,
    },
    {
      id: "5",
      type: "debit",
      amount: 230,
      description: "Model execution: gpt-4o-mini (Embedding)",
      timestamp: new Date(Date.now() - 18 * 24 * 60 * 60 * 1000).toISOString(),
      balance_after: 14195,
      model: "gpt-4o-mini",
    },
    {
      id: "6",
      type: "debit",
      amount: 680,
      description: "Model execution: gpt-4-turbo (Chat completion)",
      timestamp: new Date(Date.now() - 15 * 24 * 60 * 60 * 1000).toISOString(),
      balance_after: 13515,
      model: "gpt-4-turbo",
    },
    {
      id: "7",
      type: "debit",
      amount: 95,
      description: "Model execution: text-embedding-ada-002 (Embedding)",
      timestamp: new Date(Date.now() - 12 * 24 * 60 * 60 * 1000).toISOString(),
      balance_after: 13420,
      model: "text-embedding-ada-002",
    },
    {
      id: "8",
      type: "debit",
      amount: 320,
      description: "Model execution: claude-3-opus (Chat completion)",
      timestamp: new Date(Date.now() - 10 * 24 * 60 * 60 * 1000).toISOString(),
      balance_after: 13100,
      model: "claude-3-opus",
    },
    {
      id: "9",
      type: "debit",
      amount: 180,
      description: "Model execution: gpt-4o-mini (Chat completion)",
      timestamp: new Date(Date.now() - 8 * 24 * 60 * 60 * 1000).toISOString(),
      balance_after: 12920,
      model: "gpt-4o-mini",
    },
    {
      id: "10",
      type: "debit",
      amount: 540,
      description: "Model execution: gpt-4-turbo (Chat completion)",
      timestamp: new Date(Date.now() - 7 * 24 * 60 * 60 * 1000).toISOString(),
      balance_after: 12380,
      model: "gpt-4-turbo",
    },
    {
      id: "11",
      type: "credit",
      amount: 3000,
      description: "Credit purchase - Top up",
      timestamp: new Date(Date.now() - 5 * 24 * 60 * 60 * 1000).toISOString(),
      balance_after: 15380,
    },
    {
      id: "12",
      type: "debit",
      amount: 210,
      description: "Model execution: claude-3-sonnet (Chat completion)",
      timestamp: new Date(Date.now() - 4 * 24 * 60 * 60 * 1000).toISOString(),
      balance_after: 15170,
      model: "claude-3-sonnet",
    },
    {
      id: "13",
      type: "debit",
      amount: 145,
      description: "Model execution: text-embedding-ada-002 (Embedding)",
      timestamp: new Date(Date.now() - 3 * 24 * 60 * 60 * 1000).toISOString(),
      balance_after: 15025,
      model: "text-embedding-ada-002",
    },
    {
      id: "14",
      type: "debit",
      amount: 290,
      description: "Model execution: gpt-4o-mini (Chat completion)",
      timestamp: new Date(Date.now() - 2 * 24 * 60 * 60 * 1000).toISOString(),
      balance_after: 14735,
      model: "gpt-4o-mini",
    },
    {
      id: "15",
      type: "debit",
      amount: 425,
      description: "Model execution: claude-3-opus (Chat completion)",
      timestamp: new Date(Date.now() - 1 * 24 * 60 * 60 * 1000).toISOString(),
      balance_after: 14310,
      model: "claude-3-opus",
    },
    {
      id: "16",
      type: "debit",
      amount: 110,
      description: "Model execution: gpt-4o-mini (Embedding)",
      timestamp: new Date(Date.now() - 18 * 60 * 60 * 1000).toISOString(),
      balance_after: 14200,
      model: "gpt-4o-mini",
    },
    {
      id: "17",
      type: "debit",
      amount: 520,
      description: "Model execution: gpt-4-turbo (Chat completion)",
      timestamp: new Date(Date.now() - 12 * 60 * 60 * 1000).toISOString(),
      balance_after: 13680,
      model: "gpt-4-turbo",
    },
    {
      id: "18",
      type: "debit",
      amount: 280,
      description: "Model execution: claude-3-sonnet (Chat completion)",
      timestamp: new Date(Date.now() - 6 * 60 * 60 * 1000).toISOString(),
      balance_after: 13400,
      model: "claude-3-sonnet",
    },
  ];
};

export function CostManagement() {
  const { isFeatureEnabled } = useSettings();
  const isDemoMode = isFeatureEnabled("demo");

  // Demo mode state
  const [demoTransactions, setDemoTransactions] = useState<Transaction[]>(
    generateDummyTransactions().reverse()
  );

  // Filter states
  const [selectedModel, setSelectedModel] = useState<string>("all");
  const [transactionType, setTransactionType] = useState<string>("all");
  const [dateRange, setDateRange] = useState<{ from: Date; to: Date } | undefined>();

  // API mode hooks (only fetch when not in demo mode)
  const { data: balanceData, isLoading: isLoadingBalance } = useCreditBalance();
  const {
    data: transactionsData,
    isLoading: isLoadingTransactions,
    refetch: refetchTransactions,
  } = useTransactions();
  const addCreditsMutation = useAddCredits();

  // Determine which data source to use
  const transactions = isDemoMode
    ? demoTransactions
    : transactionsData?.transactions || [];
  const currentBalance = isDemoMode
    ? demoTransactions[0]?.balance_after || 0
    : balanceData?.balance || 0;
  const isLoading = !isDemoMode && (isLoadingBalance || isLoadingTransactions);

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
      };
      setDemoTransactions([newTransaction, ...demoTransactions]);
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
        <Card className="p-6">
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
        </Card>
        </>
        )}
      </div>
    </div>
  );
}
