/**
 * Reasoning-model "thinking" can arrive over a chat stream in several shapes,
 * depending on which backend served the request:
 *
 *  - `reasoning_content` — emitted by some inference backends
 *  - `reasoning`         — emitted by others (flattened text)
 *  - `reasoning_details` — emitted alongside `reasoning` by some backends
 *                          (structured; its `text` mirrors `reasoning`)
 *  - `<think>...</think>` — some models emit thinking inline in `content`
 *
 * The Playground previously only read `reasoning_content`, so responses from a
 * backend that uses the other fields showed an empty thinking block — and, when
 * the token budget was spent thinking, an empty answer too. These helpers
 * normalise all of the above into a single content/reasoning pair.
 */

export interface DeltaWithReasoning {
  content?: string | null;
  /** Reasoning channel used by some backends. */
  reasoning_content?: string | null;
  /** Reasoning channel used by other backends (flattened text). */
  reasoning?: string | null;
  /** Structured reasoning; `text` mirrors `reasoning`. */
  reasoning_details?: Array<{ text?: string | null }> | null;
}

const THINK_OPEN = "<think>";
const THINK_CLOSE = "</think>";

/**
 * Reasoning text contributed by a single streaming delta.
 *
 * We pick exactly ONE channel per delta (never summing): some backends send
 * `reasoning` and `reasoning_details` as duplicates of one another in the same
 * chunk, so concatenating them would double the thinking text.
 */
export function reasoningFromDelta(
  delta: DeltaWithReasoning | undefined,
): string {
  if (!delta) return "";
  if (delta.reasoning_content) return delta.reasoning_content;
  if (delta.reasoning) return delta.reasoning;
  if (delta.reasoning_details?.length) {
    return delta.reasoning_details.map((d) => d?.text ?? "").join("");
  }
  return "";
}

/**
 * Split `<think>...</think>` blocks out of (possibly mid-stream) content.
 *
 * A trailing unclosed `<think>` routes the remainder into `thinking`, so the
 * tags never leak into the rendered answer while the model is still thinking.
 */
export function splitThinkTags(content: string): {
  display: string;
  thinking: string;
} {
  if (!content.includes(THINK_OPEN)) return { display: content, thinking: "" };
  let display = "";
  const thinking: string[] = [];
  let rest = content;
  for (;;) {
    const open = rest.indexOf(THINK_OPEN);
    if (open === -1) {
      display += rest;
      break;
    }
    display += rest.slice(0, open);
    const afterOpen = rest.slice(open + THINK_OPEN.length);
    const close = afterOpen.indexOf(THINK_CLOSE);
    if (close === -1) {
      thinking.push(afterOpen); // still streaming inside a think block
      break;
    }
    thinking.push(afterOpen.slice(0, close));
    rest = afterOpen.slice(close + THINK_CLOSE.length);
  }
  return { display, thinking: thinking.join("\n\n") };
}

export interface ReasoningAccumulator {
  rawContent: string;
  fieldReasoning: string;
}

export function createReasoningAccumulator(): ReasoningAccumulator {
  return { rawContent: "", fieldReasoning: "" };
}

export interface AppliedDelta {
  /** content text in THIS delta (for first-token timing) */
  contentDelta: string;
  /** field reasoning text in THIS delta (for first-token timing) */
  reasoningDelta: string;
  /** answer so far, with `<think>` blocks removed */
  content: string;
  /** all reasoning so far (field channels + extracted `<think>` text) */
  reasoning: string;
}

/**
 * Fold one stream delta into the accumulator and return the normalised
 * content/reasoning to display so far.
 */
export function applyDelta(
  acc: ReasoningAccumulator,
  delta: DeltaWithReasoning | undefined,
): AppliedDelta {
  const contentDelta = delta?.content ?? "";
  const reasoningDelta = reasoningFromDelta(delta);
  acc.rawContent += contentDelta;
  acc.fieldReasoning += reasoningDelta;
  return { contentDelta, reasoningDelta, ...finalizeReasoning(acc) };
}

/** Final normalised content/reasoning for a completed stream. */
export function finalizeReasoning(acc: ReasoningAccumulator): {
  content: string;
  reasoning: string;
} {
  const { display, thinking } = splitThinkTags(acc.rawContent);
  const reasoning = [acc.fieldReasoning, thinking].filter(Boolean).join("\n\n");
  return { content: display, reasoning };
}
