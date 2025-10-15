import type {
  RequestResponsePair,
  RequestsAggregateResponse,
} from "../waycast/types";
import demoRequests from "./data/requests.json";
import type { Request as DemoRequest } from "./types";

/**
 * Transform demo request data to match backend RequestResponsePair format
 */
function transformDemoToBackendFormat(
  demoRequest: DemoRequest,
  index: number,
): RequestResponsePair {
  // Make some requests streaming to test the streaming functionality
  const isStreaming = index % 4 === 0; // Every 4th request is streaming
  const hasUsage = !isStreaming || index % 8 === 0; // Streaming requests sometimes lack usage

  const responseBody = isStreaming
    ? {
        type: "chat_completions_stream" as const,
        data: [
          {
            id: `chatcmpl-${demoRequest.id}`,
            object: "chat.completion.chunk",
            created: demoRequest.response.created,
            model: demoRequest.response.model,
            choices: [
              {
                index: 0,
                delta: {
                  content:
                    demoRequest.response.choices?.[0]?.message?.content || "",
                },
                finish_reason: undefined,
              },
            ],
          },
          ...(hasUsage
            ? [
                {
                  id: `chatcmpl-${demoRequest.id}`,
                  object: "chat.completion.chunk",
                  created: demoRequest.response.created,
                  model: demoRequest.response.model,
                  choices: [
                    {
                      index: 0,
                      delta: {},
                      finish_reason: "stop",
                    },
                  ],
                  usage: demoRequest.response.usage,
                },
              ]
            : []),
        ],
      }
    : {
        type: "chat_completions" as const,
        data: {
          id: `chatcmpl-${demoRequest.id}`,
          object: "chat.completion",
          created: demoRequest.response.created,
          model: demoRequest.response.model,
          choices: (demoRequest.response.choices || []).map((choice) => ({
            ...choice,
            message: {
              role: choice.message.role as "system" | "user" | "assistant",
              content: choice.message.content,
            },
          })),
          usage: demoRequest.response.usage,
        },
      };

  return {
    request: {
      id: parseInt(demoRequest.id.replace("req_", ""), 10),
      timestamp: demoRequest.timestamp,
      method: "POST",
      uri: "/ai/v1/chat/completions",
      headers: {
        "content-type": "application/json",
        authorization: "Bearer ***",
      },
      body: {
        type: "chat_completions",
        data: {
          model: demoRequest.model,
          messages: (demoRequest.request.messages || []).map((msg) => ({
            role: msg.role as "system" | "user" | "assistant",
            content: msg.content,
          })),
          temperature: demoRequest.request.temperature,
          max_completion_tokens: demoRequest.request.max_completion_tokens,
          stream: isStreaming,
        },
      },
      created_at: demoRequest.timestamp,
    },
    response: {
      id: parseInt(demoRequest.id.replace("req_", ""), 10) + 1000,
      timestamp: new Date(
        new Date(demoRequest.timestamp).getTime() + demoRequest.duration_ms,
      ).toISOString(),
      status_code: 200,
      headers: {
        "content-type": "application/json",
      },
      body: responseBody,
      duration_ms: demoRequest.duration_ms,
      created_at: new Date(
        new Date(demoRequest.timestamp).getTime() + demoRequest.duration_ms,
      ).toISOString(),
    },
  };
}

/**
 * Shift timestamps to make demo data appear recent (within last 24 hours)
 */
function shiftTimestampsToRecent(requests: DemoRequest[]): {
  requests: DemoRequest[];
  timeShift: number;
} {
  if (requests.length === 0) {
    return { requests, timeShift: 0 };
  }

  const now = new Date();
  const originalTimestamps = requests.map((r) => new Date(r.timestamp));
  const originalMax = Math.max(...originalTimestamps.map((d) => d.getTime()));

  // Calculate time shift to place most recent request within last hour
  const oneHourAgo = now.getTime() - 60 * 60 * 1000;
  const timeShift = oneHourAgo - originalMax;

  // Shift all timestamps
  const shiftedRequests = requests.map((req) => ({
    ...req,
    timestamp: new Date(
      new Date(req.timestamp).getTime() + timeShift,
    ).toISOString(),
  }));

  return { requests: shiftedRequests, timeShift };
}

/**
 * Get mock request-response pairs that match the backend format
 * Timestamps are shifted to appear as recent activity (within last 24 hours)
 */
export function getMockRequestResponsePairs(): RequestResponsePair[] {
  const { requests: shiftedRequests } = shiftTimestampsToRecent(
    demoRequests as DemoRequest[],
  );
  return shiftedRequests.map((demoRequest, index) =>
    transformDemoToBackendFormat(demoRequest, index),
  );
}

/**
 * Mock hook that returns request data in backend format for demo mode
 */
export function useMockRequests(
  query?: {
    limit?: number;
    order_desc?: boolean;
  },
  options?: { enabled?: boolean },
  dateRange?: { from: Date; to: Date },
) {
  // If disabled, return null data
  if (options?.enabled === false) {
    return {
      data: null,
      isLoading: false,
      error: null,
    };
  }

  let pairs = getMockRequestResponsePairs();

  // Apply date filtering if provided
  if (dateRange) {
    pairs = pairs.filter((pair) => {
      const timestamp = new Date(pair.request.timestamp);
      return timestamp >= dateRange.from && timestamp <= dateRange.to;
    });
  }

  // Apply sorting if requested
  let sortedPairs = pairs;
  if (query?.order_desc !== false) {
    sortedPairs = [...pairs].sort(
      (a, b) =>
        new Date(b.request.timestamp).getTime() -
        new Date(a.request.timestamp).getTime(),
    );
  }

  // Apply limit if requested
  if (query?.limit) {
    sortedPairs = sortedPairs.slice(0, query.limit);
  }

  return {
    data: { requests: sortedPairs },
    isLoading: false,
    error: null,
  };
}

/**
 * Generate mock aggregation data from demo requests
 */
export function getMockAggregateData(
  model?: string,
  dateRange?: { from: Date; to: Date },
): RequestsAggregateResponse {
  let pairs = getMockRequestResponsePairs();

  // Apply date filtering if provided
  if (dateRange) {
    pairs = pairs.filter((pair) => {
      const timestamp = new Date(pair.request.timestamp);
      return timestamp >= dateRange.from && timestamp <= dateRange.to;
    });
  }

  // Filter by model if specified
  const filteredPairs = model
    ? pairs.filter((pair) => pair.request.body?.data?.model === model)
    : pairs;

  const totalRequests = filteredPairs.length;

  // Generate status code breakdown
  const statusCounts = new Map<string, number>();
  filteredPairs.forEach((pair) => {
    const status = pair.response?.status_code.toString() || "error";
    statusCounts.set(status, (statusCounts.get(status) || 0) + 1);
  });

  const statusCodes = Array.from(statusCounts.entries()).map(
    ([status, count]) => ({
      status,
      count,
      percentage: totalRequests > 0 ? (count / totalRequests) * 100 : 0,
    }),
  );

  // Generate model usage (only if not filtering by specific model)
  let models = undefined;
  if (!model) {
    const modelCounts = new Map<
      string,
      { count: number; totalLatency: number }
    >();
    filteredPairs.forEach((pair) => {
      const requestModel = pair.request.body?.data?.model || "unknown";
      const latency = pair.response?.duration_ms || 0;
      const current = modelCounts.get(requestModel) || {
        count: 0,
        totalLatency: 0,
      };
      modelCounts.set(requestModel, {
        count: current.count + 1,
        totalLatency: current.totalLatency + latency,
      });
    });

    models = Array.from(modelCounts.entries()).map(([modelName, data]) => ({
      model: modelName,
      count: data.count,
      percentage: totalRequests > 0 ? (data.count / totalRequests) * 100 : 0,
      avg_latency_ms: data.count > 0 ? data.totalLatency / data.count : 0,
    }));
  }

  // Generate time series data
  const now = new Date();
  const timeSeries = [];

  // Create realistic traffic pattern (deterministic, no random)
  const trafficPattern = [
    0.2,
    0.1,
    0.1,
    0.1,
    0.2,
    0.3,
    0.4,
    0.6, // Night hours (0-7)
    0.8,
    1.2,
    1.5,
    1.8,
    2.0,
    1.9,
    1.7,
    1.6, // Business hours (8-15)
    1.4,
    1.2,
    1.0,
    0.8,
    0.6,
    0.5,
    0.4,
    0.3,
    0.2, // Evening hours (16-24)
  ];

  // Calculate base metrics for scaling
  const totalDataRequests = filteredPairs.length;
  const avgRequestsPerHour = Math.max(1, Math.floor(totalDataRequests / 25));

  // Create 25 hourly buckets for the last 24 hours + current hour
  for (let i = 24; i >= 0; i--) {
    const timestamp = new Date(now.getTime() - i * 60 * 60 * 1000);
    timestamp.setMinutes(0, 0, 0); // Round to hour

    const bucketIndex = 24 - i;
    const patternMultiplier = trafficPattern[bucketIndex];

    // Scale requests based on traffic pattern
    const requests = Math.max(
      1,
      Math.floor(avgRequestsPerHour * patternMultiplier),
    );

    // Use actual data samples but scale to match request count
    let inputTokens = 0;
    let outputTokens = 0;
    let totalLatency = 0;
    const latencies: number[] = [];

    // Sample from actual data, cycling through to match request count
    for (let j = 0; j < requests; j++) {
      const pairIndex = (bucketIndex * 17 + j) % filteredPairs.length; // Use prime offset for better distribution
      const pair = filteredPairs[pairIndex];

      const usage = pair?.response?.body?.data?.usage;
      if (usage) {
        inputTokens += usage.prompt_tokens || 0;
        outputTokens += usage.completion_tokens || 0;
      }

      const latency = pair?.response?.duration_ms || 850;
      totalLatency += latency;
      latencies.push(latency);
    }

    // Calculate percentiles
    latencies.sort((a, b) => a - b);
    const p95_index = Math.floor(latencies.length * 0.95);
    const p99_index = Math.floor(latencies.length * 0.99);

    timeSeries.push({
      timestamp: timestamp.toISOString(),
      requests,
      input_tokens: inputTokens,
      output_tokens: outputTokens,
      avg_latency_ms:
        requests > 0 && model ? Math.round(totalLatency / requests) : undefined,
      p95_latency_ms:
        latencies.length > 0 && model ? latencies[p95_index] : undefined,
      p99_latency_ms:
        latencies.length > 0 && model ? latencies[p99_index] : undefined,
    });
  }

  return {
    total_requests: totalRequests,
    model,
    status_codes: statusCodes,
    models,
    time_series: timeSeries,
  };
}

/**
 * Mock hook for aggregation data
 */
export function useMockAggregateData(
  model?: string,
  dateRange?: { from: Date; to: Date },
) {
  return {
    data: getMockAggregateData(model, dateRange),
    isLoading: false,
    error: null,
  };
}
