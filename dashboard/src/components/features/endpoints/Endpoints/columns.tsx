/* eslint-disable react-refresh/only-export-components */
"use client";

import { type ColumnDef } from "@tanstack/react-table";
import React, { useState } from "react";
import {
  ArrowUpDown,
  MoreHorizontal,
  ExternalLink,
  RefreshCw,
  Edit2,
  Trash2,
  Check,
  X,
} from "lucide-react";
import { Button } from "../../../ui/button";
import { Checkbox } from "../../../ui/checkbox";
import { Input } from "../../../ui/input";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "../../../ui/dropdown-menu";
import { Popover, PopoverContent, PopoverTrigger } from "../../../ui/popover";
import type { Endpoint } from "../../../../api/control-layer/types";

interface ColumnActions {
  onEdit: (
    endpoint: Endpoint,
    field: "name" | "description" | "url",
    value: string,
  ) => void;
  onEditModal: (endpoint: Endpoint) => void;
  onDelete: (endpoint: Endpoint) => void;
  onSynchronize: (endpoint: Endpoint) => void;
  isSynchronizing?: boolean;
}

// Editable cell component
function EditableCell({
  endpoint,
  field,
  value,
  onEdit,
  children,
}: {
  endpoint: Endpoint;
  field: "name" | "description" | "url";
  value: string;
  onEdit?: (
    endpoint: Endpoint,
    field: "name" | "description" | "url",
    value: string,
  ) => void;
  children: React.ReactNode;
}) {
  const [isOpen, setIsOpen] = useState(false);
  const [editValue, setEditValue] = useState(value);

  const handleSave = () => {
    if (onEdit && editValue.trim() !== value) {
      onEdit(endpoint, field, editValue.trim());
    }
    setIsOpen(false);
  };

  const handleCancel = () => {
    setEditValue(value);
    setIsOpen(false);
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Enter") {
      handleSave();
    } else if (e.key === "Escape") {
      handleCancel();
    }
  };

  return (
    <div className="group/edit-cell flex items-center gap-1">
      {children}
      {onEdit && (
        <Popover open={isOpen} onOpenChange={setIsOpen}>
          <PopoverTrigger asChild>
            <Edit2 className="h-3.5 w-3.5 opacity-0 group-hover/edit-cell:opacity-100 transition-opacity cursor-pointer text-doubleword-neutral-600 hover:text-doubleword-neutral-900" />
          </PopoverTrigger>
          <PopoverContent className="w-80" align="start">
            <div className="space-y-2">
              <h4 className="font-medium text-sm">
                Edit{" "}
                {field === "name"
                  ? "Name"
                  : field === "description"
                    ? "Description"
                    : "URL"}
              </h4>
              <div className="flex gap-2">
                <Input
                  value={editValue}
                  onChange={(e) => setEditValue(e.target.value)}
                  onKeyDown={handleKeyDown}
                  placeholder={`Enter ${field}...`}
                  autoFocus
                  className="flex-1"
                />
                <Button
                  size="icon"
                  variant="ghost"
                  className="h-8 w-8"
                  onClick={handleSave}
                >
                  <Check className="h-4 w-4" />
                </Button>
                <Button
                  size="icon"
                  variant="ghost"
                  className="h-8 w-8"
                  onClick={handleCancel}
                >
                  <X className="h-4 w-4" />
                </Button>
              </div>
            </div>
          </PopoverContent>
        </Popover>
      )}
    </div>
  );
}

export const createColumns = (
  actions: ColumnActions,
): ColumnDef<Endpoint>[] => [
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
      const endpoint = row.original;
      return (
        <EditableCell
          endpoint={endpoint}
          field="name"
          value={endpoint.name}
          onEdit={actions.onEdit}
        >
          <div className="flex items-center gap-2">
            <span className="font-medium">{endpoint.name}</span>
            <a
              href={endpoint.url}
              target="_blank"
              rel="noopener noreferrer"
              className="text-doubleword-accent-blue hover:text-blue-700 transition-colors"
            >
              <ExternalLink className="w-3.5 h-3.5" />
            </a>
          </div>
        </EditableCell>
      );
    },
  },
  {
    accessorKey: "url",
    header: "URL",
    cell: ({ row }) => {
      const endpoint = row.original;
      return (
        <EditableCell
          endpoint={endpoint}
          field="url"
          value={endpoint.url}
          onEdit={actions.onEdit}
        >
          <span className="text-doubleword-neutral-600 font-mono text-xs">
            {endpoint.url}
          </span>
        </EditableCell>
      );
    },
  },
  {
    accessorKey: "description",
    header: "Description",
    cell: ({ row }) => {
      const endpoint = row.original;
      const description = row.getValue("description") as string | null;
      return (
        <EditableCell
          endpoint={endpoint}
          field="description"
          value={description || ""}
          onEdit={actions.onEdit}
        >
          <span className="text-doubleword-neutral-600">
            {description || "-"}
          </span>
        </EditableCell>
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
          {date.toLocaleDateString()}
        </span>
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
      const endpoint = row.original;

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
            <DropdownMenuItem onClick={() => actions.onEditModal(endpoint)}>
              <Edit2 className="mr-2 h-4 w-4" />
              Edit
            </DropdownMenuItem>
            <DropdownMenuItem
              onClick={() => actions.onSynchronize(endpoint)}
              disabled={actions.isSynchronizing}
            >
              <RefreshCw className="mr-2 h-4 w-4" />
              Synchronize
            </DropdownMenuItem>
            <DropdownMenuSeparator />
            <DropdownMenuItem
              onClick={() => actions.onDelete(endpoint)}
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
