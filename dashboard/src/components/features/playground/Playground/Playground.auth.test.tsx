import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import OpenAI from "openai";

describe("Playground OpenAI Client Authentication", () => {
  let fetchSpy: any;
  let originalFetch: typeof fetch;

  beforeEach(() => {
    originalFetch = global.fetch;
    fetchSpy = vi
      .spyOn(global, "fetch")
      .mockImplementation(async (url, _init) => {
        // Return a mock streaming response for OpenAI API calls
        if (url.toString().includes("/chat/completions")) {
          const encoder = new TextEncoder();
          const stream = new ReadableStream({
            async start(controller) {
              controller.enqueue(
                encoder.encode(
                  'data: {"choices":[{"delta":{"content":"test"}}]}\n\n',
                ),
              );
              controller.enqueue(encoder.encode("data: [DONE]\n\n"));
              controller.close();
            },
          });

          return new Response(stream, {
            status: 200,
            headers: { "Content-Type": "text/event-stream" },
          });
        }

        // For other requests, return empty response
        return new Response("{}", {
          status: 200,
          headers: { "Content-Type": "application/json" },
        });
      });
  });

  afterEach(() => {
    fetchSpy.mockRestore();
    global.fetch = originalFetch;
  });

  it("does not send Authorization header when defaultHeaders overrides it", async () => {
    // Create OpenAI client with Authorization header explicitly set to null
    // (like in Playground component)
    const openai = new OpenAI({
      baseURL: "http://localhost:3000/admin/api/v1/ai/v1",
      apiKey: "", // SDK requires this
      dangerouslyAllowBrowser: true,
      defaultHeaders: {
        // Override Authorization header to prevent it from being sent
        Authorization: null as any,
      },
    });

    // Make a test request
    try {
      await openai.chat.completions.create({
        model: "test-model",
        messages: [{ role: "user", content: "test" }],
        stream: true,
      });
    } catch {
      // It's okay if the request fails, we just want to check the headers
    }

    // Wait for fetch to be called
    expect(fetchSpy).toHaveBeenCalled();

    // Find the OpenAI API call
    const openaiCalls = fetchSpy.mock.calls.filter((call: any) =>
      call[0]?.toString().includes("/chat/completions"),
    );

    expect(openaiCalls.length).toBeGreaterThan(0);

    // Check that the Authorization header is not present
    const [_url, requestInit] = openaiCalls[0] as [
      unknown,
      RequestInit | undefined,
    ];
    const headers = new Headers(requestInit?.headers);

    // The Authorization header should not be present at all (unset)
    expect(headers.has("Authorization")).toBe(false);
    expect(headers.get("Authorization")).toBe(null);

    // Log all headers for debugging
    const allHeaders: Record<string, string> = {};
    headers.forEach((value, key) => {
      allHeaders[key] = value;
    });
    console.log("All headers sent:", JSON.stringify(allHeaders, null, 2));
  });

  it("sends Authorization header when apiKey is provided", async () => {
    // Create OpenAI client WITH apiKey
    const openai = new OpenAI({
      baseURL: "http://localhost:3000/admin/api/v1/ai/v1",
      apiKey: "test-api-key",
      dangerouslyAllowBrowser: true,
    });

    // Make a test request
    try {
      await openai.chat.completions.create({
        model: "test-model",
        messages: [{ role: "user", content: "test" }],
        stream: true,
      });
    } catch {
      // It's okay if the request fails, we just want to check the headers
    }

    // Wait for fetch to be called
    expect(fetchSpy).toHaveBeenCalled();

    // Find the OpenAI API call
    const openaiCalls = fetchSpy.mock.calls.filter((call: any) =>
      call[0]?.toString().includes("/chat/completions"),
    );

    expect(openaiCalls.length).toBeGreaterThan(0);

    // Check that the Authorization header IS present with the API key
    const [_url, requestInit] = openaiCalls[0] as [
      unknown,
      RequestInit | undefined,
    ];
    const headers = new Headers(requestInit?.headers);

    expect(headers.has("Authorization")).toBe(true);
    expect(headers.get("Authorization")).toBe("Bearer test-api-key");
  });
});
