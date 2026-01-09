"use client";

import { type ColumnDef } from "@tanstack/react-table";
import {
  ArrowUpDown,
  Trash2,
  List,
  FileInput,
  Download,
  Play,
  FileCheck,
  AlertCircle,
  Layers,
  Loader2,
} from "lucide-react";
import { Button } from "../../../ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "../../../ui/tooltip";
import { formatBytes, formatTimestamp } from "../../../../utils";
import type { FileObject } from "../types";
import type { FileCostEstimate } from "../../../../api/control-layer/types";

interface ColumnActions {
  onView: (file: FileObject) => void;
  onDelete: (file: FileObject) => void;
  onDownloadCode: (file: FileObject) => void;
  onTriggerBatch: (file: FileObject) => void;
  onViewBatches: (file: FileObject) => void;
  isFileInProgress: (file: FileObject) => boolean;
  fileEstimates: Map<string, FileCostEstimate>;
}

export const createFileColumns = (
  actions: ColumnActions,
): ColumnDef<FileObject>[] => [
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

      // Check if file is in progress
      const isInProgress = actions.isFileInProgress(file);

      // Choose icon based on purpose and progress
      let icon = <FileInput className="w-4 h-4 text-gray-500" />;
      if (isInProgress) {
        icon = <Loader2 className="w-4 h-4 text-blue-600 animate-spin" />;
      } else if (file.purpose === "batch_output") {
        icon = <FileCheck className="w-4 h-4 text-green-600" />;
      } else if (file.purpose === "batch_error") {
        icon = <AlertCircle className="w-4 h-4 text-red-500" />;
      }

      return (
        <div
          className="flex items-center gap-2 cursor-pointer hover:text-blue-600 transition-colors py-0"
          onClick={() => actions.onView(file)}
        >
          {icon}
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
      return <span className="font-mono text-xs text-gray-600">{id}</span>;
    },
    enableHiding: true,
    meta: {
      defaultHidden: true,
    },
  },
  // Hidden until backend expiration enforcement is implemented
  // {
  //   accessorKey: "expires_at",
  //   header: "Expires",
  //   cell: ({ row }) => {
  //     const timestamp = row.getValue("expires_at") as number | undefined;
  //     if (!timestamp) return <span className="text-gray-400">Never</span>;

  //     const expiresDate = new Date(timestamp * 1000);
  //     const now = new Date();
  //     const isExpired = expiresDate < now;

  //     if (isExpired) {
  //       return (
  //         <span className="text-red-600 font-medium">
  //           Expired {formatTimestamp(expiresDate.toISOString())}
  //         </span>
  //       );
  //     }

  //     return (
  //       <span className="text-gray-700">
  //         {formatTimestamp(expiresDate.toISOString())}
  //       </span>
  //     );
  //   },
  // },
  {
    accessorKey: "bytes",
    header: ({ column }) => {
      return (
        <button
          onClick={() => column.toggleSorting(column.getIsSorted() === "asc")}
          className="flex items-center justify-end text-right font-medium group w-full"
        >
          Size
          <ArrowUpDown className="ml-2 h-4 w-4 text-gray-400 group-hover:text-gray-700 transition-colors" />
        </button>
      );
    },
    cell: ({ row }) => {
      const bytes = row.getValue("bytes") as number;
      return <div className="text-right text-gray-700">{formatBytes(bytes)}</div>;
    },
  },
  {
    id: "cost_estimate",
    header: () => {
      return (
        <div className="flex items-center justify-end text-right font-medium">Est. Cost</div>
      );
    },
    cell: ({ row }) => {
      const file = row.original;

      // Only show cost estimate for batch input files
      if (file.purpose !== "batch") {
        return <div className="text-right text-gray-400">—</div>;
      }

      // Get estimate from the map
      const estimate = actions.fileEstimates.get(file.id);

      if (!estimate) {
        return <div className="text-right text-gray-400">—</div>;
      }

      const cost = parseFloat(estimate.total_estimated_cost);

      // Format cost with appropriate precision
      const formattedCost =
        cost === 0
          ? "$0.00"
          : cost < 0.01
            ? `$${cost.toFixed(4)}`
            : `$${cost.toFixed(2)}`;

      // Create tooltip content with per-model breakdown
      const tooltipContent = (
        <div className="space-y-1">
          <div className="font-medium">Cost Estimate Breakdown (24hr SLA)</div>
          <div className="text-xs space-y-0.5">
            {estimate.models.map((model) => {
              const modelCost = parseFloat(model.estimated_cost);
              const formattedModelCost =
                modelCost === 0
                  ? "$0.00"
                  : modelCost < 0.01
                    ? `$${modelCost.toFixed(4)}`
                    : `$${modelCost.toFixed(2)}`;

              return (
                <div key={model.model} className="flex justify-between gap-4">
                  <span>{model.model}:</span>
                  <span className="font-mono">{formattedModelCost}</span>
                </div>
              );
            })}
            <div className="pt-1 border-t border-gray-200 flex justify-between gap-4 font-medium">
              <span>Total:</span>
              <span className="font-mono">{formattedCost}</span>
            </div>
          </div>
          <div className="text-xs text-gray-400 mt-2">
            {estimate.total_requests} request
            {estimate.total_requests !== 1 ? "s" : ""}
          </div>
        </div>
      );

      return (
        <div className="text-right">
          <Tooltip delayDuration={300}>
            <TooltipTrigger asChild>
              <span className="text-gray-700 font-mono cursor-help">
                {formattedCost}
              </span>
            </TooltipTrigger>
            <TooltipContent>{tooltipContent}</TooltipContent>
          </Tooltip>
        </div>
      );
    },
  },
  {
    id: "actions",
    header: () => <div className="text-right font-medium">Actions</div>,
    size: 200, // Constrain the actions column width
    cell: ({ row }) => {
      const file = row.original;
      const isExpired =
        file.expires_at && new Date(file.expires_at * 1000) < new Date();

      return (
        <div className="flex items-center justify-end gap-1">
          {!isExpired && file.purpose === "batch" && (
            <Tooltip delayDuration={500}>
              <TooltipTrigger asChild>
                <Button
                  variant="ghost"
                  size="sm"
                  className="h-7 w-7 p-0 text-gray-700 hover:bg-gray-100 hover:text-gray-900"
                  onClick={(e) => {
                    e.stopPropagation();
                    actions.onTriggerBatch(file);
                  }}
                >
                  <Play className="h-4 w-4" />
                </Button>
              </TooltipTrigger>
              <TooltipContent>Trigger Batch</TooltipContent>
            </Tooltip>
          )}
          <Tooltip delayDuration={500}>
            <TooltipTrigger asChild>
              <Button
                variant="ghost"
                size="sm"
                className="h-7 w-7 p-0 text-gray-700 hover:bg-gray-100 hover:text-gray-900"
                onClick={(e) => {
                  e.stopPropagation();
                  actions.onView(file);
                }}
              >
                <List className="h-4 w-4" />
              </Button>
            </TooltipTrigger>
            <TooltipContent>View Requests</TooltipContent>
          </Tooltip>
          {file.purpose === "batch" && (
            <Tooltip delayDuration={500}>
              <TooltipTrigger asChild>
                <Button
                  variant="ghost"
                  size="sm"
                  className="h-7 w-7 p-0 text-gray-700 hover:bg-gray-100 hover:text-gray-900"
                  onClick={(e) => {
                    e.stopPropagation();
                    actions.onViewBatches(file);
                  }}
                >
                  <Layers className="h-4 w-4" />
                </Button>
              </TooltipTrigger>
              <TooltipContent>View Batches</TooltipContent>
            </Tooltip>
          )}
          {!isExpired && (
            <Tooltip delayDuration={500}>
              <TooltipTrigger asChild>
                <Button
                  variant="ghost"
                  size="sm"
                  className="h-7 w-7 p-0 text-gray-700 hover:bg-gray-100 hover:text-gray-900"
                  onClick={(e) => {
                    e.stopPropagation();
                    actions.onDownloadCode(file);
                  }}
                >
                  <Download className="h-4 w-4" />
                </Button>
              </TooltipTrigger>
              <TooltipContent>Download File</TooltipContent>
            </Tooltip>
          )}
          <Tooltip delayDuration={500}>
            <TooltipTrigger asChild>
              <Button
                variant="ghost"
                size="sm"
                className="h-8 w-8 p-0 text-gray-700 hover:bg-red-50 hover:text-red-600"
                onClick={(e) => {
                  e.stopPropagation();
                  actions.onDelete(file);
                }}
              >
                <Trash2 className="h-4 w-4" />
              </Button>
            </TooltipTrigger>
            <TooltipContent>Delete</TooltipContent>
          </Tooltip>
        </div>
      );
    },
  },
];
