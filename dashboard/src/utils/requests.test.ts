import { describe, it, expect } from "vitest";
import { transformRequestResponsePairs } from "./requests";

describe("requests utils", () => {
  it("should transform empty array to empty array", () => {
    const result = transformRequestResponsePairs([]);
    expect(result).toEqual([]);
  });

  it("should transform chat completions request", () => {
    const pair = {
      request: {
        id: 1,
        timestamp: "2023-01-01T00:00:00Z",
        method: "POST",
        uri: "/v1/chat/completions",
        headers: {},
        body: {
          type: "chat_completions" as const,
          data: {
            model: "gpt-4",
            messages: [{ role: "user" as const, content: "Hello" }],
          },
        },
        created_at: "2023-01-01T00:00:00Z",
      },
      response: undefined,
    };

    const result = transformRequestResponsePairs([pair]);
    expect(result).toHaveLength(1);
    expect(result[0].id).toBe("1");
    expect(result[0].request_type).toBe("chat_completions");
    expect(result[0].request_content).toBe("Hello");
    expect(result[0].model).toBe("gpt-4");

    // These are a bit more questionable: what should we display if there's no response?
    // We choose zero duration and "No response" content.
    expect(result[0].duration_ms).toBe(0);
    expect(result[0].response_content).toBe("No response");
  });

  it("should transform completions request", () => {
    const pair = {
      request: {
        id: 3,
        timestamp: "2023-01-01T00:00:00Z",
        method: "POST",
        uri: "/v1/completions",
        headers: {},
        body: {
          type: "completions" as const,
          data: {
            model: "gpt-3.5-turbo-instruct",
            prompt: "Write a haiku about cats",
          },
        },
        created_at: "2023-01-01T00:00:00Z",
      },
      response: undefined,
    };

    const result = transformRequestResponsePairs([pair]);
    expect(result[0].request_type).toBe("completions");
    expect(result[0].request_content).toBe("Write a haiku about cats");
    expect(result[0].model).toBe("gpt-3.5-turbo-instruct");
  });

  it("should transform embeddings request with string input", () => {
    const pair = {
      request: {
        id: 4,
        timestamp: "2023-01-01T00:00:00Z",
        method: "POST",
        uri: "/v1/embeddings",
        headers: {},
        body: {
          type: "embeddings" as const,
          data: {
            model: "text-embedding-ada-002",
            input: "Hello world",
          },
        },
        created_at: "2023-01-01T00:00:00Z",
      },
      response: undefined,
    };

    const result = transformRequestResponsePairs([pair]);
    expect(result[0].request_type).toBe("embeddings");
    expect(result[0].request_content).toBe("Hello world");
    expect(result[0].model).toBe("text-embedding-ada-002");
  });

  it("should transform embeddings request with array input", () => {
    const pair = {
      request: {
        id: 5,
        timestamp: "2023-01-01T00:00:00Z",
        method: "POST",
        uri: "/v1/embeddings",
        headers: {},
        body: {
          type: "embeddings" as const,
          data: {
            model: "text-embedding-ada-002",
            input: ["First text", "Second text", "Third text"],
          },
        },
        created_at: "2023-01-01T00:00:00Z",
      },
      response: undefined,
    };

    const result = transformRequestResponsePairs([pair]);
    expect(result[0].request_type).toBe("embeddings");
    expect(result[0].request_content).toBe("3 texts: First text...");
    expect(result[0].model).toBe("text-embedding-ada-002");
  });

  it("should transform other request type", () => {
    const pair = {
      request: {
        id: 6,
        timestamp: "2023-01-01T00:00:00Z",
        method: "POST",
        uri: "/v1/custom",
        headers: {},
        body: {
          type: "other" as const,
          data: {
            model: "custom-model",
            prompt: "Custom prompt",
          },
        },
        created_at: "2023-01-01T00:00:00Z",
      },
      response: undefined,
    };

    const result = transformRequestResponsePairs([pair]);
    expect(result[0].request_type).toBe("other");
    expect(result[0].request_content).toBe(
      '{"model":"custom-model","prompt":"Custom prompt"}',
    );
    expect(result[0].model).toBe("custom-model");
  });

  it("should transform chat_completions_stream response", () => {
    const pair = {
      request: {
        id: 2,
        timestamp: "2023-01-01T00:00:00Z",
        method: "POST",
        uri: "/v1/chat/completions",
        headers: {},
        body: {
          type: "chat_completions" as const,
          data: {
            model: "gpt-4",
            messages: [{ role: "user" as const, content: "Hello" }],
            stream: true,
          },
        },
        created_at: "2023-01-01T00:00:00Z",
      },
      response: {
        id: 2,
        timestamp: "2023-01-01T00:00:01Z",
        status_code: 200,
        headers: {},
        body: {
          type: "chat_completions_stream" as const,
          data: [
            {
              id: "chatcmpl-123",
              object: "chat.completion.chunk",
              created: 1677652288,
              model: "gpt-4",
              choices: [{ index: 0, delta: { content: "Hello" } }],
            },
            {
              id: "chatcmpl-123",
              object: "chat.completion.chunk",
              created: 1677652288,
              model: "gpt-4",
              choices: [
                {
                  index: 0,
                  delta: { content: " world!" },
                  finish_reason: "stop",
                },
              ],
            },
          ],
        },
        duration_ms: 1500,
        created_at: "2023-01-01T00:00:01Z",
      },
    };

    const result = transformRequestResponsePairs([pair]);
    expect(result).toHaveLength(1);
    expect(result[0].id).toBe("2");
    expect(result[0].request_type).toBe("chat_completions");
    expect(result[0].request_content).toBe("Hello");
    expect(result[0].model).toBe("gpt-4");
    expect(result[0].duration_ms).toBe(1500);
    expect(result[0].response_content).toBe("Hello world!");
    expect(result[0].status_code).toBe(200);
  });

  it("should transform chat_completions response", () => {
    const pair = {
      request: {
        id: 8,
        timestamp: "2023-01-01T00:00:00Z",
        method: "POST",
        uri: "/v1/chat/completions",
        headers: {},
        body: {
          type: "chat_completions" as const,
          data: {
            model: "gpt-4",
            messages: [{ role: "user" as const, content: "Hello" }],
          },
        },
        created_at: "2023-01-01T00:00:00Z",
      },
      response: {
        id: 8,
        timestamp: "2023-01-01T00:00:01Z",
        status_code: 200,
        headers: {},
        body: {
          type: "chat_completions" as const,
          data: {
            id: "chatcmpl-123",
            object: "chat.completion",
            created: 1677652288,
            model: "gpt-4",
            choices: [
              {
                index: 0,
                message: {
                  role: "assistant" as const,
                  content: "Hello! How can I help you?",
                },
                finish_reason: "stop",
              },
            ],
            usage: {
              prompt_tokens: 10,
              completion_tokens: 15,
              total_tokens: 25,
            },
          },
        },
        duration_ms: 800,
        created_at: "2023-01-01T00:00:01Z",
      },
    };

    const result = transformRequestResponsePairs([pair]);
    expect(result[0].response_content).toBe("Hello! How can I help you?");
    expect(result[0].usage).toEqual({
      prompt_tokens: 10,
      completion_tokens: 15,
      total_tokens: 25,
    });
  });

  it("should transform completions response", () => {
    const pair = {
      request: {
        id: 9,
        timestamp: "2023-01-01T00:00:00Z",
        method: "POST",
        uri: "/v1/completions",
        headers: {},
        body: {
          type: "completions" as const,
          data: {
            model: "gpt-3.5-turbo-instruct",
            prompt: "Write a haiku",
          },
        },
        created_at: "2023-01-01T00:00:00Z",
      },
      response: {
        id: 9,
        timestamp: "2023-01-01T00:00:01Z",
        status_code: 200,
        headers: {},
        body: {
          type: "completions" as const,
          data: {
            id: "cmpl-123",
            object: "text_completion",
            created: 1677652288,
            model: "gpt-3.5-turbo-instruct",
            choices: [
              {
                index: 0,
                text: "\n\nCode flows like stream\nThrough silicon valleys deep\nBugs are debugged",
                finish_reason: "stop",
              },
            ],
            usage: {
              prompt_tokens: 5,
              completion_tokens: 20,
              total_tokens: 25,
            },
          },
        },
        duration_ms: 1200,
        created_at: "2023-01-01T00:00:01Z",
      },
    };

    const result = transformRequestResponsePairs([pair]);
    expect(result[0].response_content).toBe(
      "\n\nCode flows like stream\nThrough silicon valleys deep\nBugs are debugged",
    );
    expect(result[0].usage).toEqual({
      prompt_tokens: 5,
      completion_tokens: 20,
      total_tokens: 25,
    });
  });

  it("should transform embeddings response", () => {
    const pair = {
      request: {
        id: 10,
        timestamp: "2023-01-01T00:00:00Z",
        method: "POST",
        uri: "/v1/embeddings",
        headers: {},
        body: {
          type: "embeddings" as const,
          data: {
            model: "text-embedding-ada-002",
            input: "Hello world",
          },
        },
        created_at: "2023-01-01T00:00:00Z",
      },
      response: {
        id: 10,
        timestamp: "2023-01-01T00:00:01Z",
        status_code: 200,
        headers: {},
        body: {
          type: "embeddings" as const,
          data: {
            object: "list",
            data: [
              { index: 0, embedding: [0.1, 0.2, 0.3] },
              { index: 1, embedding: [0.4, 0.5, 0.6] },
            ],
            model: "text-embedding-ada-002",
            usage: {
              prompt_tokens: 2,
              total_tokens: 2,
            },
          },
        },
        duration_ms: 300,
        created_at: "2023-01-01T00:00:01Z",
      },
    };

    const result = transformRequestResponsePairs([pair]);
    expect(result[0].response_content).toBe("Generated 2 embeddings");
    expect(result[0].usage).toEqual({
      prompt_tokens: 2,
      completion_tokens: 0,
      total_tokens: 2,
    });
  });

  it("should handle other response type with null data", () => {
    const pair = {
      request: {
        id: 11,
        timestamp: "2023-01-01T00:00:00Z",
        method: "POST",
        uri: "/v1/custom",
        headers: {},
        body: {
          type: "other" as const,
          data: { model: "custom-model" },
        },
        created_at: "2023-01-01T00:00:00Z",
      },
      response: {
        id: 11,
        timestamp: "2023-01-01T00:00:01Z",
        status_code: 200,
        headers: {},
        body: {
          type: "other" as const,
          data: null,
        },
        duration_ms: 500,
        created_at: "2023-01-01T00:00:01Z",
      },
    };

    const result = transformRequestResponsePairs([pair]);
    expect(result[0].response_content).toBe("No data found");
  });
});
