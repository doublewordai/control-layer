import {
  TrendingDown,
  TrendingUp,
  X,
  Plus,
  DollarSign,
  ChevronLeft,
  ChevronRight,
  ChevronDown,
  Search,
  RefreshCw,
} from "lucide-react";
import { Card } from "../../../ui/card.tsx";
import { Button } from "@/components";
import { useState, useMemo } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../../ui/select.tsx";
import { DateTimeRangeSelector } from "../../../ui/date-time-range-selector.tsx";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "../../../ui/table.tsx";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import type { Transaction } from "@/api/control-layer";
import { useUserBalance, useTransactions, useUser } from "@/api/control-layer";
import { useSettings } from "@/contexts";
import { formatDollars } from "@/utils/money.ts";
import { useDebounce } from "@/hooks/useDebounce";

export type AddFundsConfig =
  | { type: "direct"; onAddFunds: () => void }
  | { type: "redirect"; onAddFunds: () => void }
  | { type: "split"; onPrimaryAction: () => void; onDirectAction: () => void }
  | undefined;

export interface TransactionHistoryProps {
  userId: string;
  showCard?: boolean;
  addFundsConfig?: AddFundsConfig;
  filterUserId?: string;
}

export function TransactionHistory({
  userId,
  showCard = true,
  addFundsConfig,
  filterUserId,
}: TransactionHistoryProps) {
  const { isFeatureEnabled } = useSettings();
  const isDemoMode = isFeatureEnabled("demo");
  const navigate = useNavigate();
  const [searchParams, setSearchParams] = useSearchParams();

  // Fetch user info for display
  const { data: displayUser } = useUser(filterUserId || userId);

  // Fetch balance
  const { refetch: refetchBalance } = useUserBalance(userId);

  // Read pagination state from URL params
  const currentPage = parseInt(searchParams.get("txPage") || "1", 10);
  const pageSize = parseInt(searchParams.get("txPageSize") || "10", 10);

  // Read filter state from URL params
  const transactionType = searchParams.get("txType") || "all";

  // Local state for filters that don't need URL persistence
  const [dateRange, setDateRange] = useState<
    { from: Date; to: Date } | undefined
  >();
  const [searchTerm, setSearchTerm] = useState<string>("");
  const debouncedSearch = useDebounce(searchTerm, 300);

  // Calculate skip for server-side pagination
  const skip = (currentPage - 1) * pageSize;

  // Helper to map UI transaction type to API transaction_types
  const getTransactionTypesParam = (type: string): string | undefined => {
    if (type === "credit") return "admin_grant,purchase";
    if (type === "debit") return "admin_removal,usage";
    return undefined; // "all" means no filter
  };

  // Always pass userId to filter transactions by the specific user
  // Backend enforces permissions: non-admins can only see their own transactions
  // Pass date range for server-side filtering
  const {
    data: transactionsResponse,
    isLoading: isLoadingTransactions,
    refetch: refetchTransactions,
  } = useTransactions({
    userId,
    group_batches: true,
    limit: pageSize,
    skip,
    // Server-side filters
    search: debouncedSearch.trim() || undefined,
    transaction_types: getTransactionTypesParam(transactionType),
    timestamp_after: dateRange?.from?.toISOString(),
    timestamp_before: dateRange?.to?.toISOString(),
  });

  // Get transactions and metadata from response
  const transactions = useMemo<Transaction[]>(() => {
    return transactionsResponse?.data || [];
  }, [transactionsResponse]);

  const totalCount = transactionsResponse?.total_count ?? 0;
  const pageStartBalance = transactionsResponse?.page_start_balance ?? 0;

  // Compute balance_after for each transaction
  // Transactions are in desc order (most recent first), so we iterate and "undo" each transaction
  // to compute what the balance was after that transaction
  const balanceByTransactionId = useMemo(() => {
    const balanceMap = new Map<string, number>();
    let runningBalance = Number(pageStartBalance);

    for (const tx of transactions) {
      // The balance after this transaction is the current running balance
      balanceMap.set(tx.id, runningBalance);

      // "Undo" this transaction to get the balance before it
      // Note: amount may come as string from API for precision, so coerce to number
      const amount = Number(tx.amount);
      const isCredit =
        tx.transaction_type === "admin_grant" ||
        tx.transaction_type === "purchase";
      if (isCredit) {
        // Was a credit, so before this tx the balance was lower
        runningBalance -= amount;
      } else {
        // Was a debit, so before this tx the balance was higher
        runningBalance += amount;
      }
    }

    return balanceMap;
  }, [transactions, pageStartBalance]);

  const isLoading = !isDemoMode && isLoadingTransactions;

  // Helper functions
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

  // Filtering is now done server-side, so we use transactions directly
  // The filterUserId is applied by passing userId to the API
  const filteredTransactions = transactions;

  const totalPages = Math.ceil(totalCount / pageSize);

  const hasActiveFilters =
    transactionType !== "all" || dateRange !== undefined || searchTerm !== "";

  // URL param update helpers
  const updateUrlParams = (updates: Record<string, string | null>) => {
    setSearchParams((prev) => {
      const next = new URLSearchParams(prev);
      for (const [key, value] of Object.entries(updates)) {
        if (value === null) {
          next.delete(key);
        } else {
          next.set(key, value);
        }
      }
      return next;
    });
  };

  const handlePageChange = (newPage: number) => {
    updateUrlParams({ txPage: newPage.toString() });
  };

  const handlePageSizeChange = (newSize: string) => {
    updateUrlParams({ txPage: "1", txPageSize: newSize });
  };

  const clearFilters = () => {
    setDateRange(undefined);
    setSearchTerm("");
    updateUrlParams({ txType: null, txPage: "1" });
  };

  const handleTransactionTypeChange = (value: string) => {
    updateUrlParams({
      txType: value === "all" ? null : value,
      txPage: "1",
    });
  };

  // Reset to page 1 when date range changes
  const handleDateRangeChange = (
    range: { from: Date; to: Date } | undefined,
  ) => {
    setDateRange(range);
    handlePageChange(1);
  };

  // Reset to page 1 when search term changes
  const handleSearchChange = (value: string) => {
    setSearchTerm(value);
    handlePageChange(1);
  };

  // Refresh handler
  const [isRefreshing, setIsRefreshing] = useState(false);
  const handleRefresh = async () => {
    setIsRefreshing(true);
    try {
      await Promise.all([refetchBalance(), refetchTransactions()]);
    } finally {
      setIsRefreshing(false);
    }
  };

  const content = (
    <>
      {/* Header with Title and Balance */}
      <div className="flex items-center justify-between mb-6">
        <div>
          <div className="flex items-center gap-3">
            <h2 className="text-2xl font-semibold text-doubleword-neutral-900">
              Transaction History
            </h2>
            <Button
              variant="ghost"
              size="sm"
              onClick={handleRefresh}
              disabled={isRefreshing || isLoading}
              className="h-8 w-8 p-0"
              title="Refresh balance and transactions"
            >
              <RefreshCw
                className={`h-4 w-4 ${isRefreshing ? "animate-spin" : ""}`}
              />
            </Button>
          </div>
          {displayUser && (
            <p
              className={`text-sm mt-1 ${filterUserId && filterUserId !== userId ? "text-red-600 font-semibold" : "text-gray-600"}`}
            >
              Showing transactions for user{" "}
              <span className="font-medium">{displayUser.email}</span>
            </p>
          )}
        </div>
        <div className="flex items-center gap-3">
          {addFundsConfig && (
            <>
              {(addFundsConfig.type === "direct" ||
                addFundsConfig.type === "redirect") && (
                <Button
                  variant="outline"
                  size="sm"
                  onClick={addFundsConfig.onAddFunds}
                >
                  <Plus className="w-4 h-4 mr-2" />
                  Add to Credit Balance
                </Button>
              )}
              {addFundsConfig.type === "split" && (
                <div className="flex">
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={addFundsConfig.onPrimaryAction}
                    className="rounded-r-none"
                  >
                    <Plus className="w-4 h-4 mr-2" />
                    Add to Credit Balance
                  </Button>
                  <DropdownMenu>
                    <DropdownMenuTrigger asChild>
                      <Button
                        variant="outline"
                        size="sm"
                        className="rounded-l-none border-l-0 px-2"
                      >
                        <ChevronDown className="w-4 h-4" />
                      </Button>
                    </DropdownMenuTrigger>
                    <DropdownMenuContent align="end">
                      <DropdownMenuItem onClick={addFundsConfig.onDirectAction}>
                        Grant as admin
                      </DropdownMenuItem>
                    </DropdownMenuContent>
                  </DropdownMenu>
                </div>
              )}
            </>
          )}
        </div>
      </div>

      {/* Transaction History Card */}
      <Card className="p-4">
        {/* Search and Filters */}
        <div className="space-y-2">
          <div className="flex flex-wrap items-center gap-2">
            {/* Search Bar */}
            <div className="relative flex-1 min-w-[250px]">
              <Search className="absolute left-3 top-1/2 -translate-y-1/2 w-4 h-4 text-doubleword-neutral-400" />
              <input
                type="text"
                placeholder="Search transactions..."
                value={searchTerm}
                onChange={(e) => handleSearchChange(e.target.value)}
                className="w-full pl-10 pr-4 py-2 border border-doubleword-neutral-300 rounded-md focus:outline-none focus:ring-2 focus:ring-blue-500 focus:border-transparent"
              />
            </div>

            {/* Transaction Type Filter */}
            <Select
              value={transactionType}
              onValueChange={handleTransactionTypeChange}
            >
              <SelectTrigger className="w-[150px]">
                <SelectValue placeholder="All types" />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="all">All types</SelectItem>
                <SelectItem value="credit">Deposits only</SelectItem>
                <SelectItem value="debit">Withdrawals only</SelectItem>
              </SelectContent>
            </Select>

            {/* Date Range Filter */}
            <DateTimeRangeSelector
              value={dateRange}
              onChange={handleDateRangeChange}
            />

            {/* Page Size Selector */}
            <Select
              value={pageSize.toString()}
              onValueChange={handlePageSizeChange}
            >
              <SelectTrigger className="w-[80px]">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="10">10</SelectItem>
                <SelectItem value="20">20</SelectItem>
                <SelectItem value="50">50</SelectItem>
                <SelectItem value="100">100</SelectItem>
              </SelectContent>
            </Select>

            <Button
              variant="ghost"
              size="sm"
              onClick={clearFilters}
              disabled={!hasActiveFilters}
              className="h-9"
            >
              <X className="w-4 h-4 mr-1" />
              Clear
            </Button>
          </div>

          {/* Filter Status */}
          {hasActiveFilters && (
            <div className="text-sm text-doubleword-neutral-600">
              Showing {filteredTransactions.length} filtered transactions (page{" "}
              {currentPage} of {totalPages})
            </div>
          )}
        </div>

        <div className="-mt-2 mb-0">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="w-[50px]"></TableHead>
                <TableHead>Description</TableHead>
                <TableHead>Date</TableHead>
                <TableHead className="text-right">Amount</TableHead>
                <TableHead className="text-right">Balance</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {filteredTransactions.map((transaction) => {
                // Determine if transaction is a credit (adds money) or debit (removes money)
                const isCredit =
                  transaction.transaction_type === "admin_grant" ||
                  transaction.transaction_type === "purchase";

                const handleRowClick = () => {
                  if (transaction.batch_id) {
                    // Preserve current URL params when navigating to batch detail
                    const currentParams = searchParams.toString();
                    const fromUrl = currentParams
                      ? `/cost-management?${currentParams}`
                      : "/cost-management";
                    navigate(
                      `/batches/${transaction.batch_id}?from=${encodeURIComponent(fromUrl)}`,
                    );
                  }
                };

                return (
                  <TableRow
                    key={transaction.id}
                    onClick={handleRowClick}
                    tabIndex={transaction.batch_id ? 0 : -1}
                    onKeyDown={(event) => {
                      if (!transaction.batch_id) return;
                      if (event.key === "Enter" || event.key === " ") {
                        event.preventDefault();
                        handleRowClick();
                      }
                    }}
                    role={transaction.batch_id ? "button" : undefined}
                    aria-label={
                      transaction.batch_id
                        ? `View batch ${transaction.batch_id} details`
                        : undefined
                    }
                    className={
                      transaction.batch_id
                        ? "cursor-pointer hover:bg-doubleword-neutral-50"
                        : ""
                    }
                  >
                    <TableCell>
                      <div
                        className={`p-2 rounded-full ${
                          isCredit ? "bg-green-100" : "bg-red-100"
                        }`}
                      >
                        {isCredit ? (
                          <TrendingUp className="w-4 h-4 text-green-600" />
                        ) : (
                          <TrendingDown className="w-4 h-4 text-red-600" />
                        )}
                      </div>
                    </TableCell>
                    <TableCell>
                      <p className="font-medium text-doubleword-neutral-900">
                        {transaction.description || "No description"}
                      </p>
                    </TableCell>
                    <TableCell>
                      <p className="text-sm text-doubleword-neutral-600">
                        {formatDate(transaction.created_at)}
                      </p>
                    </TableCell>
                    <TableCell className="text-right">
                      <p
                        className={`font-semibold ${
                          isCredit ? "text-green-600" : "text-red-600"
                        }`}
                      >
                        {isCredit ? "+" : "-"}
                        {formatDollars(transaction.amount, 9)}
                      </p>
                    </TableCell>
                    <TableCell className="text-right">
                      <p className="text-sm text-doubleword-neutral-600">
                        {formatDollars(
                          balanceByTransactionId.get(transaction.id) ?? 0,
                        )}
                      </p>
                    </TableCell>
                  </TableRow>
                );
              })}
            </TableBody>
          </Table>
        </div>

        {filteredTransactions.length === 0 && (
          <div className="text-center py-8">
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

        {/* Pagination Controls */}
        {totalCount > 0 && (
          <div className="flex items-center justify-between border-t border-doubleword-neutral-200 pt-2">
            <div className="text-sm text-doubleword-neutral-600">
              Showing {skip + 1} to {Math.min(skip + pageSize, totalCount)} of{" "}
              {totalCount} transactions
            </div>
            <div className="flex items-center gap-2">
              <Button
                variant="outline"
                size="sm"
                onClick={() => handlePageChange(1)}
                disabled={currentPage === 1}
              >
                First
              </Button>
              <Button
                variant="outline"
                size="sm"
                onClick={() => handlePageChange(currentPage - 1)}
                disabled={currentPage === 1}
              >
                <ChevronLeft className="w-4 h-4" />
                Previous
              </Button>
              <div className="text-sm text-doubleword-neutral-600">
                Page {currentPage} of {totalPages}
              </div>
              <Button
                variant="outline"
                size="sm"
                onClick={() => handlePageChange(currentPage + 1)}
                disabled={currentPage >= totalPages}
              >
                Next
                <ChevronRight className="w-4 h-4" />
              </Button>
            </div>
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
