/* eslint-disable react-refresh/only-export-components */
import { type ColumnDef } from "@tanstack/react-table";
import { ArrowUpDown } from "lucide-react";
import type { FileRequest } from "../../../../api/control-layer/types";
import { Checkbox } from "../../../ui/checkbox";

export type FileRequestOrResponse =
  | FileRequest
  | {
      id?: string;
      custom_id: string;
      response?: { status_code: number; body: any } | null;
      error?: { code: string; message: string } | null;
    };

/**
 * Button component to trigger viewing request body
 * Modal state is managed at parent level to prevent loss during re-renders
 */
function RequestBodyButton({
  request,
  isOutput,
  onView,
}: {
  request: FileRequestOrResponse;
  isOutput: boolean;
  onView: (request: FileRequestOrResponse) => void;
}) {
  // Determine what to show based on whether it's an output file or input file
  let content: any;

  if (isOutput) {
    // Output/Error file - show response or error
    const outputRequest = request as {
      response?: any;
      error?: any;
      custom_id: string;
    };
    if (outputRequest.error) {
      content = outputRequest.error;
    } else if (outputRequest.response) {
      content = outputRequest.response.body || outputRequest.response;
    } else {
      content = null;
    }
  } else {
    // Input file - show request body
    const inputRequest = request as FileRequest;
    content = inputRequest.body;
  }

  // Generate preview text
  const previewText = content
    ? typeof content === "string"
      ? content
      : JSON.stringify(content)
    : "No content";
  const truncatedPreview =
    previewText.length > 50
      ? previewText.substring(0, 50) + "..."
      : previewText;

  return (
    <button
      onClick={() => onView(request)}
      className="text-left text-sm text-gray-700 hover:text-blue-600 transition-colors cursor-pointer font-mono"
      title="Click to view full content"
    >
      {truncatedPreview}
    </button>
  );
}

export const createFileRequestsColumns = (
  isOutput: boolean,
  onViewRequestBody: (request: FileRequestOrResponse) => void,
  enableSelection = false,
): ColumnDef<FileRequestOrResponse>[] => [
  // Checkbox column for selection (only shown if enableSelection is true)
  ...(enableSelection
    ? ([
        {
          id: "select",
          header: ({ table }) => (
            <Checkbox
              checked={table.getIsAllPageRowsSelected()}
              onCheckedChange={(value) =>
                table.toggleAllPageRowsSelected(!!value)
              }
              aria-label="Select all"
            />
          ),
          cell: ({ row }) => (
            <Checkbox
              checked={row.getIsSelected()}
              onCheckedChange={(value) => row.toggleSelected(!!value)}
              aria-label="Select row"
            />
          ),
          enableSorting: false,
          enableHiding: false,
        },
      ] as ColumnDef<FileRequestOrResponse>[])
    : []),
  {
    accessorKey: "custom_id",
    header: ({ column }) => {
      return (
        <button
          onClick={() => column.toggleSorting(column.getIsSorted() === "asc")}
          className="flex items-center text-left font-medium group"
        >
          Custom ID
          <ArrowUpDown className="ml-2 h-4 w-4 text-gray-400 group-hover:text-gray-700 transition-colors" />
        </button>
      );
    },
    cell: ({ row }) => {
      return (
        <span className="font-medium text-gray-900">
          {row.getValue("custom_id")}
        </span>
      );
    },
  },
  // Only show method and URL columns for input files
  ...(!isOutput
    ? ([
        {
          accessorKey: "method",
          header: "Method",
          cell: ({ row }: { row: any }) => {
            return (
              <span className="font-mono text-xs px-2 py-1 bg-gray-100 rounded">
                {row.getValue("method")}
              </span>
            );
          },
        },
        {
          accessorKey: "url",
          header: "URL",
          cell: ({ row }: { row: any }) => {
            return (
              <span className="text-gray-700 font-mono text-xs">
                {row.getValue("url")}
              </span>
            );
          },
        },
      ] as ColumnDef<FileRequestOrResponse>[])
    : []),
  // Show status column for output files
  ...(isOutput
    ? ([
        {
          id: "status",
          header: "Status",
          cell: ({ row }: { row: any }) => {
            const request = row.original as {
              response?: { status_code: number; body: any };
              error?: any;
            };
            const hasError =
              !!request.error ||
              (request.response && request.response.status_code >= 400);
            return (
              <span
                className={`inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium ${
                  hasError
                    ? "bg-red-100 text-red-800"
                    : "bg-green-100 text-green-800"
                }`}
              >
                {hasError ? "Error" : "Success"}
              </span>
            );
          },
        },
      ] as ColumnDef<FileRequestOrResponse>[])
    : []),
  {
    id: "body",
    header: isOutput ? "Content" : "Request Body",
    cell: ({ row }) => {
      const request = row.original;
      return (
        <RequestBodyButton
          request={request}
          isOutput={isOutput}
          onView={onViewRequestBody}
        />
      );
    },
  },
];
