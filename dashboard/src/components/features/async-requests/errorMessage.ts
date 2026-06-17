// Pull a human-readable message out of a failed request. `response_body` is the
// raw upstream/gateway body (populated for realtime failures); `error` is the
// canonical FailureReason envelope (always set on failed rows, and the only
// source for batch failures, which leave response_body null). Prefer the
// cleanest available string, mirroring the server's parse_failure_error.
export function extractErrorMessage(request: {
  response_body?: string | null;
  error?: string | null;
}): string {
  const fromBody = (body: string | null | undefined): string | null => {
    if (!body) return null;
    try {
      const parsed = JSON.parse(body);
      // OpenAI-style error envelope: {"error": {"message": "..."}}
      if (parsed?.error?.message) return String(parsed.error.message);
    } catch {
      // Not JSON - the body itself is the message (e.g. a plain-text 402).
    }
    return body;
  };

  // Realtime failures carry the clean body here.
  const bodyMessage = fromBody(request.response_body);
  if (bodyMessage) return bodyMessage;

  // Fall back to `error`. This mirrors the server's parse_failure_error, which
  // accepts several shapes:
  if (request.error) {
    // 1. FailureReason envelope: {"type": ..., "details": {"status", "body"}}
    try {
      const reason = JSON.parse(request.error);
      const inner = fromBody(reason?.details?.body);
      if (inner) return inner;
    } catch {
      // Not JSON - fall through.
    }
    // 2. Legacy "Upstream returned {status}: {body}".
    const legacy = request.error.match(/^Upstream returned \d+: ([\s\S]+)$/);
    if (legacy) return fromBody(legacy[1]) ?? legacy[1];
    // 3. Raw OpenAI envelope or plain string.
    return fromBody(request.error) ?? request.error;
  }

  return "Request failed";
}
