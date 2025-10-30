import React, { useState, useEffect } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { Play, ArrowLeft } from "lucide-react";
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
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "../../../ui/select";

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

  const { data: models = [], error: modelsError } = useModels();

  // Initialize OpenAI client pointing to our API
  const baseURL = `${window.location.origin}/admin/api/v1/ai/v1`;
  console.log("OpenAI Base URL:", baseURL);

  const openai = new OpenAI({
    baseURL,
    apiKey: "placeholder", // This should be handled by your auth system
    dangerouslyAllowBrowser: true,
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
      setTextA("");
      setTextB("");
      setQuery("What is the capital of France?");
      setDocuments([
        "The capital of Brazil is Brasilia.",
        "The capital of France is Paris.",
        "Horses and cows are both animals",
      ]);
    }
  }, [selectedModel]);

  const handleModelChange = (modelId: string) => {
    const model = models.find((m) => m.alias === modelId);
    if (model) {
      setSelectedModel(model);
      setModelType(
        (model.model_type?.toLowerCase() as ModelType) || "chat",
      );
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

    try {
      console.log("Sending request to model:", selectedModel.alias);
      console.log("Full request URL will be:", `${baseURL}/chat/completions`);

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
          console.log(
            `Chunk ${chunkCount}: "${content}" (length: ${content.length})`,
          );

          // Update immediately without requestAnimationFrame to avoid batching
          setStreamingContent(fullContent);
        }
      }

      console.log(`Total chunks received: ${chunkCount}`);

      // Add the complete assistant message
      const assistantMessage: Message = {
        role: "assistant",
        content: fullContent,
        timestamp: new Date(),
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
    setTextA("");
    setTextB("");
    setQuery("What is the capital of France?");
    setDocuments([
      "The capital of Brazil is Brasilia.",
      "The capital of France is Paris.",
      "Horses and cows are both animals",
    ]);
  };

  const [copiedMessageIndex, setCopiedMessageIndex] = useState<number | null>(
    null,
  );
  const [abortController, setAbortController] =
    useState<AbortController | null>(null);

  const copyMessage = (content: string, messageIndex: number) => {
    navigator.clipboard.writeText(content);
    setCopiedMessageIndex(messageIndex);
    setTimeout(() => setCopiedMessageIndex(null), 2000);
  };

  return (
    <div className="h-[calc(100vh-4rem)] bg-white flex flex-col">
      {/* Header */}
      <div className="bg-white border-b border-gray-200 px-8 py-3 flex-shrink-0">
        <div className="flex items-center justify-between">
          <div className="flex items-center gap-4">
            <button
              onClick={() => navigate(fromUrl || "/models")}
              className="p-2 text-gray-500 hover:bg-gray-100 rounded-lg transition-colors"
              aria-label={fromUrl ? "Go back" : "Back to Models"}
              title={fromUrl ? "Go back" : "Back to Models"}
            >
              <ArrowLeft className="w-5 h-5" />
            </button>
            <div className="flex items-center gap-3">
              <div className="w-10 h-10 bg-gray-100 rounded-lg flex items-center justify-center">
                <Play className="w-5 h-5 text-gray-600" />
              </div>
              <div>
                <h1 className="text-2xl font-bold text-gray-900">
                  {modelType === "embeddings"
                    ? "Embeddings Playground"
                    : modelType === "reranker"
                      ? "Reranker Playground"
                      : "Chat Playground"}
                </h1>
                <p className="text-sm text-gray-600">
                  {modelType === "embeddings"
                    ? "Generate vector embeddings from text"
                    : modelType === "reranker"
                      ? "Rank documents by relevance to a query"
                      : "Test AI models with custom settings"}
                </p>
              </div>
            </div>
          </div>
          <div className="flex items-center gap-3">
            {/* Model Selector */}
            <Select
              value={selectedModel?.alias || ""}
              onValueChange={handleModelChange}
              disabled={!models.length}
            >
              <SelectTrigger className="w-[200px]" aria-label="Select model">
                <SelectValue placeholder="Select a model..." />
              </SelectTrigger>
              <SelectContent>
                {models.map((model) => (
                  <SelectItem key={model.id} value={model.alias}>
                    {model.alias}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
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
          onCurrentMessageChange={setCurrentMessage}
          onImageUpload={handleImageUpload}
          onRemoveImage={handleRemoveImage}
          onSendMessage={handleSendMessage}
          onCopyMessage={copyMessage}
          onKeyDown={handleKeyDown}
          onClearConversation={clearConversation}
          onCancelStreaming={cancelStreaming}
        />
      )}
    </div>
  );
};

export default Playground;
