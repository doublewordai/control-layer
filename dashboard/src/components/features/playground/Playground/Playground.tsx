import React, { useState, useEffect, useMemo } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { Play, ArrowLeft, GitCompare, X as XIcon } from "lucide-react";
import { toast } from "sonner";
import OpenAI from "openai";
import type { ChatCompletionMessageParam } from "openai/resources/chat/completions";
import { useModels } from "../../../../api/control-layer";
import { type ModelType } from "../../../../utils/modelType";
import type {
  Model,
  RerankResponse,
} from "../../../../api/control-layer/types";
import EmbeddingPlayground from "./EmbeddingPlayground";
import GenerationPlayground from "./GenerationPlayground";
import RerankPlayground from "./RerankPlayground";
import { Button } from "../../../ui/button";
import {
  Command,
  CommandEmpty,
  CommandGroup,
  CommandInput,
  CommandItem,
  CommandList,
} from "../../../ui/command";
import { Popover, PopoverContent, PopoverTrigger } from "../../../ui/popover";
import { ChevronsUpDownIcon } from "lucide-react";
import { useDebounce } from "@/hooks/useDebounce";

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

const Playground: React.FC = () => {
  const navigate = useNavigate();
  const [searchParams] = useSearchParams();
  const selectedModelId = searchParams.get("model");
  const fromUrl = searchParams.get("from");

  const [messages, setMessages] = useState<Message[]>([]);
  const [currentMessage, setCurrentMessage] = useState("");
  const [uploadedImages, setUploadedImages] = useState<string[]>([]);
  const [isStreaming, setIsStreaming] = useState(false);
  const [streamingContent, setStreamingContent] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [selectedModel, setSelectedModel] = useState<Model | null>(null);
  const [modelType, setModelType] = useState<ModelType>("chat");
  const [systemPrompt, setSystemPrompt] = useState("");
  const [systemPromptModelB, setSystemPromptModelB] = useState("");
  const [textA, setTextA] = useState("");
  const [textB, setTextB] = useState("");
  const [similarityResult, setSimilarityResult] = useState<{
    score: number;
    category: string;
  } | null>(null);

  // Reranker state
  const [query, setQuery] = useState("What is the capital of France?");
  const [documents, setDocuments] = useState<string[]>([
    "The capital of Brazil is Brasilia.",
    "The capital of France is Paris.",
    "Horses and cows are both animals",
  ]);
  const [rerankResult, setRerankResult] = useState<RerankResponse | null>(null);

  const [modelSearchQuery, setModelSearchQuery] = useState("");
  const [modelSelectOpen, setModelSelectOpen] = useState(false);
  const debouncedModelSearch = useDebounce(modelSearchQuery, 300);

  const [compareSearchQuery, setCompareSearchQuery] = useState("");
  const [compareSelectOpen, setCompareSelectOpen] = useState(false);
  const debouncedCompareSearch = useDebounce(compareSearchQuery, 300);

  const { data: modelsData, error: modelsError } = useModels({
    search: debouncedModelSearch || undefined,
    limit: 50,
  });
  const models = useMemo(() => modelsData?.data ?? [], [modelsData]);

  const { data: compareModelsData } = useModels({
    search: debouncedCompareSearch || undefined,
    limit: 50,
  });
  const compareModels = useMemo(() => {
    const data = compareModelsData?.data ?? [];
    return data.filter(
      (model) =>
        model.alias !== selectedModel?.alias &&
        (model.model_type?.toLowerCase() as ModelType) === "chat",
    );
  }, [compareModelsData, selectedModel?.alias]);

  // Initialize OpenAI client pointing to our API
  const baseURL = `${window.location.origin}/admin/api/v1/ai/v1`;

  const openai = new OpenAI({
    baseURL,
    apiKey: "", // SDK requires this but we override the header below
    dangerouslyAllowBrowser: true,
    defaultHeaders: {
      // Remove Authorization header so proxy can transform session cookies
      Authorization: null as any,
    },
  });

  // Convert models data to array and handle URL model selection
  useEffect(() => {
    if (models && models.length > 0) {
      // If a model ID is specified in URL, select it
      if (selectedModelId) {
        const model = models.find((m) => m.alias === selectedModelId);
        if (model) {
          setSelectedModel(model);
          setModelType(
            (model.model_type?.toLowerCase() as ModelType) || "chat",
          );
        }
      }
    }
  }, [models, selectedModelId]);

  // Handle models loading error
  useEffect(() => {
    if (modelsError) {
      console.error("Error loading models:", modelsError);
      setError("Failed to load models");
    }
  }, [modelsError]);

  // Reset state when switching models
  useEffect(() => {
    if (selectedModel) {
      setMessages([]);
      setStreamingContent("");
      setSimilarityResult(null);
      setRerankResult(null);
      setError(null);
      setCurrentMessage("");
      setUploadedImages([]);
      setSystemPrompt("");
      setSystemPromptModelB("");
      setTextA("");
      setTextB("");
      setQuery("What is the capital of France?");
      setDocuments([
        "The capital of Brazil is Brasilia.",
        "The capital of France is Paris.",
        "Horses and cows are both animals",
      ]);
      // Reset comparison mode when switching primary model
      setIsComparisonMode(false);
      setComparisonModel(null);
      setMessagesModelB([]);
      setStreamingContentModelB("");
      setCurrentMessageModelB("");
      setIsSplitInput(false);
    }
  }, [selectedModel]);

  const handleModelChange = (modelId: string) => {
    const model = models.find((m) => m.alias === modelId);
    if (model) {
      setSelectedModel(model);
      setModelType((model.model_type?.toLowerCase() as ModelType) || "chat");
      navigate(`/playground?model=${encodeURIComponent(modelId)}`);
    }
  };

  // Calculate cosine similarity between two vectors
  const calculateCosineSimilarity = (
    vecA: number[],
    vecB: number[],
  ): number => {
    if (vecA.length !== vecB.length) {
      throw new Error("Vectors must have the same dimension");
    }

    let dotProduct = 0;
    let normA = 0;
    let normB = 0;

    for (let i = 0; i < vecA.length; i++) {
      dotProduct += vecA[i] * vecB[i];
      normA += vecA[i] * vecA[i];
      normB += vecB[i] * vecB[i];
    }

    normA = Math.sqrt(normA);
    normB = Math.sqrt(normB);

    if (normA === 0 || normB === 0) {
      return 0;
    }

    return dotProduct / (normA * normB);
  };

  // Categorize similarity score
  const getSimilarityCategory = (score: number): string => {
    if (score >= 0.9) return "Very Similar";
    if (score >= 0.7) return "Similar";
    if (score >= 0.5) return "Somewhat Similar";
    if (score >= 0.3) return "Slightly Similar";
    return "Different";
  };

  const handleCompareSimilarity = async () => {
    if (!textA.trim() || !textB.trim() || isStreaming || !selectedModel) return;

    setIsStreaming(true);
    setSimilarityResult(null);
    setError(null);

    try {
      // Get embeddings for both texts
      const [responseA, responseB] = await Promise.all([
        openai.embeddings.create({
          model: selectedModel.alias,
          input: textA.trim(),
        }),
        // Note: embeddings API doesn't support include_usage in stream_options
        openai.embeddings.create({
          model: selectedModel.alias,
          input: textB.trim(),
        }),
      ]);

      const embeddingA = responseA.data[0].embedding;
      const embeddingB = responseB.data[0].embedding;

      // Calculate similarity
      const similarity = calculateCosineSimilarity(embeddingA, embeddingB);
      const category = getSimilarityCategory(similarity);

      setSimilarityResult({
        score: similarity,
        category: category,
      });
    } catch (err) {
      console.error("Error comparing similarity:", err);
      setError(
        err instanceof Error ? err.message : "Failed to compare similarity",
      );
    } finally {
      setIsStreaming(false);
    }
  };

  // Reranker functions
  const handleRerank = async () => {
    if (
      !query.trim() ||
      documents.length < 2 ||
      documents.some((doc) => !doc.trim()) ||
      isStreaming ||
      !selectedModel
    )
      return;

    setIsStreaming(true);
    setRerankResult(null);
    setError(null);

    try {
      const response = await fetch(
        `${window.location.origin}/admin/api/v1/ai/rerank`,
        {
          method: "POST",
          headers: {
            "Content-Type": "application/json",
          },
          body: JSON.stringify({
            model: selectedModel.alias,
            query: query.trim(),
            documents: documents
              .filter((doc) => doc.trim())
              .map((doc) => doc.trim()),
          }),
        },
      );

      if (!response.ok) {
        throw new Error(`HTTP ${response.status}: ${response.statusText}`);
      }

      const result = await response.json();
      setRerankResult(result);
    } catch (err) {
      console.error("Error reranking documents:", err);
      setError(
        err instanceof Error ? err.message : "Failed to rerank documents",
      );
    } finally {
      setIsStreaming(false);
    }
  };

  const handleDocumentChange = (index: number, value: string) => {
    const newDocuments = [...documents];
    newDocuments[index] = value;
    setDocuments(newDocuments);
  };

  const handleAddDocument = () => {
    if (documents.length < 10) {
      setDocuments([...documents, ""]);
    }
  };

  const handleRemoveDocument = (index: number) => {
    if (documents.length > 2) {
      const newDocuments = documents.filter((_, i) => i !== index);
      setDocuments(newDocuments);
    }
  };

  const cancelStreaming = () => {
    if (abortController) {
      abortController.abort();
      setAbortController(null);
      setIsStreaming(false);
      setStreamingContent("");
    }
  };

  const handleImageUpload = async (
    event: React.ChangeEvent<HTMLInputElement>,
  ) => {
    const files = event.target.files;
    if (!files || files.length === 0) return;

    const newImages: string[] = [];

    for (let i = 0; i < files.length; i++) {
      const file = files[i];

      // Validate file type
      if (!file.type.startsWith("image/")) {
        setError("Please select only image files");
        continue;
      }

      // Validate file size (max 10MB)
      if (file.size > 10 * 1024 * 1024) {
        setError("Image size must be less than 10MB");
        continue;
      }

      // Convert to base64
      const reader = new FileReader();
      const base64Promise = new Promise<string>((resolve) => {
        reader.onload = (e) => {
          const base64String = e.target?.result as string;
          resolve(base64String);
        };
        reader.readAsDataURL(file);
      });

      const base64 = await base64Promise;
      newImages.push(base64);
    }

    setUploadedImages((prev) => [...prev, ...newImages]);
    // Reset the input so the same file can be uploaded again
    event.target.value = "";
  };

  const handleRemoveImage = (index: number) => {
    setUploadedImages((prev) => prev.filter((_, i) => i !== index));
  };

  const handleSendMessage = async () => {
    if (
      (!currentMessage.trim() && uploadedImages.length === 0) ||
      isStreaming ||
      !selectedModel
    )
      return;

    // Create message content - use multimodal format if images are present
    let messageContent: MessageContent;
    if (uploadedImages.length > 0) {
      const contentParts: (TextContent | ImageContent)[] = [];

      // Add text if present
      if (currentMessage.trim()) {
        contentParts.push({
          type: "text",
          text: currentMessage.trim(),
        });
      }

      // Add images
      uploadedImages.forEach((imageUrl) => {
        contentParts.push({
          type: "image_url",
          image_url: {
            url: imageUrl,
          },
        });
      });

      messageContent = contentParts;
    } else {
      messageContent = currentMessage.trim();
    }

    const userMessage: Message = {
      role: "user",
      content: messageContent,
      timestamp: new Date(),
    };

    setMessages((prev) => [...prev, userMessage]);
    setCurrentMessage("");
    setUploadedImages([]);
    setIsStreaming(true);
    setStreamingContent("");
    setError(null);

    const controller = new AbortController();
    setAbortController(controller);

    // If in comparison mode with unified input, also send to Model B
    if (isComparisonMode && comparisonModel && !isSplitInput) {
      setMessagesModelB((prev) => [...prev, userMessage]);
      setIsStreamingModelB(true);
      setStreamingContentModelB("");
      const controllerB = new AbortController();
      setAbortControllerModelB(controllerB);

      // Start streaming for Model B in parallel
      (async () => {
        try {
          const startTimeB = performance.now();
          let firstTokenTimeB: number | undefined;
          let totalTokensB = 0;
          let inputTokensB = 0;

          // Build messages array with optional system prompt for Model B
          const apiMessagesB: ChatCompletionMessageParam[] = [];

          // Add system prompt if provided (use Model B's system prompt if in comparison mode, otherwise use shared)
          const systemPromptB =
            systemPromptModelB.trim() || systemPrompt.trim();
          if (systemPromptB) {
            apiMessagesB.push({
              role: "system",
              content: systemPromptB,
            });
          }

          // Add conversation history
          messagesModelB.forEach((msg) => {
            apiMessagesB.push({
              role: msg.role,
              content: msg.content,
            } as ChatCompletionMessageParam);
          });

          // Add current user message
          apiMessagesB.push({ role: "user", content: userMessage.content });

          const streamB = await openai.chat.completions.create(
            {
              model: comparisonModel.alias,
              messages: apiMessagesB,
              stream: true,
              stream_options: {
                include_usage: true,
              },
            },
            {
              signal: controllerB.signal,
            },
          );

          let fullContentB = "";
          let chunkCountB = 0;

          for await (const chunk of streamB) {
            const content = chunk.choices[0]?.delta?.content || "";
            if (content) {
              chunkCountB++;
              fullContentB += content;

              // Track time to first token
              if (firstTokenTimeB === undefined) {
                firstTokenTimeB = performance.now() - startTimeB;
              }

              setStreamingContentModelB(fullContentB);
            }

            // Track tokens from usage info
            if (chunk.usage?.completion_tokens) {
              totalTokensB = chunk.usage.completion_tokens;
            }
            if (chunk.usage?.prompt_tokens) {
              inputTokensB = chunk.usage.prompt_tokens;
            }
          }

          const endTimeB = performance.now();
          const totalTimeB = endTimeB - startTimeB;

          // Calculate metrics
          const metricsB: MessageMetrics = {
            timeToFirstToken: firstTokenTimeB,
            totalTime: totalTimeB,
            totalTokens: totalTokensB || chunkCountB,
            inputTokens: inputTokensB || undefined,
            tokensPerSecond:
              totalTokensB && totalTimeB > 0
                ? totalTokensB / (totalTimeB / 1000)
                : undefined,
          };

          const assistantMessageB: Message = {
            role: "assistant",
            content: fullContentB,
            timestamp: new Date(),
            metrics: metricsB,
          };
          setMessagesModelB((prev) => [...prev, assistantMessageB]);
          setStreamingContentModelB("");
        } catch (err) {
          console.error("Error sending message to Model B:", err);
        } finally {
          setIsStreamingModelB(false);
          setAbortControllerModelB(null);
        }
      })();
    }

    try {
      // Build messages array with optional system prompt
      const apiMessages: ChatCompletionMessageParam[] = [];

      // Add system prompt if provided
      if (systemPrompt.trim()) {
        apiMessages.push({
          role: "system",
          content: systemPrompt.trim(),
        });
      }

      // Add conversation history
      messages.forEach((msg) => {
        apiMessages.push({
          role: msg.role,
          content: msg.content,
        } as ChatCompletionMessageParam);
      });

      // Add current user message
      apiMessages.push({ role: "user", content: userMessage.content });

      // Performance tracking
      const startTime = performance.now();
      let firstTokenTime: number | undefined;
      let totalTokens = 0;
      let inputTokens = 0;

      const stream = await openai.chat.completions.create(
        {
          model: selectedModel.alias,
          messages: apiMessages,
          stream: true,
          stream_options: {
            include_usage: true,
          },
        },
        {
          signal: controller.signal,
        },
      );

      let fullContent = "";
      let chunkCount = 0;

      for await (const chunk of stream) {
        const content = chunk.choices[0]?.delta?.content || "";
        if (content) {
          chunkCount++;
          fullContent += content;

          // Track time to first token
          if (firstTokenTime === undefined) {
            firstTokenTime = performance.now() - startTime;
          }

          // Update immediately without requestAnimationFrame to avoid batching
          setStreamingContent(fullContent);
        }

        // Track tokens from usage info
        if (chunk.usage?.completion_tokens) {
          totalTokens = chunk.usage.completion_tokens;
        }
        if (chunk.usage?.prompt_tokens) {
          inputTokens = chunk.usage.prompt_tokens;
        }
      }

      const endTime = performance.now();
      const totalTime = endTime - startTime;

      // Calculate metrics
      const metrics: MessageMetrics = {
        timeToFirstToken: firstTokenTime,
        totalTime,
        totalTokens: totalTokens || chunkCount, // Fallback to chunk count if no usage info
        inputTokens: inputTokens || undefined,
        tokensPerSecond:
          totalTokens && totalTime > 0
            ? totalTokens / (totalTime / 1000)
            : undefined,
      };

      // Add the complete assistant message
      const assistantMessage: Message = {
        role: "assistant",
        content: fullContent,
        timestamp: new Date(),
        metrics,
      };

      setMessages((prev) => [...prev, assistantMessage]);
      setStreamingContent("");
    } catch (err) {
      console.error("Error sending message:", err);
      if (err instanceof Error && err.name === "AbortError") {
        setError("Message cancelled");
      } else {
        setError(err instanceof Error ? err.message : "Failed to send message");
      }
    } finally {
      setIsStreaming(false);
      setAbortController(null);
    }
  };

  // Handler for sending messages to Model B in split input mode
  const handleSendMessageModelB = async () => {
    if (!currentMessageModelB.trim() || isStreamingModelB || !comparisonModel)
      return;

    const userMessage: Message = {
      role: "user",
      content: currentMessageModelB.trim(),
      timestamp: new Date(),
    };

    setMessagesModelB((prev) => [...prev, userMessage]);
    setCurrentMessageModelB("");
    setIsStreamingModelB(true);
    setStreamingContentModelB("");

    const controller = new AbortController();
    setAbortControllerModelB(controller);

    try {
      const startTime = performance.now();
      let firstTokenTime: number | undefined;
      let totalTokens = 0;
      let inputTokens = 0;

      // Build messages array with optional system prompt for Model B
      const apiMessagesB: ChatCompletionMessageParam[] = [];

      // Add system prompt if provided (use Model B's system prompt if set, otherwise use shared)
      const systemPromptB = systemPromptModelB.trim() || systemPrompt.trim();
      if (systemPromptB) {
        apiMessagesB.push({
          role: "system",
          content: systemPromptB,
        });
      }

      // Add conversation history
      messagesModelB.forEach((msg) => {
        apiMessagesB.push({
          role: msg.role,
          content: msg.content,
        } as ChatCompletionMessageParam);
      });

      // Add current user message
      apiMessagesB.push({ role: "user", content: userMessage.content });

      const stream = await openai.chat.completions.create(
        {
          model: comparisonModel.alias,
          messages: apiMessagesB,
          stream: true,
          stream_options: {
            include_usage: true,
          },
        },
        {
          signal: controller.signal,
        },
      );

      let fullContent = "";
      let chunkCount = 0;

      for await (const chunk of stream) {
        const content = chunk.choices[0]?.delta?.content || "";
        if (content) {
          chunkCount++;
          fullContent += content;

          // Track time to first token
          if (firstTokenTime === undefined) {
            firstTokenTime = performance.now() - startTime;
          }

          setStreamingContentModelB(fullContent);
        }

        // Track tokens from usage info
        if (chunk.usage?.completion_tokens) {
          totalTokens = chunk.usage.completion_tokens;
        }
        if (chunk.usage?.prompt_tokens) {
          inputTokens = chunk.usage.prompt_tokens;
        }
      }

      const endTime = performance.now();
      const totalTime = endTime - startTime;

      // Calculate metrics
      const metrics: MessageMetrics = {
        timeToFirstToken: firstTokenTime,
        totalTime,
        totalTokens: totalTokens || chunkCount,
        inputTokens: inputTokens || undefined,
        tokensPerSecond:
          totalTokens && totalTime > 0
            ? totalTokens / (totalTime / 1000)
            : undefined,
      };

      const assistantMessage: Message = {
        role: "assistant",
        content: fullContent,
        timestamp: new Date(),
        metrics,
      };

      setMessagesModelB((prev) => [...prev, assistantMessage]);
      setStreamingContentModelB("");
    } catch (err) {
      console.error("Error sending message to Model B:", err);
      setError(
        err instanceof Error
          ? err.message
          : "Failed to send message to Model B",
      );
    } finally {
      setIsStreamingModelB(false);
      setAbortControllerModelB(null);
    }
  };

  const handleKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      if (modelType === "embeddings") {
        handleCompareSimilarity();
      } else if (modelType === "reranker") {
        handleRerank();
      } else {
        handleSendMessage();
      }
    }
  };

  const clearConversation = () => {
    setMessages([]);
    setStreamingContent("");
    setSimilarityResult(null);
    setRerankResult(null);
    setError(null);
    setUploadedImages([]);
    setSystemPrompt("");
    setSystemPromptModelB("");
    setTextA("");
    setTextB("");
    setQuery("What is the capital of France?");
    setDocuments([
      "The capital of Brazil is Brasilia.",
      "The capital of France is Paris.",
      "Horses and cows are both animals",
    ]);
    // Also clear comparison model messages
    if (isComparisonMode) {
      setMessagesModelB([]);
      setStreamingContentModelB("");
      setCurrentMessageModelB("");
    }
  };

  const handleComparisonModelSelect = (modelId: string) => {
    const model = models.find((m) => m.alias === modelId);
    if (model) {
      setComparisonModel(model);
      setIsComparisonMode(true);
      setMessagesModelB([]);
      setStreamingContentModelB("");
      setCurrentMessageModelB("");
    }
  };

  const handleExitComparisonMode = () => {
    setIsComparisonMode(false);
    setComparisonModel(null);
    setMessagesModelB([]);
    setStreamingContentModelB("");
    setCurrentMessageModelB("");
    setIsSplitInput(false);
  };

  const handleCopyMessagesToModelB = () => {
    // Copy messages but exclude metrics since they're model-specific
    const messagesWithoutMetrics = messages.map((msg) => ({
      ...msg,
      metrics: undefined,
    }));
    setMessagesModelB(messagesWithoutMetrics);
    setStreamingContentModelB("");
  };

  const handleCopyMessagesToModelA = () => {
    // Copy messages but exclude metrics since they're model-specific
    const messagesWithoutMetrics = messagesModelB.map((msg) => ({
      ...msg,
      metrics: undefined,
    }));
    setMessages(messagesWithoutMetrics);
    setStreamingContent("");
  };

  const [copiedMessageIndex, setCopiedMessageIndex] = useState<number | null>(
    null,
  );
  const [abortController, setAbortController] =
    useState<AbortController | null>(null);

  // Comparison mode state
  const [isComparisonMode, setIsComparisonMode] = useState(false);
  const [comparisonModel, setComparisonModel] = useState<Model | null>(null);
  const [messagesModelB, setMessagesModelB] = useState<Message[]>([]);
  const [streamingContentModelB, setStreamingContentModelB] = useState("");
  const [isStreamingModelB, setIsStreamingModelB] = useState(false);
  const [_abortControllerModelB, setAbortControllerModelB] =
    useState<AbortController | null>(null);
  const [isSplitInput, setIsSplitInput] = useState(false);
  const [currentMessageModelB, setCurrentMessageModelB] = useState("");

  const copyMessage = async (content: string, messageIndex: number) => {
    try {
      await navigator.clipboard.writeText(content);
      setCopiedMessageIndex(messageIndex);
      toast.success("Message copied to clipboard");
      setTimeout(() => setCopiedMessageIndex(null), 2000);
    } catch (err) {
      console.error("Failed to copy to clipboard:", err);
      toast.error("Failed to copy message");
    }
  };

  return (
    <div className="h-[calc(100vh-4rem)] bg-white flex flex-col">
      {/* Header */}
      <div className="bg-white border-b border-gray-200 px-4 md:px-8 py-3 flex-shrink-0">
        <div className="flex flex-col md:flex-row items-start md:items-center justify-between gap-3">
          <div className="flex items-center gap-2 md:gap-4 w-full md:w-auto">
            <button
              onClick={() => navigate(fromUrl || "/models")}
              className="p-2 text-gray-500 hover:bg-gray-100 rounded-lg transition-colors flex-shrink-0"
              aria-label={fromUrl ? "Go back" : "Back to Models"}
              title={fromUrl ? "Go back" : "Back to Models"}
            >
              <ArrowLeft className="w-5 h-5" />
            </button>
            <div className="flex items-center gap-2 md:gap-3 min-w-0">
              <div className="w-8 h-8 md:w-10 md:h-10 bg-gray-100 rounded-lg flex items-center justify-center flex-shrink-0">
                <Play className="w-4 h-4 md:w-5 md:h-5 text-gray-600" />
              </div>
              <div className="min-w-0">
                <h1 className="text-lg md:text-2xl font-bold text-gray-900 truncate">
                  {modelType === "embeddings"
                    ? "Embeddings Playground"
                    : modelType === "reranker"
                      ? "Reranker Playground"
                      : "Chat Playground"}
                </h1>
                <p className="text-xs md:text-sm text-gray-600 hidden sm:block">
                  {modelType === "embeddings"
                    ? "Generate vector embeddings from text"
                    : modelType === "reranker"
                      ? "Rank documents by relevance to a query"
                      : "Test AI models with custom settings"}
                </p>
              </div>
            </div>
          </div>
          <div className="flex items-center gap-2 md:gap-3 w-full md:w-auto">
            {/* Model Selector */}
            <Popover open={modelSelectOpen} onOpenChange={setModelSelectOpen}>
              <PopoverTrigger asChild>
                <Button
                  variant="outline"
                  role="combobox"
                  aria-expanded={modelSelectOpen}
                  className="w-full md:w-[200px] justify-between text-left"
                >
                  <span className="truncate">
                    {selectedModel?.alias || "Select a model..."}
                  </span>
                  <ChevronsUpDownIcon className="ml-2 h-4 w-4 shrink-0 opacity-50" />
                </Button>
              </PopoverTrigger>
              <PopoverContent
                className="p-0"
                style={{ width: "var(--radix-popover-trigger-width)" }}
              >
                <Command shouldFilter={false}>
                  <CommandInput
                    placeholder="Search models..."
                    value={modelSearchQuery}
                    onValueChange={setModelSearchQuery}
                  />
                  <CommandList>
                    <CommandEmpty>No models found.</CommandEmpty>
                    <CommandGroup>
                      {models.map((model) => (
                        <CommandItem
                          key={model.id}
                          value={model.alias}
                          onSelect={() => {
                            handleModelChange(model.alias);
                            setModelSelectOpen(false);
                          }}
                        >
                          <span className="truncate">{model.alias}</span>
                        </CommandItem>
                      ))}
                    </CommandGroup>
                  </CommandList>
                </Command>
              </PopoverContent>
            </Popover>

            {/* Comparison Mode Button - Only show for chat models */}
            {selectedModel && modelType === "chat" && (
              <>
                {!isComparisonMode ? (
                  <Popover
                    open={compareSelectOpen}
                    onOpenChange={setCompareSelectOpen}
                  >
                    <PopoverTrigger asChild>
                      <Button
                        variant="outline"
                        role="combobox"
                        aria-expanded={compareSelectOpen}
                        className="w-full md:w-[180px] justify-between text-left"
                      >
                        <GitCompare className="w-4 h-4 mr-2" />
                        <span className="truncate">Compare...</span>
                        <ChevronsUpDownIcon className="ml-2 h-4 w-4 shrink-0 opacity-50" />
                      </Button>
                    </PopoverTrigger>
                    <PopoverContent
                      className="p-0"
                      style={{ width: "var(--radix-popover-trigger-width)" }}
                    >
                      <Command shouldFilter={false}>
                        <CommandInput
                          placeholder="Search models..."
                          value={compareSearchQuery}
                          onValueChange={setCompareSearchQuery}
                        />
                        <CommandList>
                          <CommandEmpty>No models found.</CommandEmpty>
                          <CommandGroup>
                            {compareModels.map((model) => (
                              <CommandItem
                                key={model.id}
                                value={model.alias}
                                onSelect={() => {
                                  handleComparisonModelSelect(model.alias);
                                  setCompareSelectOpen(false);
                                }}
                                className="flex items-center"
                              >
                                <span className="truncate">{model.alias}</span>
                              </CommandItem>
                            ))}
                          </CommandGroup>
                        </CommandList>
                      </Command>
                    </PopoverContent>
                  </Popover>
                ) : (
                  <div className="flex items-center gap-2">
                    <div className="text-sm text-gray-600 flex items-center gap-2 bg-gray-100 rounded-lg px-3 py-2">
                      <GitCompare className="w-4 h-4" />
                      <span className="font-medium">
                        {comparisonModel?.alias}
                      </span>
                    </div>
                    <Button
                      variant="ghost"
                      size="icon"
                      onClick={handleExitComparisonMode}
                      aria-label="Exit comparison mode"
                      title="Exit comparison mode"
                    >
                      <XIcon className="w-4 h-4" />
                    </Button>
                  </div>
                )}
              </>
            )}
          </div>
        </div>
      </div>

      {/* Content */}
      {!selectedModel ? (
        <div
          className="flex-1 flex items-center justify-center"
          role="main"
          aria-label="Welcome to playground"
        >
          <div className="text-center">
            <Play className="w-16 h-16 text-gray-400 mx-auto mb-4" />
            <h2 className="text-xl text-gray-600 mb-2">
              Welcome to the Playground
            </h2>
            <p className="text-gray-500">
              Select a model from the dropdown to start testing
            </p>
          </div>
        </div>
      ) : modelType === "embeddings" ? (
        <div className="flex-1 overflow-y-auto px-8 py-6">
          <EmbeddingPlayground
            selectedModel={selectedModel}
            textA={textA}
            textB={textB}
            similarityResult={similarityResult}
            isStreaming={isStreaming}
            error={error}
            onTextAChange={setTextA}
            onTextBChange={setTextB}
            onCompareSimilarity={handleCompareSimilarity}
            onClearResult={() => setSimilarityResult(null)}
            onKeyDown={handleKeyDown}
          />
        </div>
      ) : modelType === "reranker" ? (
        <div className="flex-1 overflow-y-auto px-8 py-6">
          <RerankPlayground
            selectedModel={selectedModel}
            query={query}
            documents={documents}
            rerankResult={rerankResult}
            isStreaming={isStreaming}
            error={error}
            onQueryChange={setQuery}
            onDocumentChange={handleDocumentChange}
            onAddDocument={handleAddDocument}
            onRemoveDocument={handleRemoveDocument}
            onRerank={handleRerank}
            onClearResult={() => setRerankResult(null)}
            onKeyDown={handleKeyDown}
          />
        </div>
      ) : (
        <GenerationPlayground
          selectedModel={selectedModel}
          messages={messages}
          currentMessage={currentMessage}
          uploadedImages={uploadedImages}
          streamingContent={streamingContent}
          isStreaming={isStreaming}
          error={error}
          copiedMessageIndex={copiedMessageIndex}
          supportsImages={
            selectedModel.capabilities?.includes("vision") ?? false
          }
          systemPrompt={systemPrompt}
          onSystemPromptChange={setSystemPrompt}
          systemPromptModelB={systemPromptModelB}
          onSystemPromptModelBChange={setSystemPromptModelB}
          onCurrentMessageChange={setCurrentMessage}
          onImageUpload={handleImageUpload}
          onRemoveImage={handleRemoveImage}
          onSendMessage={handleSendMessage}
          onCopyMessage={copyMessage}
          onKeyDown={handleKeyDown}
          onClearConversation={clearConversation}
          onCancelStreaming={cancelStreaming}
          // Comparison mode props
          isComparisonMode={isComparisonMode}
          comparisonModel={comparisonModel}
          messagesModelB={messagesModelB}
          streamingContentModelB={streamingContentModelB}
          isStreamingModelB={isStreamingModelB}
          isSplitInput={isSplitInput}
          currentMessageModelB={currentMessageModelB}
          onCurrentMessageModelBChange={setCurrentMessageModelB}
          onToggleSplitInput={() => setIsSplitInput(!isSplitInput)}
          onSendMessageModelB={handleSendMessageModelB}
          onCopyMessagesToModelB={handleCopyMessagesToModelB}
          onCopyMessagesToModelA={handleCopyMessagesToModelA}
        />
      )}
    </div>
  );
};

export default Playground;
