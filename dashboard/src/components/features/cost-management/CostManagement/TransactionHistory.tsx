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

  // Fetch user info for display
  const { data: displayUser } = useUser(filterUserId || userId);

  // Fetch balance and transactions
  const { refetch: refetchBalance } = useUserBalance(userId);

  // Always pass userId to filter transactions by the specific user
  // Backend enforces permissions: non-admins can only see their own transactions
  const {
    data: transactionsData,
    isLoading: isLoadingTransactions,
    refetch: refetchTransactions,
  } = useTransactions({ userId });

  // Get transactions - use fetched data in both demo and API mode
  // In demo mode, MSW returns data from transactions.json
  const transactions = useMemo<Transaction[]>(() => {
    return transactionsData || [];
  }, [transactionsData]);

  const isLoading = !isDemoMode && isLoadingTransactions;

  // Filter states
  const [transactionType, setTransactionType] = useState<string>("all");
  const [dateRange, setDateRange] = useState<
    { from: Date; to: Date } | undefined
  >();
  const [searchTerm, setSearchTerm] = useState<string>("");

  // Pagination state
  const [currentPage, setCurrentPage] = useState(1);
  const itemsPerPage = 10;

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

  // Apply filters
  const filteredTransactions = useMemo(() => {
    let filtered = [...transactions];

    // Filter by specific user if provided
    if (filterUserId) {
      filtered = filtered.filter((t) => t.user_id === filterUserId);
    }

    // Filter by search term
    if (searchTerm) {
      const lowerSearch = searchTerm.toLowerCase();
      filtered = filtered.filter((t) => {
        const description = (t.description || "").toLowerCase();
        const amount = formatDollars(t.amount).toLowerCase();
        return (
          description.includes(lowerSearch) || amount.includes(lowerSearch)
        );
      });
    }

    // Filter by transaction type
    if (transactionType !== "all") {
      // Map UI filter values to backend transaction types
      if (transactionType === "credit") {
        filtered = filtered.filter(
          (t) =>
            t.transaction_type === "admin_grant" ||
            t.transaction_type === "purchase",
        );
      } else if (transactionType === "debit") {
        filtered = filtered.filter(
          (t) =>
            t.transaction_type === "admin_removal" ||
            t.transaction_type === "usage",
        );
      }
    }

    // Filter by date range
    if (dateRange?.from && dateRange?.to) {
      filtered = filtered.filter((t) => {
        const transactionDate = new Date(t.created_at);
        return (
          transactionDate >= dateRange.from && transactionDate <= dateRange.to
        );
      });
    }

    return filtered;
  }, [transactions, transactionType, dateRange, searchTerm, filterUserId]);

  // Paginate filtered transactions
  const paginatedTransactions = useMemo(() => {
    const startIndex = (currentPage - 1) * itemsPerPage;
    const endIndex = startIndex + itemsPerPage;
    return filteredTransactions.slice(startIndex, endIndex);
  }, [filteredTransactions, currentPage, itemsPerPage]);

  const totalPages = Math.ceil(filteredTransactions.length / itemsPerPage);

  const hasActiveFilters =
    transactionType !== "all" || dateRange !== undefined || searchTerm !== "";

  const clearFilters = () => {
    setTransactionType("all");
    setDateRange(undefined);
    setSearchTerm("");
    setCurrentPage(1); // Reset to first page when clearing filters
  };

  const handleTransactionTypeChange = (value: string) => {
    setTransactionType(value);
    setCurrentPage(1); // Reset to first page when filter changes
  };

  // Reset to page 1 when date range changes
  const handleDateRangeChange = (
    range: { from: Date; to: Date } | undefined,
  ) => {
    setDateRange(range);
    setCurrentPage(1);
  };

  // Reset to page 1 when search term changes
  const handleSearchChange = (value: string) => {
    setSearchTerm(value);
    setCurrentPage(1);
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
              Showing {filteredTransactions.length} of {transactions.length}{" "}
              transactions
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
              {paginatedTransactions.map((transaction) => {
                // Determine if transaction is a credit (adds money) or debit (removes money)
                const isCredit =
                  transaction.transaction_type === "admin_grant" ||
                  transaction.transaction_type === "purchase";

                return (
                  <TableRow key={transaction.id}>
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
                        {formatDollars(transaction.balance_after)}
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
        {filteredTransactions.length > itemsPerPage && (
          <div className="flex items-center justify-between border-t border-doubleword-neutral-200 pt-2">
            <div className="text-sm text-doubleword-neutral-600">
              Showing {(currentPage - 1) * itemsPerPage + 1} to{" "}
              {Math.min(
                currentPage * itemsPerPage,
                filteredTransactions.length,
              )}{" "}
              of {filteredTransactions.length} transactions
            </div>
            <div className="flex items-center gap-2">
              <Button
                variant="outline"
                size="sm"
                onClick={() => setCurrentPage((p) => Math.max(1, p - 1))}
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
                onClick={() =>
                  setCurrentPage((p) => Math.min(totalPages, p + 1))
                }
                disabled={currentPage === totalPages}
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
