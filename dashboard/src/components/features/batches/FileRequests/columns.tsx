"use client";

import { type ColumnDef } from "@tanstack/react-table";
import { ArrowUpDown, Eye } from "lucide-react";
import { useState } from "react";
import { Prism as SyntaxHighlighter } from "react-syntax-highlighter";
import { oneDark } from "react-syntax-highlighter/dist/esm/styles/prism";
import { Button } from "../../../ui/button";
import { Tooltip, TooltipContent, TooltipTrigger } from "../../../ui/tooltip";
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from "../../../ui/dialog";
import type { FileRequest } from "../../../../api/control-layer/types";

type FileRequestOrResponse =
  | FileRequest
  | {
      id?: string;
      custom_id: string;
      response?: { status_code: number; body: any } | null;
      error?: { code: string; message: string } | null;
    };

function RequestBodyModal({
  request,
  isOutput,
}: {
  request: FileRequestOrResponse;
  isOutput: boolean;
}) {
  const [open, setOpen] = useState(false);

  // Determine what to show based on whether it's an output file or input file
  let content: any;
  let title: string;

  if (isOutput) {
    // Output/Error file - show response or error
    const outputRequest = request as {
      response?: any;
      error?: any;
      custom_id: string;
    };
    if (outputRequest.error) {
      content = outputRequest.error;
      title = `Error: ${request.custom_id}`;
    } else if (outputRequest.response) {
      content = outputRequest.response.body || outputRequest.response;
      title = `Response: ${request.custom_id}`;
    } else {
      content = null;
      title = request.custom_id;
    }
  } else {
    // Input file - show request body
    const inputRequest = request as FileRequest;
    content = inputRequest.body;
    title = `Request Body: ${request.custom_id}`;
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
    <>
      <button
        onClick={() => setOpen(true)}
        className="text-left text-sm text-gray-700 hover:text-blue-600 transition-colors cursor-pointer font-mono"
        title="Click to view full content"
      >
        {truncatedPreview}
      </button>
      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent className="sm:max-w-4xl max-h-[80vh] flex flex-col">
          <DialogHeader>
            <DialogTitle>{title}</DialogTitle>
          </DialogHeader>
          <div className="overflow-auto flex-1 min-h-0">
            {content ? (
              <SyntaxHighlighter
                language="json"
                style={oneDark}
                customStyle={{
                  margin: 0,
                  borderRadius: "0.375rem",
                  fontSize: "0.75rem",
                  maxHeight: "none",
                }}
              >
                {JSON.stringify(content, null, 2)}
              </SyntaxHighlighter>
            ) : (
              <p className="text-gray-500 text-sm p-4">No content available</p>
            )}
          </div>
        </DialogContent>
      </Dialog>
    </>
  );
}

export const createFileRequestsColumns = (
  isOutput: boolean,
): ColumnDef<FileRequestOrResponse>[] => [
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
      return <RequestBodyModal request={request} isOutput={isOutput} />;
    },
  },
];
