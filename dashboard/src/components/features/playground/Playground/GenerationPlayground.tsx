import React, { useRef, useEffect, useState } from "react";
import { Send, Copy, Play, Trash2, X } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { Prism as SyntaxHighlighter } from "react-syntax-highlighter";
import { oneDark } from "react-syntax-highlighter/dist/esm/styles/prism";
import type { Model } from "../../../../api/dwctl/types";
import { Textarea } from "../../../ui/textarea";
import { Button } from "../../../ui/button";

interface Message {
  role: "user" | "assistant" | "system";
  content: string;
  timestamp: Date;
}

interface GenerationPlaygroundProps {
  selectedModel: Model;
  messages: Message[];
  currentMessage: string;
  streamingContent: string;
  isStreaming: boolean;
  error: string | null;
  copiedMessageIndex: number | null;
  onCurrentMessageChange: (value: string) => void;
  onSendMessage: () => void;
  onCopyMessage: (content: string, index: number) => void;
  onKeyDown: (e: React.KeyboardEvent) => void;
  onClearConversation: () => void;
  onCancelStreaming?: () => void;
}

const GenerationPlayground: React.FC<GenerationPlaygroundProps> = ({
  selectedModel,
  messages,
  currentMessage,
  streamingContent,
  isStreaming,
  error,
  copiedMessageIndex,
  onCurrentMessageChange,
  onSendMessage,
  onCopyMessage,
  onKeyDown,
  onClearConversation,
  onCancelStreaming,
}) => {
  const messagesEndRef = useRef<HTMLDivElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const [isHovered, setIsHovered] = useState(false);
  const [copiedCode, setCopiedCode] = useState<string | null>(null);

  const scrollToBottom = () => {
    messagesEndRef.current?.scrollIntoView({ behavior: "smooth" });
  };

  const copyCode = (code: string) => {
    navigator.clipboard.writeText(code);
    setCopiedCode(code);
    setTimeout(() => setCopiedCode(null), 2000);
  };

  useEffect(() => {
    scrollToBottom();
  }, [messages, streamingContent]);

  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.key === "Escape" && isStreaming && onCancelStreaming) {
        e.preventDefault();
        onCancelStreaming();
      }
    };

    document.addEventListener("keydown", handleKeyDown);
    return () => document.removeEventListener("keydown", handleKeyDown);
  }, [isStreaming, onCancelStreaming]);

  return (
    <div className="flex-1 flex flex-col">
      {/* Messages */}
      <div className="flex-1 overflow-y-auto px-8 py-6 bg-white">
        {messages.length === 0 && !streamingContent ? (
          <div className="flex items-center justify-center h-full">
            <div
              className="text-center"
              role="status"
              aria-label="Empty conversation"
            >
              <Play className="w-16 h-16 text-gray-400 mx-auto mb-4" />
              <p className="text-xl text-gray-600 mb-2">
                Test {selectedModel.alias}
              </p>
              <p className="text-gray-500">
                Send a message to start a conversation
              </p>
            </div>
          </div>
        ) : (
          <div className="max-w-4xl mx-auto px-4">
            <div className="space-y-6">
              {messages.map((message, index) => (
                <div key={index}>
                  {message.role === "user" ? (
                    /* User Message - Bubble on Right */
                    <div className="flex justify-end">
                      <div className="max-w-[70%] bg-gray-800 text-white rounded-lg p-4">
                        <div className="flex items-center gap-2 mb-2">
                          <span className="text-sm font-medium opacity-75">
                            You
                          </span>
                          <span className="text-xs opacity-50">
                            {message.timestamp.toLocaleTimeString()}
                          </span>
                        </div>
                        <div className="text-sm whitespace-pre-wrap leading-relaxed">
                          {message.content}
                        </div>
                      </div>
                    </div>
                  ) : (
                    /* AI/System Message - Full Width Document Style */
                    <div className="w-full">
                      <div className="flex items-center gap-2 mb-3">
                        <span className="text-sm font-medium text-gray-600">
                          {message.role === "system" ? "System" : "AI"}
                        </span>
                        <span className="text-xs text-gray-400">
                          {message.timestamp.toLocaleTimeString()}
                        </span>
                      </div>

                      <div
                        className={`text-sm leading-relaxed prose prose-sm max-w-none ${
                          message.role === "system"
                            ? "bg-yellow-50 border border-yellow-200 rounded-lg p-4"
                            : ""
                        }`}
                      >
                        <ReactMarkdown
                          remarkPlugins={[remarkGfm]}
                          components={{
                            p: ({ children }) => (
                              <p className="mb-4 last:mb-0">{children}</p>
                            ),
                            code: ({ children, className }) => {
                              const match = /language-(\w+)/.exec(
                                className || "",
                              );
                              const language = match ? match[1] : "";
                              const codeString = String(children).replace(
                                /\n$/,
                                "",
                              );

                              // Don't syntax highlight markdown blocks - just show them as plain code
                              if (className && language === "markdown") {
                                return (
                                  <div className="relative group">
                                    <pre className="bg-gray-900 text-gray-100 p-4 rounded-lg overflow-x-auto text-sm my-4">
                                      <code>{children}</code>
                                    </pre>
                                    <button
                                      onClick={() => copyCode(codeString)}
                                      className="absolute top-2 right-2 p-2 bg-gray-700 hover:bg-gray-600 rounded transition-all duration-200 active:scale-95"
                                      title="Copy code"
                                    >
                                      <Copy className="w-4 h-4 text-gray-300" />
                                      {copiedCode === codeString && (
                                        <span className="absolute -top-8 right-0 bg-gray-800 text-white text-xs px-2 py-1 rounded">
                                          Copied!
                                        </span>
                                      )}
                                    </button>
                                  </div>
                                );
                              }

                              return className ? (
                                <div className="relative group">
                                  <SyntaxHighlighter
                                    style={oneDark}
                                    language={language}
                                    PreTag="div"
                                    className="my-4 text-sm rounded-lg"
                                  >
                                    {codeString}
                                  </SyntaxHighlighter>
                                  <button
                                    onClick={() => copyCode(codeString)}
                                    className="absolute top-2 right-2 p-2 bg-gray-700 hover:bg-gray-600 rounded transition-all duration-200 active:scale-95"
                                    title="Copy code"
                                  >
                                    <Copy className="w-4 h-4 text-gray-300" />
                                    {copiedCode === codeString && (
                                      <span className="absolute -top-8 right-0 bg-gray-800 text-white text-xs px-2 py-1 rounded">
                                        Copied!
                                      </span>
                                    )}
                                  </button>
                                </div>
                              ) : (
                                <code className="bg-gray-100 text-gray-800 px-2 py-1 rounded text-sm font-mono">
                                  {children}
                                </code>
                              );
                            },
                            ul: ({ children }) => (
                              <ul className="list-disc list-inside mb-4 space-y-1">
                                {children}
                              </ul>
                            ),
                            ol: ({ children }) => (
                              <ol className="list-decimal list-inside mb-4 space-y-1">
                                {children}
                              </ol>
                            ),
                            li: ({ children }) => (
                              <li className="">{children}</li>
                            ),
                            h1: ({ children }) => (
                              <h1 className="text-xl font-bold mb-3 mt-6 first:mt-0">
                                {children}
                              </h1>
                            ),
                            h2: ({ children }) => (
                              <h2 className="text-lg font-semibold mb-2 mt-5 first:mt-0">
                                {children}
                              </h2>
                            ),
                            h3: ({ children }) => (
                              <h3 className="text-base font-medium mb-2 mt-4 first:mt-0">
                                {children}
                              </h3>
                            ),
                            table: ({ children }) => (
                              <div className="overflow-x-auto my-4">
                                <table className="min-w-full border-collapse border border-gray-300">
                                  {children}
                                </table>
                              </div>
                            ),
                            thead: ({ children }) => (
                              <thead className="bg-gray-50">{children}</thead>
                            ),
                            tbody: ({ children }) => <tbody>{children}</tbody>,
                            tr: ({ children }) => (
                              <tr className="border-b border-gray-200">
                                {children}
                              </tr>
                            ),
                            th: ({ children }) => (
                              <th className="border border-gray-300 px-4 py-2 text-left font-semibold">
                                {children}
                              </th>
                            ),
                            td: ({ children }) => (
                              <td className="border border-gray-300 px-4 py-2">
                                {children}
                              </td>
                            ),
                          }}
                        >
                          {message.content}
                        </ReactMarkdown>
                      </div>

                      {/* Action buttons below AI responses */}
                      {message.role !== "system" && (
                        <div className="flex items-center gap-2 mt-3">
                          <button
                            onClick={() =>
                              onCopyMessage(message.content, index)
                            }
                            className="flex items-center gap-1 px-2 py-1 text-xs text-gray-500 hover:text-gray-700 hover:bg-gray-100 rounded transition-colors"
                            aria-label="Copy message"
                          >
                            <Copy className="w-3 h-3" />
                            {copiedMessageIndex === index ? "Copied!" : "Copy"}
                          </button>
                        </div>
                      )}
                    </div>
                  )}
                </div>
              ))}

              {/* Typing Indicator */}
              {isStreaming && !streamingContent && (
                <div className="w-full">
                  <div className="flex items-center gap-2 mb-3">
                    <span className="text-sm font-medium text-gray-600">
                      AI
                    </span>
                    <div className="flex space-x-1">
                      <div className="w-2 h-2 bg-gray-400 rounded-full animate-bounce"></div>
                      <div
                        className="w-2 h-2 bg-gray-400 rounded-full animate-bounce"
                        style={{ animationDelay: "0.1s" }}
                      ></div>
                      <div
                        className="w-2 h-2 bg-gray-400 rounded-full animate-bounce"
                        style={{ animationDelay: "0.2s" }}
                      ></div>
                    </div>
                  </div>
                </div>
              )}

              {/* Streaming message */}
              {streamingContent && (
                <div className="w-full">
                  <div className="flex items-center gap-2 mb-3">
                    <span className="text-sm font-medium text-gray-600">
                      AI
                    </span>
                    <div className="flex space-x-1">
                      <div className="w-1 h-1 bg-gray-600 rounded-full animate-pulse"></div>
                      <div
                        className="w-1 h-1 bg-gray-600 rounded-full animate-pulse"
                        style={{ animationDelay: "0.2s" }}
                      ></div>
                      <div
                        className="w-1 h-1 bg-gray-600 rounded-full animate-pulse"
                        style={{ animationDelay: "0.4s" }}
                      ></div>
                    </div>
                  </div>

                  <div className="text-sm leading-relaxed prose prose-sm max-w-none">
                    <ReactMarkdown
                      remarkPlugins={[remarkGfm]}
                      components={{
                        p: ({ children }) => (
                          <p className="mb-4 last:mb-0">{children}</p>
                        ),
                        code: ({ children, className }) => {
                          const match = /language-(\w+)/.exec(className || "");
                          const language = match ? match[1] : "";
                          const codeString = String(children).replace(
                            /\n$/,
                            "",
                          );

                          // Don't syntax highlight markdown blocks - just show them as plain code
                          if (className && language === "markdown") {
                            return (
                              <div className="relative group">
                                <pre className="bg-gray-900 text-gray-100 p-4 rounded-lg overflow-x-auto text-sm my-4">
                                  <code>{children}</code>
                                </pre>
                                <button
                                  onClick={() => copyCode(codeString)}
                                  className="absolute top-2 right-2 p-2 bg-gray-700 hover:bg-gray-600 rounded transition-all duration-200 active:scale-95"
                                  title="Copy code"
                                >
                                  <Copy className="w-4 h-4 text-gray-300" />
                                  {copiedCode === codeString && (
                                    <span className="absolute -top-8 right-0 bg-gray-800 text-white text-xs px-2 py-1 rounded">
                                      Copied!
                                    </span>
                                  )}
                                </button>
                              </div>
                            );
                          }

                          return className ? (
                            <div className="relative group">
                              <SyntaxHighlighter
                                style={oneDark}
                                language={language}
                                PreTag="div"
                                className="my-4 text-sm rounded-lg"
                              >
                                {codeString}
                              </SyntaxHighlighter>
                              <button
                                onClick={() => copyCode(codeString)}
                                className="absolute top-2 right-2 p-2 bg-gray-700 hover:bg-gray-600 rounded transition-all duration-200 active:scale-95"
                                title="Copy code"
                              >
                                <Copy className="w-4 h-4 text-gray-300" />
                                {copiedCode === codeString && (
                                  <span className="absolute -top-8 right-0 bg-gray-800 text-white text-xs px-2 py-1 rounded">
                                    Copied!
                                  </span>
                                )}
                              </button>
                            </div>
                          ) : (
                            <code className="bg-gray-100 text-gray-800 px-2 py-1 rounded text-sm font-mono">
                              {children}
                            </code>
                          );
                        },
                        ul: ({ children }) => (
                          <ul className="list-disc list-inside mb-4 space-y-1">
                            {children}
                          </ul>
                        ),
                        ol: ({ children }) => (
                          <ol className="list-decimal list-inside mb-4 space-y-1">
                            {children}
                          </ol>
                        ),
                        li: ({ children }) => <li className="">{children}</li>,
                        h1: ({ children }) => (
                          <h1 className="text-xl font-bold mb-3 mt-6 first:mt-0">
                            {children}
                          </h1>
                        ),
                        h2: ({ children }) => (
                          <h2 className="text-lg font-semibold mb-2 mt-5 first:mt-0">
                            {children}
                          </h2>
                        ),
                        h3: ({ children }) => (
                          <h3 className="text-base font-medium mb-2 mt-4 first:mt-0">
                            {children}
                          </h3>
                        ),
                        table: ({ children }) => (
                          <div className="overflow-x-auto my-4">
                            <table className="min-w-full border-collapse border border-gray-300">
                              {children}
                            </table>
                          </div>
                        ),
                        thead: ({ children }) => (
                          <thead className="bg-gray-50">{children}</thead>
                        ),
                        tbody: ({ children }) => <tbody>{children}</tbody>,
                        tr: ({ children }) => (
                          <tr className="border-b border-gray-200">
                            {children}
                          </tr>
                        ),
                        th: ({ children }) => (
                          <th className="border border-gray-300 px-4 py-2 text-left font-semibold">
                            {children}
                          </th>
                        ),
                        td: ({ children }) => (
                          <td className="border border-gray-300 px-4 py-2">
                            {children}
                          </td>
                        ),
                      }}
                    >
                      {streamingContent}
                    </ReactMarkdown>
                  </div>
                </div>
              )}

              {error && (
                <div className="bg-red-50 border border-red-200 text-red-700 rounded-lg p-4 w-full">
                  <p className="font-medium text-sm">Error</p>
                  <p className="text-sm">{error}</p>
                </div>
              )}
            </div>
            <div ref={messagesEndRef} />
          </div>
        )}
      </div>

      {/* Input Area */}
      <div className="bg-white border-t border-gray-200 px-8 py-6 flex-shrink-0">
        <div className="max-w-4xl mx-auto">
          <div className="flex-1 relative">
            <Textarea
              ref={textareaRef}
              value={currentMessage}
              onChange={(e) => onCurrentMessageChange(e.target.value)}
              onKeyDown={onKeyDown}
              placeholder="Type your message..."
              className="pr-12 text-sm"
              rows={3}
              disabled={isStreaming}
              aria-label="Message input"
            />
            <Button
              onClick={
                isStreaming
                  ? isHovered
                    ? onCancelStreaming
                    : undefined
                  : onSendMessage
              }
              onMouseEnter={() => setIsHovered(true)}
              onMouseLeave={() => setIsHovered(false)}
              disabled={!isStreaming && !currentMessage.trim()}
              size="icon"
              className="absolute top-3 right-3 h-8 w-8 focus:outline-none focus:ring-0"
              aria-label={
                isStreaming
                  ? isHovered
                    ? "Cancel message"
                    : "Streaming..."
                  : "Send message"
              }
              title={
                isStreaming ? (isHovered ? "Cancel" : "Streaming...") : "Send"
              }
            >
              {isStreaming ? (
                isHovered && onCancelStreaming ? (
                  <X className="w-4 h-4" />
                ) : (
                  <div className="relative w-4 h-4">
                    <div className="absolute inset-0 rounded-full border-2 border-white opacity-20"></div>
                    <div className="absolute inset-0 rounded-full border-2 border-transparent border-t-white animate-spin"></div>
                  </div>
                )
              ) : (
                <Send className="w-4 h-4 -ml-0.5 mt-0.5" />
              )}
            </Button>
          </div>
          <div className="flex items-center justify-between mt-3">
            <div className="text-sm text-gray-400">
              Enter to send • Shift+Enter for newline • Esc to cancel
            </div>
            <Button
              onClick={onClearConversation}
              variant="outline"
              size="sm"
              disabled={messages.length === 0 && !streamingContent}
              aria-label="Clear conversation"
            >
              <Trash2 className="w-4 h-4" />
              Clear chat
            </Button>
          </div>
        </div>
      </div>
    </div>
  );
};

export default GenerationPlayground;
