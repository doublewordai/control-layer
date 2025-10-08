// Transform backend RequestResponsePair to frontend DisplayRequest format
import type { RequestResponsePair } from "../api/clay/types";
import type { RequestsEntry } from "../components/features/requests/types";

/**
 * Transform a backend RequestResponsePair to a frontend RequestsEntry
 */
export function transformRequestResponsePair(
  pair: RequestResponsePair,
): RequestsEntry {
  const requestBody = pair.request.body;
  const responseBody = pair.response?.body;

  // Extract model from request - AiRequest is always a tagged union
  const model = requestBody?.data?.model || "unknown";

  // Determine request type
  const request_type = requestBody?.type || "other";

  // Get request content preview
  let request_content = "No request body";
  if (requestBody) {
    switch (requestBody.type) {
      case "chat_completions": {
        const data = requestBody.data;
        const lastMessage = data.messages
          ?.slice()
          .reverse()
          .find((m: any) => m.role === "user");
        request_content = lastMessage?.content || "No user message found";
        break;
      }
      case "completions": {
        const data = requestBody.data;
        request_content = data.prompt || "No prompt found";
        break;
      }
      case "embeddings": {
        const data = requestBody.data;
        const input = data.input;
        if (Array.isArray(input)) {
          request_content = `${input.length} texts: ${input[0] || ""}...`;
        } else {
          request_content = String(input || "No input found");
        }
        break;
      }
      case "rerank": {
        const data = requestBody.data;
        request_content = `Query: "${data.query}" | ${data.documents?.length || 0} documents`;
        break;
      }
      case "other": {
        request_content = requestBody.data
          ? JSON.stringify(requestBody.data)
          : "No data found";
        break;
      }
    }
  }

  // Get response content preview and usage
  let response_content = "No response";
  let usage: RequestsEntry["usage"] = undefined;

  if (responseBody) {
    switch (responseBody.type) {
      case "chat_completions": {
        const data = responseBody.data;
        response_content =
          data.choices?.[0]?.message?.content || "No response content";
        usage = data.usage;
        break;
      }
      case "chat_completions_stream": {
        const chunks = responseBody.data;
        let content = "";
        if (Array.isArray(chunks)) {
          for (const chunk of chunks) {
            if (chunk && chunk.choices?.[0]?.delta?.content) {
              content += chunk.choices?.[0]?.delta?.content;
            }
            if (chunk && chunk.usage) {
              usage = chunk.usage;
            }
          }
        }
        response_content = content || "No stream content";
        break;
      }
      case "completions": {
        const data = responseBody.data;
        response_content = data.choices?.[0]?.text || "No completion text";
        usage = data.usage;
        break;
      }
      case "embeddings": {
        const data = responseBody.data;
        response_content = `Generated ${data.data?.length || 0} embeddings`;
        usage = data.usage
          ? {
              prompt_tokens: data.usage.prompt_tokens,
              completion_tokens: 0,
              total_tokens: data.usage.total_tokens,
            }
          : undefined;
        break;
      }
      case "rerank": {
        const data = responseBody.data;
        response_content = `Ranked ${data.results?.length || 0} documents`;
        usage = data.usage
          ? {
              prompt_tokens: 0,
              completion_tokens: 0,
              total_tokens: data.usage.total_tokens,
            }
          : undefined;
        break;
      }
      case "other": {
        response_content = responseBody.data
          ? JSON.stringify(responseBody.data)
          : "No data found";
        break;
      }
    }
  }

  return {
    id: pair.request.id.toString(),
    timestamp: pair.request.timestamp,
    model,
    duration_ms: pair.response?.duration_ms || 0,
    request_type: request_type as RequestsEntry["request_type"],
    request_content,
    response_content,
    status_code: pair.response?.status_code,
    usage,
    details: {
      request: {
        method: pair.request.method,
        uri: pair.request.uri,
        headers: pair.request.headers,
        body: pair.request.body,
      },
      response: pair.response
        ? {
            status_code: pair.response.status_code,
            headers: pair.response.headers,
            body: pair.response.body,
          }
        : undefined,
    },
  };
}

/**
 * Transform an array of backend RequestResponsePairs to frontend RequestsEntries
 */
export function transformRequestResponsePairs(
  pairs: RequestResponsePair[],
): RequestsEntry[] {
  return pairs.map(transformRequestResponsePair);
}
