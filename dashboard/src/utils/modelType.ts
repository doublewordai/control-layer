export type ModelType = "chat" | "embeddings" | "reranker";

export function detectModelType(modelName: string): ModelType {
  const name = modelName.toLowerCase();

  // Reranker model patterns
  const rerankerPatterns = [
    "rerank",
    "reranker",
    "cross-encoder",
    "bge-reranker",
    "mixedbread-reranker",
    "mxbai-rerank",
  ];

  // Embeddings model patterns
  const embeddingPatterns = [
    "embed",
    "embedding",
    "ada",
    "text-embedding",
    "sentence-transformer",
    "all-minilm",
    "bge-",
    "e5-",
  ];

  // Check if model name contains any reranker patterns
  if (rerankerPatterns.some((pattern) => name.includes(pattern))) {
    return "reranker";
  }

  // Check if model name contains any embedding patterns
  if (embeddingPatterns.some((pattern) => name.includes(pattern))) {
    return "embeddings";
  }

  // Default to chat for everything else
  return "chat";
}

const MODEL_TYPE_STORAGE_KEY = "model-type-overrides";

export function getModelTypeOverrides(): Record<string, ModelType> {
  try {
    const stored = localStorage.getItem(MODEL_TYPE_STORAGE_KEY);
    return stored ? JSON.parse(stored) : {};
  } catch {
    return {};
  }
}

export function setModelTypeOverride(modelId: string, type: ModelType): void {
  try {
    const overrides = getModelTypeOverrides();
    overrides[modelId] = type;
    localStorage.setItem(MODEL_TYPE_STORAGE_KEY, JSON.stringify(overrides));
  } catch (error) {
    console.warn("Failed to save model type override:", error);
  }
}

export function getModelType(modelId: string, modelName: string): ModelType {
  const overrides = getModelTypeOverrides();

  // Return user override if exists
  if (overrides[modelId]) {
    return overrides[modelId];
  }

  // Otherwise use auto-detection
  return detectModelType(modelName);
}

export function clearModelTypeOverride(modelId: string): void {
  try {
    const overrides = getModelTypeOverrides();
    delete overrides[modelId];
    localStorage.setItem(MODEL_TYPE_STORAGE_KEY, JSON.stringify(overrides));
  } catch (error) {
    console.warn("Failed to clear model type override:", error);
  }
}
