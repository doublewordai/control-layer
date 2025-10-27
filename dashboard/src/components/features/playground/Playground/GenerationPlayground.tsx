import React, { useRef, useEffect, useState } from "react";
import { Send, Copy, Play, X, Image as ImageIcon } from "lucide-react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { Prism as SyntaxHighlighter } from "react-syntax-highlighter";
import { oneDark } from "react-syntax-highlighter/dist/esm/styles/prism";
import type { Model } from "../../../../api/control-layer/types";
import { Textarea } from "../../../ui/textarea";
import { Button } from "../../../ui/button";

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

interface Message {
  role: "user" | "assistant" | "system";
  content: MessageContent;
  timestamp: Date;
  modelAlias: string; // Track which model generated this message (for assistant messages)
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
  onCurrentMessageChange: (value: string) => void;
  onImageUpload: (event: React.ChangeEvent<HTMLInputElement>) => void;
  onRemoveImage: (index: number) => void;
  onSendMessage: () => void;
  onCopyMessage: (content: string, index: number) => void;
  onKeyDown: (e: React.KeyboardEvent) => void;
  onCancelStreaming?: () => void;
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
  onCurrentMessageChange,
  onImageUpload,
  onRemoveImage,
  onSendMessage,
  onCopyMessage,
  onKeyDown,
  onCancelStreaming,
}) => {
  const messagesEndRef = useRef<HTMLDivElement>(null);
  const messagesContainerRef = useRef<HTMLDivElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const fileInputRef = useRef<HTMLInputElement>(null);
  const [isHovered, setIsHovered] = useState(false);
  const [copiedCode, setCopiedCode] = useState<string | null>(null);
  const [userHasScrolled, setUserHasScrolled] = useState(false);

  // Check if user is near bottom of scroll
  const isNearBottom = React.useCallback(() => {
    if (!messagesContainerRef.current) return true;
    const { scrollTop, scrollHeight, clientHeight } = messagesContainerRef.current;
    return scrollHeight - scrollTop - clientHeight < 100; // Within 100px of bottom
  }, []);

  const scrollToBottom = React.useCallback(() => {
    if (messagesContainerRef.current) {
      // Scroll within the messages container, not the whole page
      messagesContainerRef.current.scrollTop = messagesContainerRef.current.scrollHeight;
    }
  }, []);

  const copyCode = (code: string) => {
    navigator.clipboard.writeText(code);
    setCopiedCode(code);
    setTimeout(() => setCopiedCode(null), 2000);
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

  // Only auto-scroll if user is near bottom or hasn't manually scrolled
  useEffect(() => {
    if (isNearBottom() || !userHasScrolled) {
      scrollToBottom();
    }
  }, [messages, streamingContent, isNearBottom, userHasScrolled, scrollToBottom]);

  // Track user scroll behavior
  useEffect(() => {
    const container = messagesContainerRef.current;
    if (!container) return;

    const handleScroll = () => {
      setUserHasScrolled(!isNearBottom());
    };

    container.addEventListener('scroll', handleScroll);
    return () => container.removeEventListener('scroll', handleScroll);
  }, [isNearBottom]);

  // Reset scroll tracking when starting new message
  useEffect(() => {
    if (isStreaming) {
      setUserHasScrolled(false);
    }
  }, [isStreaming]);

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
    <div className="flex-1 flex flex-col min-h-0">
      {/* Messages */}
      <div ref={messagesContainerRef} className="flex-1 overflow-y-auto px-8 py-4 bg-white">
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
                          {message.role === "system"
                            ? "System"
                            : (message.modelAlias)}
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
                          {getTextContent(message.content)}
                        </ReactMarkdown>
                      </div>

                      {/* Action buttons below AI responses */}
                      {message.role !== "system" && (
                        <div className="flex items-center gap-2 mt-3">
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
                      {/* Use last message model alias, if not messages then use selected model alias */}
                      {messages[messages.length - 1]?.modelAlias || selectedModel.alias}
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
                      {/* Use last message model alias, if not messages then use selected model alias */}
                      {messages[messages.length - 1]?.modelAlias || selectedModel.alias}
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
      <div className="bg-white border-t border-gray-200 px-8 py-4 flex-shrink-0">
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

          <div className="flex-1 relative">
            <Textarea
              ref={textareaRef}
              value={currentMessage}
              onChange={(e) => onCurrentMessageChange(e.target.value)}
              onKeyDown={onKeyDown}
              placeholder="Type your message..."
              className="pr-24 text-sm max-h-40 overflow-y-auto"
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
          </div>
          <div className="flex items-center justify-between mt-3">
            <div className="text-sm text-gray-400">
              Enter to send • Shift+Enter for newline • Esc to cancel
            </div>
            <div className="flex items-center gap-2">
              {/* Image Upload Button - only show if model supports images */}
              {supportsImages && (
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
            </div>
          </div>
        </div>
      </div>
    </div>
  );
};

export default GenerationPlayground;
