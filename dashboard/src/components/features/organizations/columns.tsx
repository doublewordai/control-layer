"use client";

import { type ColumnDef } from "@tanstack/react-table";
import { ArrowUpDown, Edit2, Trash2, Eye } from "lucide-react";
import type { Organization } from "@/api/control-layer/types";

interface OrganizationColumnActions {
  onView: (org: Organization) => void;
  onEdit: (org: Organization) => void;
  onDelete: (org: Organization) => void;
  canDelete: boolean;
}

export const createOrganizationColumns = (
  actions: OrganizationColumnActions,
): ColumnDef<Organization>[] => [
  {
    accessorKey: "username",
    header: ({ column }) => (
      <button
        onClick={() => column.toggleSorting(column.getIsSorted() === "asc")}
        className="flex items-center text-left font-medium group"
      >
        Name
        <ArrowUpDown className="ml-2 h-4 w-4 text-gray-400 group-hover:text-gray-700 transition-colors" />
      </button>
    ),
    cell: ({ row }) => {
      const org = row.original;
      return (
        <div>
          <p className="font-medium text-doubleword-neutral-900">
            {org.display_name || org.username}
          </p>
          <p className="text-sm text-doubleword-neutral-500">{org.username}</p>
        </div>
      );
    },
  },
  {
    accessorKey: "email",
    header: "Email",
    cell: ({ row }) => (
      <span className="text-doubleword-neutral-600">
        {row.getValue("email")}
      </span>
    ),
  },
  {
    accessorKey: "member_count",
    header: "Members",
    cell: ({ row }) => (
      <span className="text-doubleword-neutral-600">
        {row.original.member_count ?? "—"}
      </span>
    ),
  },
  {
    accessorKey: "created_at",
    header: ({ column }) => (
      <button
        onClick={() => column.toggleSorting(column.getIsSorted() === "asc")}
        className="flex items-center text-left font-medium group"
      >
        Created
        <ArrowUpDown className="ml-2 h-4 w-4 text-gray-400 group-hover:text-gray-700 transition-colors" />
      </button>
    ),
    cell: ({ row }) => {
      const date = new Date(row.getValue("created_at"));
      return (
        <span className="text-doubleword-neutral-600">
          {date.toLocaleDateString()}
        </span>
      );
    },
  },
  {
    id: "actions",
    cell: ({ row }) => {
      const org = row.original;
      return (
        <div className="flex items-center gap-2">
          <button
            onClick={() => actions.onView(org)}
            className="h-8 w-8 p-0 rounded text-gray-600 hover:text-gray-900 hover:bg-gray-100 transition-all flex items-center justify-center"
            title="View organization"
          >
            <Eye className="h-4 w-4" />
            <span className="sr-only">View</span>
          </button>
          <button
            onClick={() => actions.onEdit(org)}
            className="h-8 w-8 p-0 rounded text-gray-600 hover:text-gray-900 hover:bg-gray-100 transition-all flex items-center justify-center"
            title="Edit organization"
          >
            <Edit2 className="h-4 w-4" />
            <span className="sr-only">Edit</span>
          </button>
          {actions.canDelete && (
            <button
              onClick={() => actions.onDelete(org)}
              className="h-8 w-8 p-0 rounded text-red-600 hover:text-red-700 hover:bg-red-50 transition-all flex items-center justify-center"
              title="Delete organization"
            >
              <Trash2 className="h-4 w-4" />
              <span className="sr-only">Delete</span>
            </button>
          )}
        </div>
      );
    },
  },
];
