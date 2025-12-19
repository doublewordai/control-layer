"use client";

import { type ColumnDef } from "@tanstack/react-table";
import { ArrowUpDown, Clock, ExternalLink, DollarSign } from "lucide-react";
import { formatTimestamp, formatDuration } from "../../../../utils";
import { Tooltip, TooltipContent, TooltipTrigger } from "../../../ui/tooltip";
import type { RequestsEntry } from "../types";
import { Link } from "react-router-dom";

// Calculate estimated cost from tokens and price per token
function calculateCost(entry: RequestsEntry): number | null {
  const promptTokens = entry.prompt_tokens;
  const completionTokens = entry.completion_tokens;
  const inputPrice = entry.input_price_per_token
    ? parseFloat(entry.input_price_per_token)
    : null;
  const outputPrice = entry.output_price_per_token
    ? parseFloat(entry.output_price_per_token)
    : null;

  if (promptTokens === undefined || completionTokens === undefined) {
    return null;
  }

  if (inputPrice === null && outputPrice === null) {
    return null;
  }

  const inputCost = (promptTokens ?? 0) * (inputPrice ?? 0);
  const outputCost = (completionTokens ?? 0) * (outputPrice ?? 0);

  return inputCost + outputCost;
}

// Format cost with appropriate precision
function formatCost(cost: number): string {
  if (cost < 0.0001) {
    return `$${cost.toFixed(6)}`;
  } else if (cost < 0.01) {
    return `$${cost.toFixed(4)}`;
  } else {
    return `$${cost.toFixed(2)}`;
  }
}

const getStatusColor = (statusCode?: number) => {
  if (!statusCode) return "bg-gray-100 text-gray-800";
  if (statusCode >= 200 && statusCode < 300)
    return "bg-green-100 text-green-800";
  if (statusCode >= 400 && statusCode < 500)
    return "bg-yellow-100 text-yellow-800";
  if (statusCode >= 500) return "bg-red-100 text-red-800";
  return "bg-gray-100 text-gray-800";
};

export const createRequestColumns = (): ColumnDef<RequestsEntry>[] => [
  {
    accessorKey: "id",
    header: ({ column }) => {
      return (
        <button
          onClick={() => column.toggleSorting(column.getIsSorted() === "asc")}
          className="flex items-center text-left font-medium group"
        >
          ID
          <ArrowUpDown className="ml-2 h-4 w-4 text-gray-400 group-hover:text-gray-700 transition-colors" />
        </button>
      );
    },
    cell: ({ row }) => {
      const request = row.original;
      return (
        <span className="font-mono text-sm text-doubleword-neutral-900">
          {request.id}
        </span>
      );
    },
    size: 80,
  },
  {
    accessorKey: "timestamp",
    header: ({ column }) => {
      return (
        <button
          onClick={() => column.toggleSorting(column.getIsSorted() === "asc")}
          className="flex items-center text-left font-medium group"
        >
          Timestamp
          <ArrowUpDown className="ml-2 h-4 w-4 text-gray-400 group-hover:text-gray-700 transition-colors" />
        </button>
      );
    },
    cell: ({ row }) => {
      const timestamp = row.getValue("timestamp") as string;
      return (
        <span className="text-doubleword-neutral-900 text-sm">
          {formatTimestamp(timestamp)}
        </span>
      );
    },
    size: 140,
  },
  {
    accessorKey: "model",
    header: ({ column }) => {
      return (
        <button
          onClick={() => column.toggleSorting(column.getIsSorted() === "asc")}
          className="flex items-center text-left font-medium group"
        >
          Model
          <ArrowUpDown className="ml-2 h-4 w-4 text-gray-400 group-hover:text-gray-700 transition-colors" />
        </button>
      );
    },
    cell: ({ row }) => {
      const model = row.getValue("model") as string | undefined;
      if (!model) {
        return <span className="text-gray-400">-</span>;
      }
      return (
        <span className="inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium bg-blue-100 text-blue-800 max-w-full truncate">
          {model}
        </span>
      );
    },
    size: 140,
  },
  {
    accessorKey: "status_code",
    header: ({ column }) => {
      return (
        <button
          onClick={() => column.toggleSorting(column.getIsSorted() === "asc")}
          className="flex items-center text-left font-medium group"
        >
          Status
          <ArrowUpDown className="ml-2 h-4 w-4 text-gray-400 group-hover:text-gray-700 transition-colors" />
        </button>
      );
    },
    cell: ({ row }) => {
      const statusCode = row.getValue("status_code") as number | undefined;
      if (!statusCode) {
        return <span className="text-gray-400">-</span>;
      }
      return (
        <span
          className={`inline-flex items-center px-2 py-0.5 rounded text-xs font-medium ${getStatusColor(statusCode)}`}
        >
          {statusCode}
        </span>
      );
    },
    size: 80,
  },
  {
    accessorKey: "duration_ms",
    header: ({ column }) => {
      return (
        <button
          onClick={() => column.toggleSorting(column.getIsSorted() === "asc")}
          className="flex items-center text-left font-medium group"
        >
          Duration
          <ArrowUpDown className="ml-2 h-4 w-4 text-gray-400 group-hover:text-gray-700 transition-colors" />
        </button>
      );
    },
    cell: ({ row }) => {
      const duration = row.getValue("duration_ms") as number | undefined;
      if (!duration) {
        return <span className="text-gray-400">-</span>;
      }
      return (
        <div className="flex items-center gap-1 text-sm text-doubleword-neutral-900">
          <Clock className="w-3 h-3" />
          {formatDuration(duration)}
        </div>
      );
    },
    size: 100,
  },
  {
    accessorKey: "total_tokens",
    header: ({ column }) => {
      return (
        <button
          onClick={() => column.toggleSorting(column.getIsSorted() === "asc")}
          className="flex items-center text-left font-medium group"
        >
          Tokens
          <ArrowUpDown className="ml-2 h-4 w-4 text-gray-400 group-hover:text-gray-700 transition-colors" />
        </button>
      );
    },
    cell: ({ row }) => {
      const request = row.original;
      const promptTokens = request.prompt_tokens;
      const completionTokens = request.completion_tokens;

      if (promptTokens === undefined && completionTokens === undefined) {
        return <span className="text-gray-400">-</span>;
      }

      return (
        <Tooltip>
          <TooltipTrigger asChild>
            <span className="text-sm text-doubleword-neutral-900 cursor-help">
              {request.total_tokens?.toLocaleString() ?? "-"}
            </span>
          </TooltipTrigger>
          <TooltipContent>
            <div className="text-xs">
              <div>Prompt: {promptTokens?.toLocaleString() ?? "-"}</div>
              <div>Completion: {completionTokens?.toLocaleString() ?? "-"}</div>
            </div>
          </TooltipContent>
        </Tooltip>
      );
    },
    size: 90,
  },
  {
    id: "cost",
    header: ({ column }) => {
      return (
        <button
          onClick={() => column.toggleSorting(column.getIsSorted() === "asc")}
          className="flex items-center text-left font-medium group"
        >
          Cost
          <ArrowUpDown className="ml-2 h-4 w-4 text-gray-400 group-hover:text-gray-700 transition-colors" />
        </button>
      );
    },
    cell: ({ row }) => {
      const request = row.original;
      const cost = calculateCost(request);

      if (cost === null) {
        return <span className="text-gray-400">-</span>;
      }

      const inputCost =
        (request.prompt_tokens ?? 0) *
        (request.input_price_per_token
          ? parseFloat(request.input_price_per_token)
          : 0);
      const outputCost =
        (request.completion_tokens ?? 0) *
        (request.output_price_per_token
          ? parseFloat(request.output_price_per_token)
          : 0);

      return (
        <Tooltip>
          <TooltipTrigger asChild>
            <span className="text-sm text-green-700 font-medium cursor-help flex items-center gap-1">
              <DollarSign className="w-3 h-3" />
              {formatCost(cost).slice(1)}
            </span>
          </TooltipTrigger>
          <TooltipContent>
            <div className="text-xs">
              <div>Input: {formatCost(inputCost)}</div>
              <div>Output: {formatCost(outputCost)}</div>
            </div>
          </TooltipContent>
        </Tooltip>
      );
    },
    size: 90,
  },
  {
    accessorKey: "user_email",
    header: "User",
    cell: ({ row }) => {
      const userEmail = row.getValue("user_email") as string | undefined;
      if (!userEmail) {
        return <span className="text-gray-400">-</span>;
      }
      return (
        <span className="text-sm text-doubleword-neutral-900 truncate max-w-32">
          {userEmail}
        </span>
      );
    },
    size: 140,
  },
  {
    accessorKey: "fusillade_batch_id",
    header: "Batch",
    cell: ({ row }) => {
      const batchId = row.getValue("fusillade_batch_id") as string | undefined;
      if (!batchId) {
        return <span className="text-gray-400">-</span>;
      }
      const shortId = batchId.slice(0, 8);
      return (
        <Link
          to={`/batches/${batchId}`}
          className="inline-flex items-center gap-1 text-sm text-blue-600 hover:text-blue-800 hover:underline"
        >
          <span className="font-mono">{shortId}</span>
          <ExternalLink className="w-3 h-3" />
        </Link>
      );
    },
    size: 100,
  },
  {
    accessorKey: "custom_id",
    header: "Custom ID",
    cell: ({ row }) => {
      const customId = row.getValue("custom_id") as string | undefined;
      if (!customId) {
        return <span className="text-gray-400">-</span>;
      }
      return (
        <Tooltip>
          <TooltipTrigger asChild>
            <span className="font-mono text-sm text-doubleword-neutral-700 truncate max-w-24 block cursor-help">
              {customId}
            </span>
          </TooltipTrigger>
          <TooltipContent>
            <span className="font-mono text-xs">{customId}</span>
          </TooltipContent>
        </Tooltip>
      );
    },
    size: 120,
  },
  {
    accessorKey: "response_type",
    header: "Type",
    cell: ({ row }) => {
      const responseType = row.getValue("response_type") as string | undefined;
      if (!responseType) {
        return <span className="text-gray-400">-</span>;
      }
      return (
        <span className="text-xs text-doubleword-neutral-700">
          {responseType}
        </span>
      );
    },
    size: 100,
  },
];
