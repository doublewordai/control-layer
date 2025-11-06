"use client";

import { type ColumnDef } from "@tanstack/react-table";
import {
  ArrowUpDown,
  MoreHorizontal,
  Trash2,
  Eye,
  Download,
  Clock,
  FileText,
} from "lucide-react";
import { Button } from "../../../ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "../../../ui/dropdown-menu";
import { formatBytes, formatTimestamp } from "../../../../utils";
import type { FileObject } from "../types";

interface ColumnActions {
  onView: (file: FileObject) => void;
  onDelete: (file: FileObject) => void;
}

export const createFileColumns = (
  actions: ColumnActions,
): ColumnDef<FileObject>[] => [
  {
    accessorKey: "filename",
    header: ({ column }) => {
      return (
        <button
          onClick={() => column.toggleSorting(column.getIsSorted() === "asc")}
          className="flex items-center text-left font-medium group"
        >
          Filename
          <ArrowUpDown className="ml-2 h-4 w-4 text-gray-400 group-hover:text-gray-700 transition-colors" />
        </button>
      );
    },
    cell: ({ row }) => {
      const file = row.original;
      return (
        <div className="flex items-center gap-2">
          <FileText className="w-4 h-4 text-gray-500" />
          <span className="font-medium">{file.filename}</span>
        </div>
      );
    },
  },
  {
    accessorKey: "id",
    header: "File ID",
    cell: ({ row }) => {
      const id = row.getValue("id") as string;
      return (
        <span className="font-mono text-xs text-gray-600">{id}</span>
      );
    },
  },
  {
    accessorKey: "bytes",
    header: ({ column }) => {
      return (
        <button
          onClick={() => column.toggleSorting(column.getIsSorted() === "asc")}
          className="flex items-center text-left font-medium group"
        >
          Size
          <ArrowUpDown className="ml-2 h-4 w-4 text-gray-400 group-hover:text-gray-700 transition-colors" />
        </button>
      );
    },
    cell: ({ row }) => {
      const bytes = row.getValue("bytes") as number;
      return <span className="text-gray-700">{formatBytes(bytes)}</span>;
    },
  },
  {
    accessorKey: "purpose",
    header: "Purpose",
    cell: ({ row }) => {
      const purpose = row.getValue("purpose") as string;
      return (
        <span className="inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium bg-blue-100 text-blue-800">
          {purpose}
        </span>
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
      const timestamp = row.getValue("created_at") as number;
      return (
        <span className="text-gray-700">
          {formatTimestamp(new Date(timestamp * 1000).toISOString())}
        </span>
      );
    },
  },
  {
    accessorKey: "expires_at",
    header: "Expires",
    cell: ({ row }) => {
      const timestamp = row.getValue("expires_at") as number | undefined;
      if (!timestamp) return <span className="text-gray-400">Never</span>;
      
      const expiresDate = new Date(timestamp * 1000);
      const now = new Date();
      const isExpired = expiresDate < now;
      
      return (
        <div className="flex items-center gap-1">
          <Clock className={`w-3 h-3 ${isExpired ? 'text-red-500' : 'text-gray-500'}`} />
          <span className={isExpired ? 'text-red-600' : 'text-gray-700'}>
            {formatTimestamp(expiresDate.toISOString())}
          </span>
        </div>
      );
    },
  },
  {
    id: "actions",
    cell: ({ row }) => {
      const file = row.original;

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
            <DropdownMenuItem onClick={() => actions.onView(file)}>
              <Eye className="mr-2 h-4 w-4" />
              View Requests
            </DropdownMenuItem>
            <DropdownMenuSeparator />
            <DropdownMenuItem
              onClick={() => actions.onDelete(file)}
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