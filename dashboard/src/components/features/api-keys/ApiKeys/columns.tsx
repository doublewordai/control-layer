"use client";

import { type ColumnDef } from "@tanstack/react-table";
import { ArrowUpDown, Trash2, Pencil, RefreshCw, Eye } from "lucide-react";
import { Button } from "../../../ui/button";
import { Checkbox } from "../../../ui/checkbox";
import type { ApiKey } from "../../../../api/control-layer/types";
import { formatCredits, formatResetInstant, limitPeriodLabel } from "./spendCap";

interface ColumnActions {
  onDelete: (apiKey: ApiKey) => void;
  onEdit: (apiKey: ApiKey) => void;
  /** Whether the current user may manage this key (rename, edit caps,
   *  delete). Mirrors what the server permits: PlatformManager, org
   *  owner/admin in org context, or the key's creator when they hold
   *  self-manage rights in the active context. */
  canManage: (apiKey: ApiKey) => boolean;
  /** Whether the current user may rotate this key. Broader than canManage:
   *  rotation is the secret-recovery path, so members without creation
   *  rights can still rotate keys they hold. */
  canRotate: (apiKey: ApiKey) => boolean;
  isPlatformManager?: boolean;
  /** Org context only: show the holder of each key, resolved from the org
   *  members list via created_by. */
  showAssignee?: boolean;
  resolveAssignee?: (createdBy: string) => string;
  onRotate: (apiKey: ApiKey) => void;
  /** Whether the current user may use the ONE-OFF reveal on this key: they
   *  are its holder and the reveal hasn't been consumed. While true, Reveal
   *  replaces Rotate for them (rotating before ever seeing the secret makes
   *  no sense); admins keep plain Rotate throughout. */
  canReveal: (apiKey: ApiKey) => boolean;
  onReveal: (apiKey: ApiKey) => void;
  /** Hide the bulk-select column (e.g. members without key-creation rights
   *  can't delete). */
  showSelect?: boolean;
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
        const isPlatform = apiKey.purpose === "platform";
        return (
          <div className="flex flex-col gap-0.5">
            <div className="flex items-center gap-2">
              <span className="font-semibold text-doubleword-neutral-900">
                {apiKey.name}
              </span>
              <span
                className={`inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium ${
                  isPlatform
                    ? "bg-purple-100 text-purple-800"
                    : "bg-blue-100 text-blue-800"
                }`}
              >
                {isPlatform ? "Platform" : "Inference"}
              </span>
            </div>
            {apiKey.description && (
              <span className="text-sm text-doubleword-neutral-600">
                {apiKey.description}
              </span>
            )}
          </div>
        );
      },
    },
    {
      id: "assignee",
      header: "Assignee",
      cell: ({ row }) => {
        const apiKey = row.original;
        return (
          <div className="flex flex-col gap-0.5">
            <span className="text-sm text-doubleword-neutral-700">
              {actions.resolveAssignee
                ? actions.resolveAssignee(apiKey.created_by)
                : apiKey.created_by}
            </span>
            {/* Issued-key engagement signal for managers: once the holder
                has opened (revealed) the key it's live on their side —
                don't rotate it without cause. */}
            {apiKey.secret_revealed_at == null ? (
              <span className="text-xs text-amber-700">Not opened yet</span>
            ) : (
              <span className="text-xs text-doubleword-neutral-400">
                Opened{" "}
                {/* Fixed to UTC: local-tz rendering shifts dates around
                    midnight per viewer and makes tests flaky. */}
                {new Date(apiKey.secret_revealed_at).toLocaleDateString(
                  "en-US",
                  {
                    year: "numeric",
                    month: "short",
                    day: "numeric",
                    timeZone: "UTC",
                  },
                )}
              </span>
            )}
          </div>
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
      id: "usageLimit",
      header: "Usage Limit",
      cell: ({ row }) => {
        const apiKey = row.original;
        if (apiKey.spend_limit === null || apiKey.spend_limit === undefined) {
          return (
            <span className="text-doubleword-neutral-400 text-sm italic">
              No limit
            </span>
          );
        }
        const spent = Number(apiKey.spend ?? 0);
        const limit = Number(apiKey.spend_limit);
        const capReached = limit > 0 && spent >= limit;
        const pct =
          limit > 0 ? Math.min(100, Math.round((spent / limit) * 100)) : 0;
        return (
          <div className="min-w-40 flex flex-col gap-0.5" aria-label="Usage limit">
            <span className="flex items-center gap-2 text-sm font-medium text-doubleword-neutral-900">
              {formatCredits(apiKey.spend ?? "0")} /{" "}
              {formatCredits(apiKey.spend_limit)}
              {capReached && (
                <span className="inline-flex items-center px-2 py-0.5 rounded-full text-xs font-medium bg-red-100 text-red-800">
                  Cap reached
                </span>
              )}
            </span>
            <div
              className="h-1.5 w-full rounded bg-doubleword-neutral-100"
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
            <span className="text-sm text-doubleword-neutral-600">
              {limitPeriodLabel(apiKey.spend_limit_interval)}
              {apiKey.resets_at &&
                ` · resets ${formatResetInstant(apiKey.resets_at)}`}
            </span>
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
            Created:{" "}
            {date.toLocaleDateString("en-GB", {
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
        const canManage = actions.canManage(apiKey);
        const canReveal = actions.canReveal(apiKey);
        // While the holder's one-off reveal is pending, it REPLACES rotate
        // for them — rotating a secret you've never seen makes no sense.
        const canRotate = !canReveal && actions.canRotate(apiKey);

        if (!canManage && !canRotate && !canReveal) {
          // View-only rows: no mutating actions.
          return null;
        }

        return (
          <div className="flex items-center">
            {canManage && (
              <Button
                variant="ghost"
                size="sm"
                onClick={() => actions.onEdit(apiKey)}
                aria-label={`Edit usage limit for ${apiKey.name}`}
                className="text-doubleword-neutral-600 hover:text-doubleword-neutral-900"
              >
                <Pencil className="h-4 w-4" />
              </Button>
            )}
            {canReveal && (
              <Button
                variant="ghost"
                size="sm"
                onClick={() => actions.onReveal(apiKey)}
                aria-label={`Reveal ${apiKey.name}`}
                className="text-doubleword-neutral-600 hover:text-doubleword-neutral-900"
              >
                <Eye className="h-4 w-4" />
              </Button>
            )}
            {canRotate && (
              <Button
                variant="ghost"
                size="sm"
                onClick={() => actions.onRotate(apiKey)}
                aria-label={`Rotate ${apiKey.name}`}
                className="text-doubleword-neutral-600 hover:text-doubleword-neutral-900"
              >
                <RefreshCw className="h-4 w-4" />
              </Button>
            )}
            {canManage && (
              <Button
                variant="ghost"
                size="sm"
                onClick={() => actions.onDelete(apiKey)}
                aria-label={`Delete ${apiKey.name}`}
                className="text-red-600 hover:text-red-700 hover:bg-red-50"
              >
                <Trash2 className="h-4 w-4" />
              </Button>
            )}
          </div>
        );
      },
    },
  ];

  // The key-type badge rides inline on the Name column for everyone; the
  // Assignee column only appears in org context, and the bulk-select column
  // is dropped for users who can't delete anything anyway.
  return allColumns.filter((col) => {
    if (col.id === "assignee" && !actions.showAssignee) return false;
    if (col.id === "select" && actions.showSelect === false) return false;
    return true;
  });
};
