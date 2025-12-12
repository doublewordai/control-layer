import React, { useRef, useEffect, useState, useCallback } from "react";
import {
  Send,
  Copy,
  Play,
  Trash2,
  X,
  Image as ImageIcon,
  SplitSquareHorizontal,
  Square,
  ArrowLeft,
  ArrowRight,
  Timer,
  Zap,
  ArrowDown,
  ArrowUp,
  ChevronDown,
  Settings,
} from "lucide-react";
import { toast } from "sonner";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { CodeBlock } from "../../../ui/code-block";
import type { Model } from "../../../../api/control-layer/types";
import { Textarea } from "../../../ui/textarea";
import { Button } from "../../../ui/button";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "../../../ui/tooltip";
import { AlertBox } from "@/components/ui/alert-box";

interface ImageContent {
  type: "image_url";
  image_url: {
    url: string;
  };
}

interface TextContent {
  type: "text";
  text: string;
}

type MessageContent = string | (TextContent | ImageContent)[];

interface MessageMetrics {
  timeToFirstToken?: number; // milliseconds
  totalTime?: number; // milliseconds
  tokensPerSecond?: number;
  totalTokens?: number;
  inputTokens?: number;
}

interface Message {
  role: "user" | "assistant" | "system";
  content: MessageContent;
  timestamp: Date;
  metrics?: MessageMetrics;
}

interface GenerationPlaygroundProps {
  selectedModel: Model;
  messages: Message[];
  currentMessage: string;
  uploadedImages: string[];
  streamingContent: string;
  isStreaming: boolean;
  error: string | null;
  copiedMessageIndex: number | null;
  supportsImages: boolean;
  systemPrompt: string;
  onSystemPromptChange: (value: string) => void;
  systemPromptModelB?: string;
  onSystemPromptModelBChange?: (value: string) => void;
  onCurrentMessageChange: (value: string) => void;
  onImageUpload: (event: React.ChangeEvent<HTMLInputElement>) => void;
  onRemoveImage: (index: number) => void;
  onSendMessage: () => void;
  onCopyMessage: (content: string, index: number) => void;
  onKeyDown: (e: React.KeyboardEvent) => void;
  onClearConversation: () => void;
  onCancelStreaming?: () => void;
  // Comparison mode props
  isComparisonMode?: boolean;
  comparisonModel?: Model | null;
  messagesModelB?: Message[];
  streamingContentModelB?: string;
  isStreamingModelB?: boolean;
  isSplitInput?: boolean;
  currentMessageModelB?: string;
  onCurrentMessageModelBChange?: (value: string) => void;
  onToggleSplitInput?: () => void;
  onSendMessageModelB?: () => void;
  onCopyMessagesToModelB?: () => void;
  onCopyMessagesToModelA?: () => void;
}

const GenerationPlayground: React.FC<GenerationPlaygroundProps> = ({
  selectedModel,
  messages,
  currentMessage,
  uploadedImages,
  streamingContent,
  isStreaming,
  error,
  copiedMessageIndex,
  supportsImages,
  systemPrompt,
  onSystemPromptChange,
  systemPromptModelB = "",
  onSystemPromptModelBChange,
  onCurrentMessageChange,
  onImageUpload,
  onRemoveImage,
  onSendMessage,
  onCopyMessage,
  onKeyDown,
  onClearConversation,
  onCancelStreaming,
  // Comparison mode props
  isComparisonMode = false,
  comparisonModel = null,
  messagesModelB = [],
  streamingContentModelB = "",
  isStreamingModelB = false,
  isSplitInput = false,
  currentMessageModelB = "",
  onCurrentMessageModelBChange,
  onToggleSplitInput,
  onSendMessageModelB,
  onCopyMessagesToModelB,
  onCopyMessagesToModelA,
}) => {
  const messagesEndRef = useRef<HTMLDivElement>(null);
  const messagesEndRefModelB = useRef<HTMLDivElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const textareaRefModelB = useRef<HTMLTextAreaElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const [isHovered, setIsHovered] = useState(false);
  const [copiedCode, setCopiedCode] = useState<string | null>(null);
  const [isSystemPromptExpanded, setIsSystemPromptExpanded] = useState(false);

  const scrollToBottom = useCallback(() => {
    messagesEndRef.current?.scrollIntoView({ behavior: "smooth" });
    if (isComparisonMode) {
      messagesEndRefModelB.current?.scrollIntoView({ behavior: "smooth" });
    }
  }, [isComparisonMode]);

  const copyCode = async (code: string) => {
    try {
      await navigator.clipboard.writeText(code);
      setCopiedCode(code);
      toast.success("Code copied to clipboard");
      setTimeout(() => setCopiedCode(null), 2000);
    } catch (err) {
      console.error("Failed to copy to clipboard:", err);
      toast.error("Failed to copy code");
    }
  };

  const getTextContent = (content: MessageContent): string => {
    if (typeof content === "string") {
      return content;
    }
    // Extract text from multimodal content
    const textPart = content.find((part) => part.type === "text") as
      | TextContent
      | undefined;
    return textPart?.text || "";
  };

  const getImages = (content: MessageContent): string[] => {
    if (typeof content === "string") {
      return [];
    }
    return content
      .filter((part) => part.type === "image_url")
      .map((part) => (part as ImageContent).image_url.url);
  };

  useEffect(() => {
    scrollToBottom();
  }, [
    messages,
    streamingContent,
    messagesModelB,
    streamingContentModelB,
    scrollToBottom,
  ]);

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

  // Helper function to render messages area for a model
  const renderMessagesArea = (
    model: Model,
    modelMessages: Message[],
    modelStreamingContent: string,
    modelIsStreaming: boolean,
    messagesEndReference: React.RefObject<HTMLDivElement | null>,
    modelLabel: string,
  ) => (
    <div className="flex-1 overflow-y-auto px-8 py-4 bg-white scrollbar-thin scrollbar-thumb-gray-300 scrollbar-track-transparent hover:scrollbar-thumb-gray-400">
      {modelMessages.length === 0 && !modelStreamingContent ? (
        <div className="flex items-center justify-center h-full">
          <div
            className="text-center"
            role="status"
            aria-label="Empty conversation"
          >
            <Play className="w-16 h-16 text-gray-400 mx-auto mb-4" />
            <p className="text-xl text-gray-600 mb-2">{model.alias}</p>
            <p className="text-gray-500">{modelLabel}</p>
          </div>
        </div>
      ) : (
        <div className={isComparisonMode ? "px-4" : "max-w-4xl mx-auto px-4"}>
          <div className="space-y-6">
            {modelMessages.map((message, index) => (
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
                      {/* Display images if present */}
                      {getImages(message.content).length > 0 && (
                        <div className="mb-3 flex flex-wrap gap-2">
                          {getImages(message.content).map(
                            (imageUrl, imgIndex) => (
                              <img
                                key={imgIndex}
                                src={imageUrl}
                                alt={`Uploaded image ${imgIndex + 1}`}
                                className="max-w-full max-h-64 rounded-lg object-contain"
                              />
                            ),
                          )}
                        </div>
                      )}
                      <div className="text-sm whitespace-pre-wrap leading-relaxed">
                        {getTextContent(message.content)}
                      </div>
                    </div>
                  </div>
                ) : (
                  /* AI/System Message - Full Width Document Style */
                  <div className="w-full">
                    <div className="flex items-center gap-2 mb-3">
                      <span className="text-sm font-medium text-gray-600">
                        {message.role === "system" ? "System" : model.alias}
                      </span>
                      <span className="text-xs text-gray-400">
                        {message.timestamp.toLocaleTimeString()}
                        {message.metrics?.totalTime !== undefined && (
                          <span className="ml-1">
                            ({(message.metrics.totalTime / 1000).toFixed(2)}s)
                          </span>
                        )}
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
                              <div className="relative group my-4">
                                <CodeBlock
                                  language={
                                    language as
                                      | "python"
                                      | "javascript"
                                      | "bash"
                                      | "json"
                                  }
                                >
                                  {codeString}
                                </CodeBlock>
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
                        {getTextContent(message.content)}
                      </ReactMarkdown>
                    </div>

                    {/* Action buttons below AI responses */}
                    {message.role !== "system" && (
                      <div className="flex items-center gap-3 mt-3">
                        <button
                          onClick={() =>
                            onCopyMessage(
                              getTextContent(message.content),
                              index,
                            )
                          }
                          className="flex items-center gap-1 px-2 py-1 text-xs text-gray-500 hover:text-gray-700 hover:bg-gray-100 rounded transition-colors"
                          aria-label="Copy message"
                        >
                          <Copy className="w-3 h-3" />
                          {copiedMessageIndex === index ? "Copied!" : "Copy"}
                        </button>

                        {/* Metrics */}
                        {message.metrics && (
                          <TooltipProvider>
                            <div className="flex items-center gap-2 text-xs text-gray-400">
                              {message.metrics.timeToFirstToken !==
                                undefined && (
                                <Tooltip>
                                  <TooltipTrigger asChild>
                                    <span className="flex items-center gap-0.5">
                                      <Timer className="w-3 h-3" />
                                      {Math.round(
                                        message.metrics.timeToFirstToken,
                                      )}
                                      ms
                                    </span>
                                  </TooltipTrigger>
                                  <TooltipContent>
                                    <p>Time to first token</p>
                                  </TooltipContent>
                                </Tooltip>
                              )}
                              {message.metrics.tokensPerSecond !==
                                undefined && (
                                <Tooltip>
                                  <TooltipTrigger asChild>
                                    <span className="flex items-center gap-0.5">
                                      <Zap className="w-3 h-3" />
                                      {message.metrics.tokensPerSecond.toFixed(
                                        1,
                                      )}
                                      /s
                                    </span>
                                  </TooltipTrigger>
                                  <TooltipContent>
                                    <p>Tokens per second</p>
                                  </TooltipContent>
                                </Tooltip>
                              )}
                              {message.metrics.inputTokens !== undefined && (
                                <Tooltip>
                                  <TooltipTrigger asChild>
                                    <span className="flex items-center gap-0.5">
                                      <ArrowUp className="w-3 h-3" />
                                      {message.metrics.inputTokens}
                                    </span>
                                  </TooltipTrigger>
                                  <TooltipContent>
                                    <p>Input tokens</p>
                                  </TooltipContent>
                                </Tooltip>
                              )}
                              {message.metrics.totalTokens !== undefined && (
                                <Tooltip>
                                  <TooltipTrigger asChild>
                                    <span className="flex items-center gap-0.5">
                                      <ArrowDown className="w-3 h-3" />
                                      {message.metrics.totalTokens}
                                    </span>
                                  </TooltipTrigger>
                                  <TooltipContent>
                                    <p>Output tokens</p>
                                  </TooltipContent>
                                </Tooltip>
                              )}
                            </div>
                          </TooltipProvider>
                        )}
                      </div>
                    )}
                  </div>
                )}
              </div>
            ))}

            {/* Typing Indicator */}
            {modelIsStreaming && !modelStreamingContent && (
              <div className="w-full">
                <div className="flex items-center gap-2 mb-3">
                  <span className="text-sm font-medium text-gray-600">
                    {model.alias}
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
            {modelStreamingContent && (
              <div className="w-full">
                <div className="flex items-center gap-2 mb-3">
                  <span className="text-sm font-medium text-gray-600">
                    {model.alias}
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
                        const codeString = String(children).replace(/\n$/, "");

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
                          <div className="relative group my-4">
                            <CodeBlock
                              language={
                                language as
                                  | "python"
                                  | "javascript"
                                  | "bash"
                                  | "json"
                              }
                            >
                              {codeString}
                            </CodeBlock>
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
                        <tr className="border-b border-gray-200">{children}</tr>
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
                    {modelStreamingContent}
                  </ReactMarkdown>
                </div>
              </div>
            )}

            <AlertBox variant="error">{error}</AlertBox>
          </div>
          <div ref={messagesEndReference} />
        </div>
      )}
    </div>
  );

  return (
    <div className="flex-1 flex flex-col min-h-0 relative">
      {/* System Prompt Tab - Pokes out from top */}
      <div className="absolute top-0 left-1/2 -translate-x-1/2 z-30">
        <button
          onClick={() => setIsSystemPromptExpanded(!isSystemPromptExpanded)}
          className="flex items-center gap-1.5 px-2.5 py-1 bg-gray-100 hover:bg-gray-200 border border-gray-300 border-t-0 rounded-b-md transition-colors shadow-sm relative"
          aria-expanded={isSystemPromptExpanded}
          aria-label="Toggle system prompt"
          title={
            isSystemPromptExpanded ? "Hide system prompt" : "Show system prompt"
          }
        >
          <Settings className="w-3.5 h-3.5 text-gray-600" />
          {(systemPrompt.trim() || systemPromptModelB.trim()) &&
            !isSystemPromptExpanded && (
              <span
                className="absolute -top-0.5 -right-0.5 w-2 h-2 bg-blue-500 rounded-full border border-white"
                aria-label="System prompt is active"
              />
            )}
          <ChevronDown
            className={`w-3.5 h-3.5 text-gray-600 transition-transform ${
              isSystemPromptExpanded ? "rotate-180" : ""
            }`}
          />
        </button>
      </div>

      {/* System Prompt Expandable Section */}
      {isSystemPromptExpanded && (
        <div className="absolute top-0 left-0 right-0 bg-white border-b border-gray-300 shadow-md z-20 animate-in slide-in-from-top duration-200">
          <div className="px-8 pt-8 pb-4">
            {isComparisonMode && comparisonModel ? (
              /* Side-by-side system prompts for comparison mode */
              <div className="flex gap-4 relative">
                <div className="flex-1">
                  <label className="block text-xs font-medium text-gray-700 mb-2">
                    {selectedModel.alias}
                  </label>
                  <Textarea
                    value={systemPrompt}
                    onChange={(e) => onSystemPromptChange(e.target.value)}
                    placeholder="System prompt for Model A..."
                    className="text-sm min-h-[100px] resize-y"
                    disabled={isStreaming}
                    aria-label="System prompt for Model A"
                  />
                </div>

                {/* Copy arrows between system prompts */}
                <div className="absolute left-1/2 top-1/2 -translate-x-1/2 -translate-y-1/2 flex flex-col gap-2 z-10">
                  {/* Copy to Model B (right arrow) */}
                  {systemPrompt.trim() && (
                    <Button
                      onClick={() =>
                        onSystemPromptModelBChange &&
                        onSystemPromptModelBChange(systemPrompt)
                      }
                      size="icon"
                      variant="outline"
                      className="h-7 w-7 bg-white shadow-md hover:bg-gray-50"
                      aria-label="Copy system prompt to right model"
                      title={`Copy system prompt to ${comparisonModel.alias}`}
                    >
                      <ArrowRight className="w-3.5 h-3.5" />
                    </Button>
                  )}
                  {/* Copy to Model A (left arrow) */}
                  {systemPromptModelB.trim() && (
                    <Button
                      onClick={() => onSystemPromptChange(systemPromptModelB)}
                      size="icon"
                      variant="outline"
                      className="h-7 w-7 bg-white shadow-md hover:bg-gray-50"
                      aria-label="Copy system prompt to left model"
                      title={`Copy system prompt to ${selectedModel.alias}`}
                    >
                      <ArrowLeft className="w-3.5 h-3.5" />
                    </Button>
                  )}
                </div>

                <div className="flex-1">
                  <label className="block text-xs font-medium text-gray-700 mb-2">
                    {comparisonModel.alias}
                  </label>
                  <Textarea
                    value={systemPromptModelB}
                    onChange={(e) =>
                      onSystemPromptModelBChange &&
                      onSystemPromptModelBChange(e.target.value)
                    }
                    placeholder="System prompt for Model B..."
                    className="text-sm min-h-[100px] resize-y"
                    disabled={isStreamingModelB}
                    aria-label="System prompt for Model B"
                  />
                </div>
              </div>
            ) : (
              /* Single system prompt for regular mode */
              <Textarea
                value={systemPrompt}
                onChange={(e) => onSystemPromptChange(e.target.value)}
                placeholder="Enter a system prompt to set the behavior and context for the AI model... (e.g., 'You are a helpful assistant that speaks like a pirate.')"
                className="text-sm min-h-[100px] resize-y"
                disabled={isStreaming}
                aria-label="System prompt input"
              />
            )}
            <p className="text-xs text-gray-500 mt-2">
              The system prompt sets the behavior and context for the AI model.
              It will be sent with every message in this conversation.
            </p>
          </div>
        </div>
      )}

      {/* Messages */}
      {isComparisonMode && comparisonModel ? (
        <div className="flex-1 flex relative overflow-hidden min-h-0">
          {/* Centered Empty State - Only show when both models have no messages */}
          {messages.length === 0 &&
            !streamingContent &&
            messagesModelB.length === 0 &&
            !streamingContentModelB && (
              <div className="absolute inset-0 flex items-center justify-center pointer-events-none z-10">
                <div className="text-center bg-white px-8 py-4 rounded-lg">
                  <Play className="w-16 h-16 text-gray-400 mx-auto mb-4" />
                  <p className="text-xl text-gray-600 mb-2">
                    Compare {selectedModel.alias} and {comparisonModel.alias}
                  </p>
                  <p className="text-gray-500">
                    Send a message to start a conversation
                  </p>
                </div>
              </div>
            )}

          {/* Sync Arrows - Center of divider */}
          <div className="absolute left-1/2 top-1/2 -translate-x-1/2 -translate-y-1/2 z-20 flex flex-col gap-2">
            {/* Copy to Model B (right arrow) */}
            {onCopyMessagesToModelB && messages.length > 0 && (
              <Button
                onClick={onCopyMessagesToModelB}
                size="icon"
                variant="outline"
                className="h-8 w-8 bg-white shadow-md hover:bg-gray-50"
                aria-label="Copy conversation to right model"
                title={`Copy ${selectedModel.alias} conversation to ${comparisonModel.alias}`}
              >
                <ArrowRight className="w-4 h-4" />
              </Button>
            )}
            {/* Copy to Model A (left arrow) */}
            {onCopyMessagesToModelA && messagesModelB.length > 0 && (
              <Button
                onClick={onCopyMessagesToModelA}
                size="icon"
                variant="outline"
                className="h-8 w-8 bg-white shadow-md hover:bg-gray-50"
                aria-label="Copy conversation to left model"
                title={`Copy ${comparisonModel.alias} conversation to ${selectedModel.alias}`}
              >
                <ArrowLeft className="w-4 h-4" />
              </Button>
            )}
          </div>

          {/* Model A */}
          <div className="flex-1 border-r border-gray-200 flex flex-col min-h-0">
            {messages.length > 0 || streamingContent ? (
              renderMessagesArea(
                selectedModel,
                messages,
                streamingContent,
                isStreaming,
                messagesEndRef,
                "Model A",
              )
            ) : (
              <div className="flex-1 overflow-y-auto px-8 py-4 bg-white" />
            )}
          </div>
          {/* Model B */}
          <div className="flex-1 flex flex-col min-h-0">
            {messagesModelB.length > 0 || streamingContentModelB ? (
              renderMessagesArea(
                comparisonModel,
                messagesModelB,
                streamingContentModelB,
                isStreamingModelB,
                messagesEndRefModelB,
                "Model B",
              )
            ) : (
              <div className="flex-1 overflow-y-auto px-8 py-4 bg-white" />
            )}
          </div>
        </div>
      ) : (
        renderMessagesArea(
          selectedModel,
          messages,
          streamingContent,
          isStreaming,
          messagesEndRef,
          "Send a message to start a conversation",
        )
      )}

      {/* Input Area */}
      <div className="bg-white border-t border-gray-200 px-8 py-4 shrink-0">
        <div className="max-w-4xl mx-auto">
          {/* Image Preview */}
          {uploadedImages.length > 0 && (
            <div className="mb-3 flex flex-wrap gap-2">
              {uploadedImages.map((imageUrl, index) => (
                <div key={index} className="relative group">
                  <img
                    src={imageUrl}
                    alt={`Upload preview ${index + 1}`}
                    className="h-20 w-20 object-cover rounded-lg border border-gray-200"
                  />
                  <button
                    onClick={() => onRemoveImage(index)}
                    className="absolute -top-2 -right-2 bg-red-500 text-white rounded-full p-1 opacity-0 group-hover:opacity-100 transition-opacity"
                    aria-label="Remove image"
                  >
                    <X className="w-3 h-3" />
                  </button>
                </div>
              ))}
            </div>
          )}

          {/* Input Area - Split or Unified */}
          {isComparisonMode && isSplitInput ? (
            /* Split Input Boxes */
            <div className="flex gap-4">
              {/* Model A Input */}
              <div className="flex-1 relative">
                <div className="mb-2 text-xs font-medium text-gray-600">
                  {selectedModel.alias}
                </div>
                <Textarea
                  ref={textareaRef}
                  value={currentMessage}
                  onChange={(e) => onCurrentMessageChange(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter" && !e.shiftKey) {
                      e.preventDefault();
                      onSendMessage();
                    }
                  }}
                  placeholder="Type your message..."
                  className="pr-12 text-sm"
                  rows={3}
                  disabled={isStreaming}
                  aria-label="Message input for Model A"
                />
                <div className="absolute top-10 right-3">
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
                    disabled={
                      !isStreaming &&
                      !currentMessage.trim() &&
                      uploadedImages.length === 0
                    }
                    size="icon"
                    className="h-8 w-8 focus:outline-none focus:ring-0"
                    aria-label={
                      isStreaming
                        ? isHovered
                          ? "Cancel message"
                          : "Streaming..."
                        : "Send message"
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
              </div>

              {/* Model B Input */}
              <div className="flex-1 relative">
                <div className="mb-2 text-xs font-medium text-gray-600">
                  {comparisonModel?.alias}
                </div>
                <Textarea
                  ref={textareaRefModelB}
                  value={currentMessageModelB}
                  onChange={(e) =>
                    onCurrentMessageModelBChange &&
                    onCurrentMessageModelBChange(e.target.value)
                  }
                  onKeyDown={(e) => {
                    if (e.key === "Enter" && !e.shiftKey) {
                      e.preventDefault();
                      if (onSendMessageModelB) {
                        onSendMessageModelB();
                      }
                    }
                  }}
                  placeholder="Type your message..."
                  className="pr-12 text-sm"
                  rows={3}
                  disabled={isStreamingModelB}
                  aria-label="Message input for Model B"
                />
                <div className="absolute top-10 right-3">
                  <Button
                    onClick={onSendMessageModelB}
                    disabled={!currentMessageModelB.trim() || isStreamingModelB}
                    size="icon"
                    className="h-8 w-8 focus:outline-none focus:ring-0"
                    aria-label="Send message to Model B"
                  >
                    {isStreamingModelB ? (
                      <div className="relative w-4 h-4">
                        <div className="absolute inset-0 rounded-full border-2 border-white opacity-20"></div>
                        <div className="absolute inset-0 rounded-full border-2 border-transparent border-t-white animate-spin"></div>
                      </div>
                    ) : (
                      <Send className="w-4 h-4 -ml-0.5 mt-0.5" />
                    )}
                  </Button>
                </div>
              </div>
            </div>
          ) : (
            /* Unified Input Box */
            <div className="flex-1 relative">
              <Textarea
                ref={textareaRef}
                value={currentMessage}
                onChange={(e) => onCurrentMessageChange(e.target.value)}
                onKeyDown={onKeyDown}
                placeholder="Type your message..."
                className="pr-24 text-sm"
                rows={3}
                disabled={isStreaming}
                aria-label="Message input"
              />
              <div className="absolute top-3 right-3 flex gap-1">
                {/* Send Button */}
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
                  disabled={
                    !isStreaming &&
                    !currentMessage.trim() &&
                    uploadedImages.length === 0
                  }
                  size="icon"
                  className="h-8 w-8 focus:outline-none focus:ring-0"
                  aria-label={
                    isStreaming
                      ? isHovered
                        ? "Cancel message"
                        : "Streaming..."
                      : "Send message"
                  }
                  title={
                    isStreaming
                      ? isHovered
                        ? "Cancel"
                        : "Streaming..."
                      : "Send"
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
            </div>
          )}

          <div className="flex items-center justify-between mt-3">
            <div className="text-sm text-gray-400">
              Enter to send • Shift+Enter for newline • Esc to cancel
            </div>
            <div className="flex items-center gap-2">
              {/* Image Upload Button - only show if model supports images */}
              {supportsImages && !isComparisonMode && (
                <>
                  <input
                    ref={fileInputRef}
                    type="file"
                    accept="image/*"
                    multiple
                    onChange={onImageUpload}
                    className="hidden"
                    aria-label="Upload images"
                  />
                  <Button
                    onClick={() => fileInputRef.current?.click()}
                    disabled={isStreaming}
                    variant="outline"
                    size="sm"
                    aria-label="Upload image"
                    title="Upload image"
                  >
                    <ImageIcon className="w-4 h-4 mr-1" />
                    Upload image
                  </Button>
                </>
              )}
              {/* Split/Merge Input Toggle - Only show in comparison mode */}
              {isComparisonMode && onToggleSplitInput && (
                <Button
                  onClick={onToggleSplitInput}
                  variant="outline"
                  size="sm"
                  aria-label={
                    isSplitInput ? "Merge input boxes" : "Split input boxes"
                  }
                  title={
                    isSplitInput ? "Merge input boxes" : "Split input boxes"
                  }
                >
                  {isSplitInput ? (
                    <>
                      <Square className="w-4 h-4 mr-1" />
                      Merge inputs
                    </>
                  ) : (
                    <>
                      <SplitSquareHorizontal className="w-4 h-4 mr-1" />
                      Split inputs
                    </>
                  )}
                </Button>
              )}
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
    </div>
  );
};

export default GenerationPlayground;
