import { describe, expect, it } from "vitest";
import {
  applyDelta,
  createReasoningAccumulator,
  finalizeReasoning,
  reasoningFromDelta,
  splitThinkTags,
} from "./reasoning";

describe("reasoningFromDelta", () => {
  it("reads reasoning_content", () => {
    expect(reasoningFromDelta({ reasoning_content: "thinking" })).toBe(
      "thinking",
    );
  });

  it("reads reasoning", () => {
    expect(reasoningFromDelta({ reasoning: "thinking" })).toBe("thinking");
  });

  it("falls back to reasoning_details text when reasoning is absent", () => {
    expect(
      reasoningFromDelta({
        reasoning_details: [{ text: "abc" }, { text: "def" }],
      }),
    ).toBe("abcdef");
  });

  it("does NOT double-count reasoning + reasoning_details when a backend sends both", () => {
    // Some backends emit the same text in both fields on a single chunk.
    expect(
      reasoningFromDelta({
        reasoning: "Thinking",
        reasoning_details: [{ text: "Thinking" }],
      }),
    ).toBe("Thinking");
  });

  it("returns empty string for content-only or undefined deltas", () => {
    expect(reasoningFromDelta({ content: "hi" })).toBe("");
    expect(reasoningFromDelta(undefined)).toBe("");
  });
});

describe("splitThinkTags", () => {
  it("passes through content with no think tags", () => {
    expect(splitThinkTags("hello world")).toEqual({
      display: "hello world",
      thinking: "",
    });
  });

  it("extracts a closed think block", () => {
    expect(splitThinkTags("<think>pondering</think>answer")).toEqual({
      display: "answer",
      thinking: "pondering",
    });
  });

  it("routes a trailing unclosed think block to thinking (still streaming)", () => {
    expect(splitThinkTags("<think>still going")).toEqual({
      display: "",
      thinking: "still going",
    });
  });

  it("handles multiple think blocks", () => {
    expect(splitThinkTags("<think>a</think>X<think>b</think>Y")).toEqual({
      display: "XY",
      thinking: "a\n\nb",
    });
  });
});

describe("applyDelta / finalizeReasoning", () => {
  it("accumulates reasoning then content", () => {
    const acc = createReasoningAccumulator();
    // Many reasoning-only chunks arrive before any content; model with two.
    applyDelta(acc, { content: "", reasoning: "Think" });
    applyDelta(acc, { content: "", reasoning: "ing" });
    const last = applyDelta(acc, { content: "hello world" });

    expect(last.content).toBe("hello world");
    expect(last.reasoning).toBe("Thinking");
    expect(finalizeReasoning(acc)).toEqual({
      content: "hello world",
      reasoning: "Thinking",
    });
  });

  it("captures reasoning even when the budget is spent thinking (no content)", () => {
    const acc = createReasoningAccumulator();
    applyDelta(acc, { reasoning: "still " });
    applyDelta(acc, { reasoning: "thinking" });

    expect(finalizeReasoning(acc)).toEqual({
      content: "",
      reasoning: "still thinking",
    });
  });

  it("accumulates internal reasoning_content", () => {
    const acc = createReasoningAccumulator();
    applyDelta(acc, { reasoning_content: "step 1 " });
    applyDelta(acc, { reasoning_content: "step 2" });
    const last = applyDelta(acc, { content: "done" });

    expect(last.content).toBe("done");
    expect(last.reasoning).toBe("step 1 step 2");
  });

  it("strips inline <think> tags from content into reasoning", () => {
    const acc = createReasoningAccumulator();
    applyDelta(acc, { content: "<think>hmm" });
    applyDelta(acc, { content: "mm</think>" });
    const last = applyDelta(acc, { content: "the answer" });

    expect(last.content).toBe("the answer");
    expect(last.reasoning).toBe("hmmmm");
  });

  it("reports per-delta content/reasoning for first-token timing", () => {
    const acc = createReasoningAccumulator();
    const r = applyDelta(acc, { reasoning: "x" });
    expect(r.reasoningDelta).toBe("x");
    expect(r.contentDelta).toBe("");

    const c = applyDelta(acc, { content: "y" });
    expect(c.contentDelta).toBe("y");
    expect(c.reasoningDelta).toBe("");
  });
});
