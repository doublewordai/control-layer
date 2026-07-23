"use client";

import { type ColumnDef } from "@tanstack/react-table";
import { ArrowUpDown, Trash2, Key, Pencil } from "lucide-react";
import { Button } from "../../../ui/button";
import { Checkbox } from "../../../ui/checkbox";
import type { ApiKey } from "../../../../api/control-layer/types";
import { formatCredits, formatResetInstant } from "./spendCap";

interface ColumnActions {
  onDelete: (apiKey: ApiKey) => void;
  onEdit: (apiKey: ApiKey) => void;
  isPlatformManager?: boolean;
}

export const createColumns = (actions: ColumnActions): ColumnDef<ApiKey>[] => {
  const allColumns: ColumnDef<ApiKey>[] = [
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
          className="translate-y-0.5"
        />
      ),
      cell: ({ row }) => (
        <Checkbox
          checked={row.getIsSelected()}
          onCheckedChange={(value) => row.toggleSelected(!!value)}
          aria-label="Select row"
          className="translate-y-0.5"
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
      accessorKey: "purpose",
      header: "Purpose",
      cell: ({ row }) => {
        const purpose = row.getValue("purpose") as string;
        const isPlatform = purpose === "platform";
        return (
          <span
            className={`inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium ${
              isPlatform
                ? "bg-purple-100 text-purple-800"
                : "bg-blue-100 text-blue-800"
            }`}
          >
            {isPlatform ? "Platform" : "Inference"}
          </span>
        );
      },
    },
    // {
    //   id: "rateLimit",
    //   header: "Rate Limit",
    //   cell: ({ row }) => {
    //     const apiKey = row.original;
    //     const { requests_per_second, burst_size } = apiKey;

    //     if (!requests_per_second && !burst_size) {
    //       return (
    //         <span className="text-doubleword-neutral-400 text-sm">
    //           No limit
    //         </span>
    //       );
    //     }

    //     return (
    //       <div className="text-sm">
    //         {requests_per_second && (
    //           <div className="text-doubleword-neutral-700">
    //             {requests_per_second} req/s
    //           </div>
    //         )}
    //         {burst_size && (
    //           <div className="text-doubleword-neutral-500">
    //             burst: {burst_size}
    //           </div>
    //         )}
    //       </div>
    //     );
    //   },
    // },
    {
      id: "spend",
      header: "Spend",
      cell: ({ row }) => {
        const apiKey = row.original;
        // Uncapped keys show no spend: their usage isn't tracked against a
        // budget (and any leftover numbers from a removed cap are frozen).
        if (apiKey.spend_limit === null || apiKey.spend_limit === undefined) {
          return <span className="text-doubleword-neutral-400 text-sm">—</span>;
        }
        const spent = Number(apiKey.spend ?? 0);
        const limit = Number(apiKey.spend_limit);
        const capReached = limit > 0 && spent >= limit;
        const pct =
          limit > 0 ? Math.min(100, Math.round((spent / limit) * 100)) : 0;
        return (
          <div className="min-w-32" aria-label="Spending cap usage">
            <div className="flex items-center gap-2 text-sm">
              <span className="text-doubleword-neutral-700">
                {formatCredits(apiKey.spend ?? "0")} /{" "}
                {formatCredits(apiKey.spend_limit)}
              </span>
              {capReached && (
                <span className="inline-flex items-center px-2 py-0.5 rounded-full text-xs font-medium bg-red-100 text-red-800">
                  Cap reached
                </span>
              )}
            </div>
            <div
              className="mt-1 h-1.5 w-full rounded bg-doubleword-neutral-100"
              role="progressbar"
              aria-valuenow={pct}
              aria-valuemin={0}
              aria-valuemax={100}
            >
              <div
                className={`h-1.5 rounded ${capReached ? "bg-red-500" : "bg-doubleword-neutral-500"}`}
                style={{ width: `${pct}%` }}
              />
            </div>
            <div className="mt-0.5 text-xs text-doubleword-neutral-500">
              {apiKey.resets_at
                ? `Resets ${formatResetInstant(apiKey.resets_at)}`
                : "No automatic reset"}
            </div>
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
      header: "Actions",
      cell: ({ row }) => {
        const apiKey = row.original;

        return (
          <div className="flex items-center">
            <Button
              variant="ghost"
              size="sm"
              onClick={() => actions.onEdit(apiKey)}
              aria-label={`Edit spending cap for ${apiKey.name}`}
              className="text-doubleword-neutral-600 hover:text-doubleword-neutral-900"
            >
              <Pencil className="h-4 w-4" />
            </Button>
            <Button
              variant="ghost"
              size="sm"
              onClick={() => actions.onDelete(apiKey)}
              aria-label={`Delete ${apiKey.name}`}
              className="text-red-600 hover:text-red-700 hover:bg-red-50"
            >
              <Trash2 className="h-4 w-4" />
            </Button>
          </div>
        );
      },
    },
  ];

  // Filter out the purpose column if user is not a platform manager
  if (!actions.isPlatformManager) {
    return allColumns.filter(
      (col) => !("accessorKey" in col) || col.accessorKey !== "purpose",
    );
  }

  return allColumns;
};
