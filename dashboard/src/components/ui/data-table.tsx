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
import { ChevronLeft, ChevronRight, Search } from "lucide-react";

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

interface DataTableProps<TData, TValue> {
  columns: ColumnDef<TData, TValue>[];
  data: TData[];
  searchPlaceholder?: string;
  searchColumn?: string;
  showColumnToggle?: boolean;
  showPagination?: boolean;
  pageSize?: number;
  minRows?: number; // Optional: Minimum number of rows to display (pads with empty rows)
  rowHeight?: string; // Optional: Height for each row (e.g., "65px", "49px")
  onSelectionChange?: (selectedRows: TData[]) => void;
  actionBar?: React.ReactNode;
  headerActions?: React.ReactNode;
  initialColumnVisibility?: VisibilityState;
}

export function DataTable<TData, TValue>({
  columns,
  data,
  searchPlaceholder = "Search...",
  searchColumn,
  showColumnToggle = true,
  showPagination = true,
  pageSize = 10,
  minRows,
  rowHeight = "53px", // Default row height
  onSelectionChange,
  actionBar,
  headerActions,
  initialColumnVisibility = {},
}: DataTableProps<TData, TValue>) {
  const [sorting, setSorting] = React.useState<SortingState>([]);
  const [columnFilters, setColumnFilters] = React.useState<ColumnFiltersState>(
    [],
  );
  const [columnVisibility, setColumnVisibility] =
    React.useState<VisibilityState>(initialColumnVisibility);
  const [rowSelection, setRowSelection] = React.useState({});
  const [globalFilter, setGlobalFilter] = React.useState("");

  const table = useReactTable({
    data,
    columns,
    onSortingChange: setSorting,
    onColumnFiltersChange: setColumnFilters,
    onGlobalFilterChange: setGlobalFilter,
    getCoreRowModel: getCoreRowModel(),
    getPaginationRowModel: getPaginationRowModel(),
    getSortedRowModel: getSortedRowModel(),
    getFilteredRowModel: getFilteredRowModel(),
    onColumnVisibilityChange: setColumnVisibility,
    onRowSelectionChange: setRowSelection,
    state: {
      sorting,
      columnFilters,
      columnVisibility,
      rowSelection,
      globalFilter,
    },
    initialState: {
      pagination: {
        pageSize,
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

  return (
    <div className="space-y-4">
      {actionBar && selectedRows.length > 0 && actionBar}
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-2">
          <div className="relative">
            <Search className="absolute left-2 top-2.5 h-4 w-4 text-muted-foreground" />
            <Input
              placeholder={searchPlaceholder}
              aria-label={searchPlaceholder || "Search table"}
              value={
                searchColumn
                  ? ((table
                      .getColumn(searchColumn)
                      ?.getFilterValue() as string) ?? "")
                  : globalFilter
              }
              onChange={(event) => {
                const value = event.target.value;
                if (searchColumn) {
                  table.getColumn(searchColumn)?.setFilterValue(value);
                } else {
                  setGlobalFilter(value);
                }
              }}
              className="pl-8 w-[300px]"
            />
          </div>
          {selectedRows.length > 0 && (
            <div className="text-sm text-muted-foreground">
              {selectedRows.length} of {table.getFilteredRowModel().rows.length}{" "}
              row(s) selected
            </div>
          )}
        </div>
        <div className="flex items-center gap-2">
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
            {table.getRowModel().rows?.length ? (
              <>
                {table.getRowModel().rows.map((row) => (
                  <TableRow
                    key={row.id}
                    data-state={row.getIsSelected() && "selected"}
                    className="group"
                  >
                    {row.getVisibleCells().map((cell) => (
                      <TableCell
                        key={cell.id}
                        className={
                          cell.column.id === "select"
                            ? "pl-6 w-[50px]"
                            : cell.column.getIndex() === 0
                              ? "pl-6"
                              : cell.column.id === "actions"
                                ? "pr-6"
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
                ))}
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
        <div className="flex items-center justify-between">
          <div className="text-sm text-muted-foreground">
            Showing{" "}
            {table.getState().pagination.pageIndex *
              table.getState().pagination.pageSize +
              1}{" "}
            to{" "}
            {Math.min(
              (table.getState().pagination.pageIndex + 1) *
                table.getState().pagination.pageSize,
              table.getFilteredRowModel().rows.length,
            )}
            of {table.getFilteredRowModel().rows.length} results
          </div>
          <div className="flex items-center space-x-2">
            <Button
              variant="outline"
              size="sm"
              onClick={() => table.previousPage()}
              disabled={!table.getCanPreviousPage()}
            >
              <ChevronLeft className="h-4 w-4" />
              Previous
            </Button>
            <Button
              variant="outline"
              size="sm"
              onClick={() => table.nextPage()}
              disabled={!table.getCanNextPage()}
            >
              Next
              <ChevronRight className="h-4 w-4" />
            </Button>
          </div>
        </div>
      )}
    </div>
  );
}
