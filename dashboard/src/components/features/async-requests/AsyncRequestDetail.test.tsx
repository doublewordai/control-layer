import { describe, it, expect } from "vitest";
import { extractErrorMessage } from "./AsyncRequestDetail";

describe("extractErrorMessage", () => {
  it("returns a plain-text response_body as-is", () => {
    expect(
      extractErrorMessage({
        response_body: "Account balance too low. Please add credits to continue.",
      }),
    ).toBe("Account balance too low. Please add credits to continue.");
  });

  it("unwraps an OpenAI error envelope in response_body", () => {
    expect(
      extractErrorMessage({
        response_body: '{"error":{"message":"An internal error occurred."}}',
      }),
    ).toBe("An internal error occurred.");
  });

  it("unwraps a FailureReason envelope's details.body", () => {
    const error = JSON.stringify({
      type: "NonRetriableHttpStatus",
      details: { status: 402, body: "no credits" },
    });
    expect(extractErrorMessage({ error })).toBe("no credits");
  });

  it("unwraps an OpenAI envelope nested inside a FailureReason details.body", () => {
    const error = JSON.stringify({
      type: "NonRetriableHttpStatus",
      details: { status: 503, body: '{"error":{"message":"upstream down"}}' },
    });
    expect(extractErrorMessage({ error })).toBe("upstream down");
  });

  it("unwraps a raw OpenAI envelope stored in error", () => {
    expect(
      extractErrorMessage({ error: '{"error":{"message":"bad api key"}}' }),
    ).toBe("bad api key");
  });

  it("handles the legacy 'Upstream returned {status}: {body}' string", () => {
    expect(
      extractErrorMessage({ error: "Upstream returned 502: gateway boom" }),
    ).toBe("gateway boom");
  });

  it("prefers response_body over error", () => {
    expect(
      extractErrorMessage({ response_body: "body wins", error: "ignored" }),
    ).toBe("body wins");
  });

  it("falls through an empty response_body to the error", () => {
    const error = JSON.stringify({
      type: "NonRetriableHttpStatus",
      details: { status: 500, body: "real error" },
    });
    expect(extractErrorMessage({ response_body: "", error })).toBe("real error");
  });

  it("falls back to a default when nothing is present", () => {
    expect(extractErrorMessage({})).toBe("Request failed");
  });
});
