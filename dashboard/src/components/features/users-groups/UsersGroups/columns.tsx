"use client";

import { type ColumnDef } from "@tanstack/react-table";
import {
  ArrowUpDown,
  MoreHorizontal,
  Edit2,
  Users,
  Trash2,
  Receipt,
} from "lucide-react";
import { Button } from "../../../ui/button";
import { Checkbox } from "../../../ui/checkbox";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "../../../ui/dropdown-menu";
import { UserAvatar } from "../../../ui/user-avatar";
import type { DisplayUser, DisplayGroup } from "../../../../types/display";

interface UserColumnActions {
  onEdit: (user: DisplayUser) => void;
  onDelete: (user: DisplayUser) => void;
  onManageGroups: (user: DisplayUser) => void;
  onViewTransactions: (user: DisplayUser) => void;
  groups: DisplayGroup[];
  showTransactions?: boolean;
}

// Predefined color classes that Tailwind will include
const GROUP_COLOR_CLASSES = [
  "bg-blue-500",
  "bg-purple-500",
  "bg-green-500",
  "bg-yellow-500",
  "bg-red-500",
  "bg-indigo-500",
  "bg-teal-500",
  "bg-orange-500",
  "bg-pink-500",
  "bg-cyan-500",
];

// Function to get a consistent color for a group
const getGroupColor = (_groupId: string, index: number): string => {
  // Use index to assign colors consistently
  return GROUP_COLOR_CLASSES[index % GROUP_COLOR_CLASSES.length];
};

export const createUserColumns = (
  actions: UserColumnActions,
): ColumnDef<DisplayUser>[] => [
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
          User
          <ArrowUpDown className="ml-2 h-4 w-4 text-gray-400 group-hover:text-gray-700 transition-colors" />
        </button>
      );
    },
    cell: ({ row }) => {
      const user = row.original;
      return (
        <div className="flex items-center gap-3">
          <UserAvatar user={user} size="lg" />
          <div>
            <div className="flex items-center gap-2">
              <p className="font-medium text-doubleword-neutral-900">
                {user.name}
              </p>
              {user.isAdmin && (
                <span className="text-xs px-2 py-0.5 bg-doubleword-primary text-white rounded-full">
                  Admin
                </span>
              )}
            </div>
            <p className="text-sm text-doubleword-neutral-500">{user.email}</p>
          </div>
        </div>
      );
    },
  },
  {
    accessorKey: "groupNames",
    header: "Groups",
    cell: ({ row }) => {
      const user = row.original;
      const groups = actions.groups;

      return (
        <div className="flex flex-wrap gap-1">
          {user.groupNames?.map((groupName, idx) => {
            const groupData = groups.find((g) => g.name === groupName);
            const groupIndex = groupData ? groups.indexOf(groupData) : -1;
            const colorClass = groupData
              ? getGroupColor(groupData.id, groupIndex)
              : "bg-gray-200";
            return (
              <span
                key={idx}
                className={`text-xs px-2 py-1 rounded ${
                  groupData
                    ? `${colorClass} text-white`
                    : "bg-gray-200 text-gray-700"
                }`}
              >
                {groupName}
              </span>
            );
          })}
        </div>
      );
    },
  },
  {
    accessorKey: "updated_at",
    header: ({ column }) => {
      return (
        <button
          onClick={() => column.toggleSorting(column.getIsSorted() === "asc")}
          className="flex items-center text-left font-medium group"
        >
          Last Updated
          <ArrowUpDown className="ml-2 h-4 w-4 text-gray-400 group-hover:text-gray-700 transition-colors" />
        </button>
      );
    },
    cell: ({ row }) => {
      const date = new Date(row.getValue("updated_at"));
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
      const user = row.original;

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
            <DropdownMenuItem onClick={() => actions.onEdit(user)}>
              <Edit2 className="mr-2 h-4 w-4" />
              Edit
            </DropdownMenuItem>
            <DropdownMenuItem onClick={() => actions.onManageGroups(user)}>
              <Users className="mr-2 h-4 w-4" />
              Manage Groups
            </DropdownMenuItem>
            {actions.showTransactions && (
              <DropdownMenuItem onClick={() => actions.onViewTransactions(user)}>
                <Receipt className="mr-2 h-4 w-4" />
                View Transactions
              </DropdownMenuItem>
            )}
            <DropdownMenuSeparator />
            <DropdownMenuItem
              onClick={() => actions.onDelete(user)}
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
