"use client";

import { type ColumnDef } from "@tanstack/react-table";
import { ArrowUpDown, Trash2, Key, MoreHorizontal } from "lucide-react";
import { Button } from "../../../ui/button";
import { Checkbox } from "../../../ui/checkbox";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuTrigger,
} from "../../../ui/dropdown-menu";
import type { ApiKey } from "../../../../api/clay/types";

interface ColumnActions {
  onDelete: (apiKey: ApiKey) => void;
}

export const createColumns = (actions: ColumnActions): ColumnDef<ApiKey>[] => [
  {
    id: "select",
    header: ({ table }) => (
      <Checkbox
        checked={
          table.getIsAllPageRowsSelected() ||
          (table.getIsSomePageRowsSelected() && "indeterminate")
        }
        onCheckedChange={(value) => table.toggleAllPageRowsSelected(!!value)}
        aria-label="Select all"
        className="translate-y-[2px]"
      />
    ),
    cell: ({ row }) => (
      <Checkbox
        checked={row.getIsSelected()}
        onCheckedChange={(value) => row.toggleSelected(!!value)}
        aria-label="Select row"
        className="translate-y-[2px]"
      />
    ),
    enableSorting: false,
    enableHiding: false,
  },
  {
    accessorKey: "name",
    header: ({ column }) => {
      return (
        <button
          onClick={() => column.toggleSorting(column.getIsSorted() === "asc")}
          className="flex items-center text-left font-medium group"
        >
          Name
          <ArrowUpDown className="ml-2 h-4 w-4 text-gray-400 group-hover:text-gray-700 transition-colors" />
        </button>
      );
    },
    cell: ({ row }) => {
      const apiKey = row.original;
      return (
        <div className="flex items-center gap-2">
          <Key className="w-4 h-4 text-doubleword-neutral-500" />
          <span className="font-medium">{apiKey.name}</span>
        </div>
      );
    },
  },
  {
    accessorKey: "description",
    header: "Description",
    cell: ({ row }) => {
      const description = row.getValue("description") as string | null;
      return (
        <span className="text-doubleword-neutral-600">
          {description || "-"}
        </span>
      );
    },
  },
  {
    id: "rateLimit",
    header: "Rate Limit",
    cell: ({ row }) => {
      const apiKey = row.original;
      const { requests_per_second, burst_size } = apiKey;

      if (!requests_per_second && !burst_size) {
        return (
          <span className="text-doubleword-neutral-400 text-sm">No limit</span>
        );
      }

      return (
        <div className="text-sm">
          {requests_per_second && (
            <div className="text-doubleword-neutral-700">
              {requests_per_second} req/s
            </div>
          )}
          {burst_size && (
            <div className="text-doubleword-neutral-500">
              burst: {burst_size}
            </div>
          )}
        </div>
      );
    },
  },
  {
    accessorKey: "created_at",
    header: ({ column }) => {
      return (
        <button
          onClick={() => column.toggleSorting(column.getIsSorted() === "asc")}
          className="flex items-center text-left font-medium group"
        >
          Created
          <ArrowUpDown className="ml-2 h-4 w-4 text-gray-400 group-hover:text-gray-700 transition-colors" />
        </button>
      );
    },
    cell: ({ row }) => {
      const date = new Date(row.getValue("created_at"));
      return (
        <span className="text-doubleword-neutral-600">
          {date.toLocaleDateString("en-US", {
            year: "numeric",
            month: "short",
            day: "numeric",
          })}
        </span>
      );
    },
  },
  {
    id: "actions",
    cell: ({ row }) => {
      const apiKey = row.original;

      return (
        <DropdownMenu>
          <DropdownMenuTrigger asChild>
            <Button variant="ghost" className="h-8 w-8 p-0">
              <span className="sr-only">Open menu</span>
              <MoreHorizontal className="h-4 w-4" />
            </Button>
          </DropdownMenuTrigger>
          <DropdownMenuContent align="end">
            <DropdownMenuLabel>Actions</DropdownMenuLabel>
            <DropdownMenuItem
              onClick={() => actions.onDelete(apiKey)}
              className="text-red-600"
            >
              <Trash2 className="mr-2 h-4 w-4" />
              Delete
            </DropdownMenuItem>
          </DropdownMenuContent>
        </DropdownMenu>
      );
    },
  },
];
