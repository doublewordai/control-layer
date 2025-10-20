// Frontend display types for the requests feature
import type { AiRequest, AiResponse } from "../../../api/control-layer/types";

export interface RequestsEntry {
  id: string;
  timestamp: string;
  model: string;
  duration_ms: number;
  request_type:
    | "chat_completions"
    | "completions"
    | "embeddings"
    | "rerank"
    | "other";
  request_content: string; // User-friendly preview of request content
  response_content: string; // User-friendly preview of response content
  status_code?: number;
  usage?: {
    prompt_tokens: number;
    completion_tokens: number;
    total_tokens: number;
  };
  details: {
    request: {
      method: string;
      uri: string;
      headers: Record<string, any>;
      body?: AiRequest;
    };
    response?: {
      status_code: number;
      headers: Record<string, any>;
      body?: AiResponse;
    };
  };
}
