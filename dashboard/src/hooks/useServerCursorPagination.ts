import { useCallback, useRef, useEffect, useState } from "react";
import { useSearchParams } from "react-router-dom";

/**
 * Determines the default page size based on screen height.
 * Returns 20 for tall displays (>= 900px), otherwise 10.
 */
function getDefaultPageSizeForScreen(): number {
  if (typeof window !== "undefined" && window.innerHeight >= 900) {
    return 20;
  }
  return 10;
}

/**
 * Options for configuring server-side cursor pagination
 */
interface UseServerCursorPaginationOptions {
  /**
   * Prefix for URL parameters (e.g., "files", "batches").
   * Useful for pages with multiple paginated tables.
   * Without prefix: ?page=2&pageSize=25&after=abc123
   * With prefix "files": ?filesPage=2&filesPageSize=25&filesAfter=abc123
   */
  paramPrefix?: string;

  /**
   * Default number of items per page
   * @default 10
   */
  defaultPageSize?: number;
}

/**
 * Hook for managing server-side cursor pagination state via URL parameters.
 *
 * Uses cursor-based pagination where each page is identified by a cursor (typically the last item's ID).
 * Maintains cursor history for backward navigation.
 * All pagination state is synchronized with URL search params for shareable URLs.
 *
 * @example
 * ```tsx
 * function MyComponent() {
 *   const pagination = useServerCursorPagination({ defaultPageSize: 25 });
 *
 *   const { data, isLoading } = useQuery({
 *     queryKey: ['items', pagination.queryParams],
 *     queryFn: () => fetchItems(pagination.queryParams),
 *   });
 *
 *   // Fetch N+1 items to determine if there's a next page
 *   const items = data?.slice(0, pagination.pageSize) ?? [];
 *   const hasNextPage = (data?.length ?? 0) > pagination.pageSize;
 *
 *   return (
 *     <DataTable
 *       columns={columns}
 *       data={items}
 *       paginationMode="server-cursor"
 *       serverPagination={{
 *         page: pagination.page,
 *         pageSize: pagination.pageSize,
 *         onNextPage: () => {
 *           const lastItem = items[items.length - 1];
 *           pagination.handleNextPage(lastItem.id);
 *         },
 *         onPrevPage: pagination.handlePrevPage,
 *         onFirstPage: pagination.handleFirstPage,
 *         onPageSizeChange: pagination.handlePageSizeChange,
 *         hasNextPage,
 *         hasPrevPage: pagination.hasPrevPage,
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
 *   const filesPagination = useServerCursorPagination({
 *     paramPrefix: 'files',
 *     defaultPageSize: 10
 *   });
 *
 *   const batchesPagination = useServerCursorPagination({
 *     paramPrefix: 'batches',
 *     defaultPageSize: 25
 *   });
 *
 *   // URL will be: ?filesPage=1&filesPageSize=10&filesAfter=xyz&batchesPage=2&batchesPageSize=25&batchesAfter=abc
 * }
 * ```
 */
export function useServerCursorPagination(
  options: UseServerCursorPaginationOptions = {},
) {
  const { paramPrefix = "", defaultPageSize } = options;
  const [searchParams, setSearchParams] = useSearchParams();

  // Determine page size: use provided default, or detect from screen height
  const [screenDefaultPageSize] = useState(() => getDefaultPageSizeForScreen());
  const effectiveDefaultPageSize = defaultPageSize ?? screenDefaultPageSize;

  // Build parameter names with optional prefix
  const pageParam = paramPrefix ? `${paramPrefix}Page` : "page";
  const pageSizeParam = paramPrefix ? `${paramPrefix}PageSize` : "pageSize";
  const cursorParam = paramPrefix ? `${paramPrefix}After` : "after";

  // Read current state from URL params
  const page = parseInt(searchParams.get(pageParam) || "1", 10);
  const pageSize = parseInt(
    searchParams.get(pageSizeParam) || String(effectiveDefaultPageSize),
    10,
  );
  const cursor = searchParams.get(cursorParam) || undefined;

  // Maintain cursor history for backward navigation
  // cursorHistory[i] = cursor to use when navigating to page i+1
  const cursorHistory = useRef<(string | undefined)[]>([]);

  // Clear cursor history when page size changes
  // We detect this by tracking the previous page size
  const prevPageSize = useRef(pageSize);
  useEffect(() => {
    if (prevPageSize.current !== pageSize) {
      cursorHistory.current = [];
      prevPageSize.current = pageSize;
    }
  }, [pageSize]);

  /**
   * Navigate to the next page using the last item's ID as cursor
   * @param lastItemId - The ID of the last item on the current page
   */
  const handleNextPage = useCallback(
    (lastItemId: string) => {
      // Store current cursor in history before moving forward
      cursorHistory.current[page - 1] = cursor;

      setSearchParams((prev) => {
        const next = new URLSearchParams(prev);
        next.set(pageParam, String(page + 1));
        next.set(cursorParam, lastItemId);
        return next;
      });
    },
    [cursor, page, pageParam, cursorParam, setSearchParams],
  );

  /**
   * Navigate to the previous page using stored cursor from history
   */
  const handlePrevPage = useCallback(() => {
    if (page <= 1) return;

    const previousCursor = cursorHistory.current[page - 2];

    setSearchParams((prev) => {
      const next = new URLSearchParams(prev);
      next.set(pageParam, String(page - 1));

      if (previousCursor) {
        next.set(cursorParam, previousCursor);
      } else {
        next.delete(cursorParam);
      }

      return next;
    });
  }, [page, pageParam, cursorParam, setSearchParams]);

  /**
   * Navigate to the first page, clearing cursor and history
   */
  const handleFirstPage = useCallback(() => {
    cursorHistory.current = [];

    setSearchParams((prev) => {
      const next = new URLSearchParams(prev);
      next.set(pageParam, "1");
      next.delete(cursorParam);
      return next;
    });
  }, [pageParam, cursorParam, setSearchParams]);

  /**
   * Change the number of items per page
   * Resets to page 1 and clears cursor history
   */
  const handlePageSizeChange = useCallback(
    (newPageSize: number) => {
      cursorHistory.current = [];

      setSearchParams((prev) => {
        const next = new URLSearchParams(prev);
        next.set(pageParam, "1");
        next.set(pageSizeParam, String(newPageSize));
        next.delete(cursorParam);
        return next;
      });
    },
    [pageParam, pageSizeParam, cursorParam, setSearchParams],
  );

  // Whether previous page navigation is available
  const hasPrevPage = page > 1;

  return {
    // Current state
    page,
    pageSize,
    cursor,

    // Actions
    handleNextPage,
    handlePrevPage,
    handleFirstPage,
    handlePageSizeChange,

    // Helpers
    hasPrevPage,

    // Query parameters ready for API calls
    // Note: Fetch limit + 1 to determine if there's a next page
    queryParams: {
      limit: pageSize + 1,
      ...(cursor && { after: cursor }),
    },
  };
}
