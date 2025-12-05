import { useCallback } from "react";
import { useSearchParams } from "react-router-dom";

/**
 * Options for configuring server-side offset pagination
 */
interface UseServerPaginationOptions {
  /**
   * Prefix for URL parameters (e.g., "files", "batches").
   * Useful for pages with multiple paginated tables.
   * Without prefix: ?page=2&pageSize=25
   * With prefix "files": ?filesPage=2&filesPageSize=25
   */
  paramPrefix?: string;

  /**
   * Default number of items per page
   * @default 10
   */
  defaultPageSize?: number;
}

/**
 * Hook for managing server-side offset pagination state via URL parameters.
 *
 * Uses skip/limit strategy where skip = (page - 1) * limit.
 * All pagination state is synchronized with URL search params for shareable URLs.
 *
 * @example
 * ```tsx
 * function MyComponent() {
 *   const pagination = useServerPagination({ defaultPageSize: 25 });
 *
 *   const { data, isLoading } = useQuery({
 *     queryKey: ['items', pagination.queryParams],
 *     queryFn: () => fetchItems(pagination.queryParams),
 *   });
 *
 *   return (
 *     <DataTable
 *       columns={columns}
 *       data={data?.items ?? []}
 *       paginationMode="server-offset"
 *       serverPagination={{
 *         page: pagination.page,
 *         pageSize: pagination.pageSize,
 *         totalItems: data?.total,
 *         onPageChange: pagination.handlePageChange,
 *         onPageSizeChange: pagination.handlePageSizeChange,
 *       }}
 *       isLoading={isLoading}
 *     />
 *   );
 * }
 * ```
 *
 * @example Multi-table usage with prefix
 * ```tsx
 * function MyComponent() {
 *   const filesPagination = useServerPagination({
 *     paramPrefix: 'files',
 *     defaultPageSize: 10
 *   });
 *
 *   const batchesPagination = useServerPagination({
 *     paramPrefix: 'batches',
 *     defaultPageSize: 25
 *   });
 *
 *   // URL will be: ?filesPage=1&filesPageSize=10&batchesPage=2&batchesPageSize=25
 * }
 * ```
 */
export function useServerPagination(options: UseServerPaginationOptions = {}) {
  const { paramPrefix = "", defaultPageSize = 10 } = options;
  const [searchParams, setSearchParams] = useSearchParams();

  // Build parameter names with optional prefix
  const pageParam = paramPrefix ? `${paramPrefix}Page` : "page";
  const pageSizeParam = paramPrefix ? `${paramPrefix}PageSize` : "pageSize";

  // Read current state from URL params
  const page = parseInt(searchParams.get(pageParam) || "1", 10);
  const pageSize = parseInt(
    searchParams.get(pageSizeParam) || String(defaultPageSize),
    10,
  );

  /**
   * Navigate to a specific page number
   */
  const handlePageChange = useCallback(
    (newPage: number) => {
      setSearchParams((prev) => {
        const next = new URLSearchParams(prev);
        next.set(pageParam, String(newPage));
        return next;
      });
    },
    [pageParam, setSearchParams],
  );

  /**
   * Change the number of items per page
   * Resets to page 1 when page size changes
   */
  const handlePageSizeChange = useCallback(
    (newPageSize: number) => {
      setSearchParams((prev) => {
        const next = new URLSearchParams(prev);
        next.set(pageParam, "1"); // Reset to first page
        next.set(pageSizeParam, String(newPageSize));
        return next;
      });
    },
    [pageParam, pageSizeParam, setSearchParams],
  );

  /**
   * Reset pagination to first page
   * Useful when search/filter changes
   */
  const handleReset = useCallback(() => {
    setSearchParams((prev) => {
      const next = new URLSearchParams(prev);
      next.set(pageParam, "1");
      return next;
    });
  }, [pageParam, setSearchParams]);

  /**
   * Clear pagination parameters from URL
   * Useful when closing modals or unmounting components
   */
  const handleClear = useCallback(() => {
    setSearchParams((prev) => {
      const next = new URLSearchParams(prev);
      next.delete(pageParam);
      next.delete(pageSizeParam);
      return next;
    });
  }, [pageParam, pageSizeParam, setSearchParams]);

  // Calculate skip value for API queries
  const skip = (page - 1) * pageSize;

  return {
    // Current state
    page,
    pageSize,

    // Actions
    handlePageChange,
    handlePageSizeChange,
    handleReset,
    handleClear,

    // Query parameters ready for API calls
    queryParams: {
      skip,
      limit: pageSize,
    },
  };
}
