"use client";

import { type ColumnDef } from "@tanstack/react-table";
import {
  ArrowUpDown,
  Clock,
  MessageSquare,
  FileText,
  Layers,
  HelpCircle,
} from "lucide-react";
// Remove dropdown menu imports - no longer needed
import { formatTimestamp, formatDuration } from "../../../../utils";
import {
  HoverCard,
  HoverCardContent,
  HoverCardTrigger,
} from "../../../ui/hover-card";
import { Tooltip, TooltipContent, TooltipTrigger } from "../../../ui/tooltip";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import type { RequestsEntry } from "../types";

const getRequestTypeIcon = (type: RequestsEntry["request_type"]) => {
  switch (type) {
    case "chat_completions":
      return <MessageSquare className="w-4 h-4" />;
    case "completions":
      return <FileText className="w-4 h-4" />;
    case "embeddings":
      return <Layers className="w-4 h-4" />;
    case "other":
    default:
      return <HelpCircle className="w-4 h-4" />;
  }
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
    accessorKey: "request_content",
    header: "Request",
    cell: ({ row }) => {
      const request = row.original;
      return (
        <div className="max-w-60">
          <HoverCard>
            <HoverCardTrigger asChild>
              <div className="text-sm text-doubleword-neutral-900 line-clamp-1 wrap-break-words hover:bg-gray-50 rounded cursor-pointer px-1 truncate">
                <ReactMarkdown
                  remarkPlugins={[remarkGfm]}
                  components={{
                    p: ({ children }) => <>{children}</>,
                    strong: ({ children }) => (
                      <strong className="font-semibold">{children}</strong>
                    ),
                    em: ({ children }) => (
                      <em className="italic">{children}</em>
                    ),
                    code: ({ children }) => (
                      <code className="bg-gray-100 px-1 rounded text-xs">
                        {children}
                      </code>
                    ),
                    // Block elements become inline to prevent line breaks
                    div: ({ children }) => <>{children}</>,
                    pre: ({ children }) => (
                      <code className="bg-gray-100 px-1 rounded text-xs">
                        {children}
                      </code>
                    ),
                    h1: ({ children }) => (
                      <strong className="font-semibold">{children}</strong>
                    ),
                    h2: ({ children }) => (
                      <strong className="font-semibold">{children}</strong>
                    ),
                    h3: ({ children }) => (
                      <strong className="font-semibold">{children}</strong>
                    ),
                    h4: ({ children }) => (
                      <strong className="font-semibold">{children}</strong>
                    ),
                    h5: ({ children }) => (
                      <strong className="font-semibold">{children}</strong>
                    ),
                    h6: ({ children }) => (
                      <strong className="font-semibold">{children}</strong>
                    ),
                    ul: ({ children }) => <>{children}</>,
                    ol: ({ children }) => <>{children}</>,
                    li: ({ children }) => <>{children}</>,
                    blockquote: ({ children }) => <>{children}</>,
                    br: () => <span> </span>,
                  }}
                >
                  {request.request_content}
                </ReactMarkdown>
              </div>
            </HoverCardTrigger>
            <HoverCardContent
              side="top"
              className="w-lg max-h-64 overflow-y-auto"
            >
              <div className="text-sm prose prose-sm max-w-none">
                <ReactMarkdown
                  remarkPlugins={[remarkGfm]}
                  components={{
                    p: ({ children }) => (
                      <p className="mb-2 last:mb-0">{children}</p>
                    ),
                    code: ({ children }) => {
                      return (
                        <code className="bg-gray-100 px-1 py-0.5 rounded text-xs">
                          {children}
                        </code>
                      );
                    },
                    pre: ({ children }) => (
                      <pre className="bg-gray-100 p-2 rounded text-xs overflow-x-auto">
                        {children}
                      </pre>
                    ),
                  }}
                >
                  {request.request_content}
                </ReactMarkdown>
              </div>
            </HoverCardContent>
          </HoverCard>
        </div>
      );
    },
    enableSorting: false,
    size: 240,
  },
  {
    accessorKey: "response_content",
    header: "Response",
    cell: ({ row }) => {
      const request = row.original;
      return (
        <div className="max-w-60">
          <HoverCard>
            <HoverCardTrigger asChild>
              <div className="text-sm text-gray-700 line-clamp-1 wrap-break-words hover:bg-gray-50 rounded cursor-pointer px-1 truncate">
                <ReactMarkdown
                  remarkPlugins={[remarkGfm]}
                  components={{
                    p: ({ children }) => <>{children}</>,
                    strong: ({ children }) => (
                      <strong className="font-semibold">{children}</strong>
                    ),
                    em: ({ children }) => (
                      <em className="italic">{children}</em>
                    ),
                    code: ({ children }) => (
                      <code className="bg-gray-100 px-1 rounded text-xs">
                        {children}
                      </code>
                    ),
                    // Block elements become inline to prevent line breaks
                    div: ({ children }) => <>{children}</>,
                    pre: ({ children }) => (
                      <code className="bg-gray-100 px-1 rounded text-xs">
                        {children}
                      </code>
                    ),
                    h1: ({ children }) => (
                      <strong className="font-semibold">{children}</strong>
                    ),
                    h2: ({ children }) => (
                      <strong className="font-semibold">{children}</strong>
                    ),
                    h3: ({ children }) => (
                      <strong className="font-semibold">{children}</strong>
                    ),
                    h4: ({ children }) => (
                      <strong className="font-semibold">{children}</strong>
                    ),
                    h5: ({ children }) => (
                      <strong className="font-semibold">{children}</strong>
                    ),
                    h6: ({ children }) => (
                      <strong className="font-semibold">{children}</strong>
                    ),
                    ul: ({ children }) => <>{children}</>,
                    ol: ({ children }) => <>{children}</>,
                    li: ({ children }) => <>{children}</>,
                    blockquote: ({ children }) => <>{children}</>,
                    br: () => <span> </span>,
                    // Show raw table markdown for inline display
                    table: ({ node }) => {
                      const tableText =
                        node?.position?.start && node?.position?.end
                          ? request.response_content.slice(
                              node.position.start.offset,
                              node.position.end.offset,
                            )
                          : "[table]";
                      return <span>{tableText}</span>;
                    },
                    thead: () => null,
                    tbody: () => null,
                    tr: () => null,
                    th: () => null,
                    td: () => null,
                  }}
                >
                  {request.response_content}
                </ReactMarkdown>
              </div>
            </HoverCardTrigger>
            <HoverCardContent
              side="top"
              className="w-lg max-h-64 overflow-y-auto"
            >
              <div className="text-sm prose prose-sm max-w-none">
                <ReactMarkdown
                  remarkPlugins={[remarkGfm]}
                  components={{
                    p: ({ children }) => (
                      <p className="mb-2 last:mb-0">{children}</p>
                    ),
                    code: ({ children }) => {
                      return (
                        <code className="bg-gray-100 px-1 py-0.5 rounded text-xs">
                          {children}
                        </code>
                      );
                    },
                    pre: ({ children }) => (
                      <pre className="bg-gray-100 p-2 rounded text-xs overflow-x-auto">
                        {children}
                      </pre>
                    ),
                    table: ({ children }) => (
                      <div className="overflow-x-auto">
                        <table className="min-w-full text-xs border-collapse">
                          {children}
                        </table>
                      </div>
                    ),
                    thead: ({ children }) => (
                      <thead className="bg-gray-50">{children}</thead>
                    ),
                    tbody: ({ children }) => <tbody>{children}</tbody>,
                    tr: ({ children }) => (
                      <tr className="border-b border-gray-200">{children}</tr>
                    ),
                    th: ({ children }) => (
                      <th className="px-2 py-1 text-left font-medium text-gray-900 border-r border-gray-300 last:border-r-0">
                        {children}
                      </th>
                    ),
                    td: ({ children }) => (
                      <td className="px-2 py-1 border-r border-gray-300 last:border-r-0">
                        {children}
                      </td>
                    ),
                  }}
                >
                  {request.response_content}
                </ReactMarkdown>
              </div>
            </HoverCardContent>
          </HoverCard>
        </div>
      );
    },
    enableSorting: false,
    size: 240,
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
    size: 120,
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
      const model = row.getValue("model") as string;
      return (
        <span className="inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium bg-blue-100 text-blue-800 max-w-full truncate">
          {model}
        </span>
      );
    },
    size: 120,
  },
  {
    accessorKey: "usage.total_tokens",
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
      const usage = request.usage;
      const isStreaming =
        request.details.response?.body?.type === "chat_completions_stream";

      if (!usage) {
        if (isStreaming) {
          return (
            <Tooltip>
              <TooltipTrigger asChild>
                <span className="text-orange-600 text-xs">No usage data</span>
              </TooltipTrigger>
              <TooltipContent>
                <div className="text-sm">
                  <p className="font-medium mb-1">
                    Missing usage data for streaming response
                  </p>
                  <p className="mb-2">
                    To include usage data in streaming responses, add:
                  </p>
                  <pre className="px-2 py-1 rounded text-xs font-mono">
                    {`"stream_options": {"include_usage": true}`}
                  </pre>
                </div>
              </TooltipContent>
            </Tooltip>
          );
        }
        return <span className="text-gray-400">-</span>;
      }

      return (
        <span className="text-sm text-doubleword-neutral-900">
          {usage.prompt_tokens} in/{usage.completion_tokens} out
        </span>
      );
    },
    size: 90,
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
      const duration = row.getValue("duration_ms") as number;
      return (
        <div className="flex items-center gap-1 text-sm text-doubleword-neutral-900">
          <Clock className="w-3 h-3" />
          {formatDuration(duration)}
        </div>
      );
    },
    size: 90,
  },
  {
    accessorKey: "request_type",
    header: "Type",
    cell: ({ row }) => {
      const request = row.original;
      return (
        <div className="flex justify-center">
          <Tooltip>
            <TooltipTrigger asChild>
              <div>{getRequestTypeIcon(request.request_type)}</div>
            </TooltipTrigger>
            <TooltipContent>{request.request_type}</TooltipContent>
          </Tooltip>
        </div>
      );
    },
    enableSorting: false,
    size: 50,
  },
];
