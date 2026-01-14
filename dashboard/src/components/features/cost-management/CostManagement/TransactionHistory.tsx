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
import { useState, useMemo, useCallback } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { useServerPagination } from "@/hooks/useServerPagination";
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
  | { type: "admin-only"; onGiftFunds: () => void }
  | { type: "purchase-only"; onPurchaseFunds: () => void }
  | {
      type: "split";
      onPurchaseFunds: () => void;
      onGiftFunds?: () => void;
      onBillingPortal?: () => void;
    }
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

  // Server-side pagination
  const pagination = useServerPagination({ paramPrefix: "tx" });

  // Read filter state from URL params
  const transactionType = searchParams.get("txType") || "all";

  // Local state for filters that don't need URL persistence
  const [dateRange, setDateRange] = useState<
    { from: Date; to: Date } | undefined
  >();
  const [searchTerm, setSearchTerm] = useState<string>("");
  const debouncedSearch = useDebounce(searchTerm, 300);

  // Fetch balance and transactions
  const { refetch: refetchBalance } = useUserBalance(userId);

  // Helper to map UI transaction type to API transaction_types
  const getTransactionTypesParam = (type: string): string | undefined => {
    if (type === "credit") return "admin_grant,purchase";
    if (type === "debit") return "admin_removal,usage";
    return undefined; // "all" means no filter
  };

  // Always pass userId to filter transactions by the specific user
  // Backend enforces permissions: non-admins can only see their own transactions
  // Pass all filters for server-side filtering and pagination
  const {
    data: transactionsResponse,
    isLoading: isLoadingTransactions,
    refetch: refetchTransactions,
  } = useTransactions({
    userId,
    group_batches: true,
    ...pagination.queryParams,
    search: debouncedSearch.trim() || undefined,
    transaction_types: getTransactionTypesParam(transactionType),
    start_date: dateRange?.from?.toISOString(),
    end_date: dateRange?.to?.toISOString(),
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

  // Format transaction description with category prefix for usage transactions
  // Format: "Category: model (tokens)" or "Batch (SLA): X requests" for batches
  const formatDescription = (tx: Transaction): string => {
    const baseDescription = tx.description || "No description";

    // Only format usage transactions
    if (tx.transaction_type !== "usage") {
      return baseDescription;
    }

    // For batches, show "Batch (Source - SLA): X requests" format
    const isBatch =
      tx.request_origin === "fusillade" ||
      tx.request_origin === "frontend" ||
      tx.batch_id;
    if (isBatch) {
      const requestCount = tx.batch_request_count || 0;
      const requestsText = requestCount > 0 ? `: ${requestCount} requests` : "";

      // Determine source label: "Frontend" for frontend origin, "API" for everything else
      const source = tx.request_origin === "frontend" ? "Frontend" : "API";

      // Format SLA - should always be present, but handle edge cases
      let slaText = "";
      if (tx.batch_sla === "24h") slaText = " - 24hr";
      else if (tx.batch_sla === "1h") slaText = " - 1hr";
      else if (tx.batch_sla) slaText = ` - ${tx.batch_sla}`;

      return `Batch (${source}${slaText})${requestsText}`;
    }

    // For non-batch usage, extract model and tokens from description
    // Expected format: "API usage: ModelName (X input + Y output tokens)"
    const match = baseDescription.match(/^API usage:\s*(.+)$/i);
    const details = match ? match[1] : baseDescription;

    // Determine category prefix based on request_origin
    const category =
      tx.request_origin === "frontend" ? "Playground" : "Realtime API";

    return `${category}: ${details}`;
  };

  // Calculate total pages from server response
  const totalPages = Math.ceil(totalCount / pagination.pageSize);

  const hasActiveFilters =
    transactionType !== "all" || dateRange !== undefined || searchTerm !== "";

  // URL param update helper - updates params and resets to page 1
  const updateUrlParamsAndResetPage = useCallback(
    (updates: Record<string, string | null>) => {
      setSearchParams((prev) => {
        const next = new URLSearchParams(prev);
        for (const [key, value] of Object.entries(updates)) {
          if (value === null) {
            next.delete(key);
          } else {
            next.set(key, value);
          }
        }
        // Reset to page 1 when filters change
        next.set("txPage", "1");
        return next;
      });
    },
    [setSearchParams],
  );

  const clearFilters = useCallback(() => {
    setDateRange(undefined);
    setSearchTerm("");
    setSearchParams((prev) => {
      const next = new URLSearchParams(prev);
      next.delete("txType");
      next.set("txPage", "1");
      return next;
    });
  }, [setSearchParams]);

  const handleTransactionTypeChange = useCallback(
    (value: string) => {
      updateUrlParamsAndResetPage({ txType: value === "all" ? null : value });
    },
    [updateUrlParamsAndResetPage],
  );

  // Reset to page 1 when date range changes
  const handleDateRangeChange = useCallback(
    (range: { from: Date; to: Date } | undefined) => {
      setDateRange(range);
      pagination.handleReset();
    },
    [pagination],
  );

  // Reset to page 1 when search term changes
  const handleSearchChange = useCallback(
    (value: string) => {
      setSearchTerm(value);
      pagination.handleReset();
    },
    [pagination],
  );

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
              {addFundsConfig.type === "admin-only" && (
                <Button
                  variant="outline"
                  size="sm"
                  onClick={addFundsConfig.onGiftFunds}
                >
                  <Plus className="w-4 h-4 mr-2" />
                  Gift Funds
                </Button>
              )}
              {addFundsConfig.type === "purchase-only" && (
                <Button
                  variant="outline"
                  size="sm"
                  onClick={addFundsConfig.onPurchaseFunds}
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
                    onClick={addFundsConfig.onPurchaseFunds}
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
                      {addFundsConfig.onGiftFunds && (
                        <DropdownMenuItem onClick={addFundsConfig.onGiftFunds}>
                          Gift Funds
                        </DropdownMenuItem>
                      )}
                      {addFundsConfig.onBillingPortal && (
                        <DropdownMenuItem
                          onClick={addFundsConfig.onBillingPortal}
                        >
                          Billing Portal
                        </DropdownMenuItem>
                      )}
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
              value={pagination.pageSize.toString()}
              onValueChange={(value) =>
                pagination.handlePageSizeChange(parseInt(value, 10))
              }
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
              Showing {totalCount} filtered transaction
              {totalCount !== 1 ? "s" : ""} (page {pagination.page} of{" "}
              {totalPages || 1})
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
              {transactions.map((transaction) => {
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
                        {formatDescription(transaction)}
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

        {transactions.length === 0 && (
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
              Showing {pagination.queryParams.skip + 1} to{" "}
              {Math.min(
                pagination.queryParams.skip + pagination.pageSize,
                totalCount,
              )}{" "}
              of {totalCount} transactions
            </div>
            <div className="flex items-center gap-2">
              <Button
                variant="outline"
                size="sm"
                onClick={() => pagination.handlePageChange(1)}
                disabled={pagination.page === 1}
              >
                First
              </Button>
              <Button
                variant="outline"
                size="sm"
                onClick={() => pagination.handlePageChange(pagination.page - 1)}
                disabled={pagination.page === 1}
              >
                <ChevronLeft className="w-4 h-4" />
                Previous
              </Button>
              <div className="text-sm text-doubleword-neutral-600">
                Page {pagination.page} of {totalPages}
              </div>
              <Button
                variant="outline"
                size="sm"
                onClick={() => pagination.handlePageChange(pagination.page + 1)}
                disabled={pagination.page >= totalPages}
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
