"use client";

import * as React from "react";
import {
  type ColumnDef,
  type ColumnFiltersState,
  type SortingState,
  type VisibilityState,
  flexRender,
  getCoreRowModel,
  getFilteredRowModel,
  getPaginationRowModel,
  getSortedRowModel,
  useReactTable,
} from "@tanstack/react-table";
import { Search } from "lucide-react";

import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "./table";
import { Button } from "./button";
import { Input } from "./input";
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuTrigger,
} from "./dropdown-menu";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "./select";
import { TablePagination } from "./table-pagination";
import { CursorPagination } from "./cursor-pagination";

/**
 * Server-side pagination configuration for offset-based pagination
 */
interface ServerPagination {
  page: number;
  pageSize: number;
  totalItems?: number;
  onPageChange: (page: number) => void;
  onPageSizeChange?: (pageSize: number) => void;
}

/**
 * Server-side pagination configuration for cursor-based pagination
 */
interface ServerCursorPagination {
  page: number;
  pageSize: number;
  onNextPage: (lastItemId: string) => void;
  onPrevPage: () => void;
  onFirstPage?: () => void;
  onPageSizeChange?: (pageSize: number) => void;
  hasNextPage: boolean;
  hasPrevPage: boolean;
}

interface DataTableProps<TData, TValue> {
  columns: ColumnDef<TData, TValue>[];
  data: TData[];
  searchPlaceholder?: string;
  searchColumn?: string;
  showColumnToggle?: boolean;
  pageSize?: number;
  totalItems?: number;
  minRows?: number; // Optional: Minimum number of rows to display (pads with empty rows)
  rowHeight?: string; // Optional: Height for each row (e.g., "65px", "49px")
  onSelectionChange?: (selectedRows: TData[]) => void;
  actionBar?: React.ReactNode;
  headerActions?: React.ReactNode;
  initialColumnVisibility?: VisibilityState;
  onRowClick?: (row: TData) => void;

  // NEW: Server-side pagination support
  /**
   * Pagination mode
   * - 'client': All data loaded at once, pagination handled in-memory (default)
   * - 'server': Server-side pagination using skip/limit
   * - 'server-cursor': Server-side pagination using after(cursor)/limit
   */
  paginationMode?: "client" | "server" | "server-cursor";

  /**
   * Server pagination configuration (required when paginationMode is 'server-offset' or 'server-cursor')
   */
  serverPagination?: ServerPagination | ServerCursorPagination;

  /**
   * External search control (for server-side search)
   * When provided, search input becomes a controlled component
   */
  externalSearch?: {
    value: string;
    onChange: (value: string) => void;
  };

  /**
   * Loading state (shows skeleton rows)
   */
  isLoading?: boolean;

  /**
   * Show page size selector
   */
  showPageSizeSelector?: boolean;

  /**
   * Page size options for the selector
   * @default [10, 25, 50, 100]
   */
  pageSizeOptions?: number[];
}

export function DataTable<TData, TValue>({
  columns,
  data,
  searchPlaceholder = "Search...",
  searchColumn,
  showColumnToggle = true,
  pageSize = 10,
  totalItems,
  minRows,
  rowHeight = "53px", // Default row height
  onSelectionChange,
  actionBar,
  headerActions,
  initialColumnVisibility = {},
  onRowClick,
  paginationMode = "client",
  serverPagination,
  externalSearch,
  isLoading = false,
  showPageSizeSelector = false,
  pageSizeOptions = [10, 25, 50, 100],
}: DataTableProps<TData, TValue>) {
  const [sorting, setSorting] = React.useState<SortingState>([]);
  const [columnFilters, setColumnFilters] = React.useState<ColumnFiltersState>(
    [],
  );
  const [columnVisibility, setColumnVisibility] =
    React.useState<VisibilityState>(initialColumnVisibility);
  const [rowSelection, setRowSelection] = React.useState({});
  const [globalFilter, setGlobalFilter] = React.useState("");

  // Determine if we're in server-side mode
  const isServerMode = paginationMode !== "client";

  // Use server pagination page size if available, otherwise use prop
  const currentPageSize = serverPagination?.pageSize ?? pageSize;

  const table = useReactTable({
    data,
    columns,
    onSortingChange: setSorting,
    onColumnFiltersChange: setColumnFilters,
    onGlobalFilterChange: setGlobalFilter,
    getCoreRowModel: getCoreRowModel(),
    getPaginationRowModel: isServerMode ? undefined : getPaginationRowModel(),
    getSortedRowModel: getSortedRowModel(),
    getFilteredRowModel: getFilteredRowModel(),
    onColumnVisibilityChange: setColumnVisibility,
    onRowSelectionChange: setRowSelection,
    manualPagination: isServerMode,
    manualSorting: false,
    manualFiltering: false,
    state: {
      sorting,
      columnFilters,
      columnVisibility,
      rowSelection,
      globalFilter,
    },
    initialState: {
      pagination: {
        pageSize: currentPageSize,
      },
    },
  });

  const selectedRows = table.getFilteredSelectedRowModel().rows;

  // Call onSelectionChange when selection changes
  React.useEffect(() => {
    if (onSelectionChange) {
      const selectedData = selectedRows.map((row) => row.original);
      onSelectionChange(selectedData);
    }
    // Only depend on the actual selection state, not the rows themselves
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [rowSelection, onSelectionChange]);

  // Calculate how many empty rows we need to pad
  const currentPageRows = table.getRowModel().rows;
  const emptyRowsCount = minRows
    ? Math.max(0, minRows - currentPageRows.length)
    : 0;

  // Determine if pagination should be shown
  const showPagination = (() => {
    if (paginationMode === "server") {
      const offsetPagination = serverPagination as ServerPagination | undefined;
      return (
        !!offsetPagination?.totalItems &&
        offsetPagination.totalItems > offsetPagination.pageSize
      );
    }
    if (paginationMode === "server-cursor") {
      const cursorPagination = serverPagination as
        | ServerCursorPagination
        | undefined;
      return !!(cursorPagination?.hasNextPage || cursorPagination?.hasPrevPage);
    }
    // Client mode: show if there are more items than page size
    return data.length > currentPageSize;
  })();

  return (
    <div className="space-y-4">
      {actionBar && selectedRows.length > 0 && actionBar}
      <div className="flex flex-col sm:flex-row items-start sm:items-center justify-between gap-4">
        <div className="flex flex-col sm:flex-row items-start sm:items-center gap-2 w-full sm:w-auto">
          <div className="relative w-full sm:w-auto">
            <Search className="absolute left-2 top-2.5 h-4 w-4 text-muted-foreground" />
            <Input
              placeholder={searchPlaceholder}
              aria-label={searchPlaceholder || "Search table"}
              value={
                externalSearch
                  ? externalSearch.value
                  : searchColumn
                    ? ((table
                        .getColumn(searchColumn)
                        ?.getFilterValue() as string) ?? "")
                    : globalFilter
              }
              onChange={(event) => {
                const value = event.target.value;
                if (externalSearch) {
                  externalSearch.onChange(value);
                } else if (searchColumn) {
                  table.getColumn(searchColumn)?.setFilterValue(value);
                } else {
                  setGlobalFilter(value);
                }
              }}
              className="pl-8 w-full sm:w-[300px]"
              disabled={isLoading}
            />
          </div>
          {selectedRows.length > 0 && (
            <div className="text-sm text-muted-foreground">
              {selectedRows.length} of {table.getFilteredRowModel().rows.length}{" "}
              row(s) selected
            </div>
          )}
        </div>
        <div className="flex items-center gap-2 w-full sm:w-auto justify-end">
          {showPageSizeSelector && serverPagination?.onPageSizeChange && (
            <div className="flex items-center gap-2">
              <span className="text-sm text-muted-foreground">Show</span>
              <Select
                value={String(currentPageSize)}
                onValueChange={(value) =>
                  serverPagination.onPageSizeChange?.(Number(value))
                }
              >
                <SelectTrigger className="w-[70px]">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {pageSizeOptions.map((size) => (
                    <SelectItem key={size} value={String(size)}>
                      {size}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
          )}
          {headerActions}
          {showColumnToggle && (
            <DropdownMenu>
              <DropdownMenuTrigger asChild>
                <Button variant="outline" size="sm">
                  Columns
                </Button>
              </DropdownMenuTrigger>
              <DropdownMenuContent align="end" className="w-[150px]">
                {table
                  .getAllColumns()
                  .filter((column) => column.getCanHide())
                  .map((column) => {
                    return (
                      <DropdownMenuCheckboxItem
                        key={column.id}
                        className="capitalize"
                        checked={column.getIsVisible()}
                        onCheckedChange={(value) =>
                          column.toggleVisibility(!!value)
                        }
                      >
                        {column.id.replace(/_/g, " ")}
                      </DropdownMenuCheckboxItem>
                    );
                  })}
              </DropdownMenuContent>
            </DropdownMenu>
          )}
        </div>
      </div>
      <div className="rounded-md border">
        <Table>
          <TableHeader>
            {table.getHeaderGroups().map((headerGroup) => (
              <TableRow key={headerGroup.id}>
                {headerGroup.headers.map((header) => {
                  return (
                    <TableHead
                      key={header.id}
                      className={
                        header.column.id === "select"
                          ? "pl-6 w-[50px]"
                          : header.index === 0
                            ? "pl-6"
                            : ""
                      }
                    >
                      {header.isPlaceholder
                        ? null
                        : flexRender(
                            header.column.columnDef.header,
                            header.getContext(),
                          )}
                    </TableHead>
                  );
                })}
              </TableRow>
            ))}
          </TableHeader>
          <TableBody>
            {isLoading ? (
              // Show skeleton rows while loading
              Array.from({ length: minRows || currentPageSize }).map(
                (_, index) => (
                  <TableRow
                    key={`skeleton-${index}`}
                    className="hover:bg-transparent"
                    style={{ height: rowHeight }}
                  >
                    {columns.map((_, cellIndex) => (
                      <TableCell
                        key={`skeleton-${index}-${cellIndex}`}
                        className={cellIndex === 0 ? "pl-6" : ""}
                      >
                        <div className="h-4 bg-muted animate-pulse rounded" />
                      </TableCell>
                    ))}
                  </TableRow>
                ),
              )
            ) : table.getRowModel().rows?.length ? (
              <>
                {table.getRowModel().rows.map((row) => {
                  // Check if this is an expansion row
                  const isExpandedRow = (row.original as any)?._isExpandedRow;

                  if (isExpandedRow) {
                    // Render expansion row with colspan
                    const visibleColumns = row.getVisibleCells();
                    const firstCell = visibleColumns[0];

                    return (
                      <TableRow key={row.id} className="hover:bg-transparent">
                        <TableCell
                          colSpan={visibleColumns.length}
                          className="p-0 whitespace-normal"
                        >
                          {flexRender(
                            firstCell.column.columnDef.cell,
                            firstCell.getContext(),
                          )}
                        </TableCell>
                      </TableRow>
                    );
                  }

                  // Regular row
                  return (
                    <TableRow
                      key={row.id}
                      data-state={row.getIsSelected() && "selected"}
                      className={onRowClick ? "group cursor-pointer" : "group"}
                      style={{ height: rowHeight }}
                      onClick={() => onRowClick?.(row.original)}
                    >
                      {row.getVisibleCells().map((cell, index, cells) => (
                        <TableCell
                          key={cell.id}
                          className={
                            cell.column.id === "select"
                              ? "pl-6 w-[50px]"
                              : cell.column.getIndex() === 0
                                ? "pl-6"
                                : index === cells.length - 1
                                  ? "pr-0"
                                  : ""
                          }
                        >
                          {flexRender(
                            cell.column.columnDef.cell,
                            cell.getContext(),
                          )}
                        </TableCell>
                      ))}
                    </TableRow>
                  );
                })}
                {/* Render empty padding rows if minRows is set */}
                {emptyRowsCount > 0 &&
                  Array.from({ length: emptyRowsCount }).map((_, index) => (
                    <TableRow
                      key={`empty-${index}`}
                      className="hover:bg-transparent"
                      style={{ height: rowHeight }}
                    >
                      {columns.map((_, cellIndex) => (
                        <TableCell
                          key={`empty-${index}-${cellIndex}`}
                          className={cellIndex === 0 ? "pl-6" : ""}
                          style={{ height: rowHeight }}
                        >
                          {/* Empty cell */}
                        </TableCell>
                      ))}
                    </TableRow>
                  ))}
              </>
            ) : (
              <TableRow>
                <TableCell
                  colSpan={columns.length}
                  className="h-24 text-center"
                >
                  No results.
                </TableCell>
              </TableRow>
            )}
          </TableBody>
        </Table>
      </div>
      {showPagination && (
        <>
          {paginationMode === "server-cursor" &&
          serverPagination &&
          "hasNextPage" in serverPagination ? (
            <CursorPagination
              currentPage={serverPagination.page}
              itemsPerPage={serverPagination.pageSize}
              onNextPage={() => {
                if (data.length > 0) {
                  const lastItem = data[data.length - 1] as any;
                  const lastItemId =
                    lastItem.id || lastItem._id || lastItem.key;
                  serverPagination.onNextPage(lastItemId);
                }
              }}
              onPrevPage={serverPagination.onPrevPage}
              onFirstPage={serverPagination.onFirstPage}
              hasNextPage={serverPagination.hasNextPage}
              hasPrevPage={serverPagination.hasPrevPage}
              currentPageItemCount={data.length}
              itemName="results"
            />
          ) : paginationMode === "server" &&
            serverPagination &&
            "onPageChange" in serverPagination ? (
            <TablePagination
              currentPage={serverPagination.page}
              itemsPerPage={serverPagination.pageSize}
              onPageChange={serverPagination.onPageChange}
              totalItems={serverPagination.totalItems ?? 0}
              itemName="results"
            />
          ) : (
            <TablePagination
              currentPage={table.getState().pagination.pageIndex + 1}
              itemsPerPage={table.getState().pagination.pageSize}
              onPageChange={(page) => table.setPageIndex(page - 1)}
              totalItems={totalItems ?? data.length}
              itemName="results"
            />
          )}
        </>
      )}
    </div>
  );
}
