import { describe, expect, it } from "vitest";
import type { ReasoningTranslation } from "../../../api/control-layer/types";
import {
  REASONING_EFFORTS,
  buildBinaryTranslation,
  buildNativeTranslation,
  buildTokenBudgetTranslation,
  buildUpstreamPreview,
  inferReasoningStrategy,
  normalizeReasoningTranslationOverrides,
  validateReasoningTranslation,
} from "./reasoningTranslation";

const reject = { mode: "reject" as const };

describe("reasoning translation helpers", () => {
  it("exports the seven canonical efforts", () => {
    expect(REASONING_EFFORTS).toEqual([
      "none",
      "minimal",
      "low",
      "medium",
      "high",
      "xhigh",
      "max",
    ]);
  });

  it("infers exact native, token-budget, binary, and custom shapes", () => {
    expect(
      inferReasoningStrategy({
        unsupported_efforts: ["none"],
        writes: [{ target_path: "/reasoning_effort", values: { low: "low" } }],
      }),
    ).toBe("native");
    expect(
      inferReasoningStrategy({
        unsupported_efforts: ["none"],
        writes: [
          { target_path: "/thinking_token_budget", values: { low: 1024 } },
          { target_path: "/reasoning_effort", values: { low: "low" } },
        ],
      }),
    ).toBe("token_budget");
    expect(
      inferReasoningStrategy({
        unsupported_efforts: ["none"],
        writes: [{ target_path: "/thinking", values: { low: true } }],
      }),
    ).toBe("binary");
    expect(
      inferReasoningStrategy({
        unsupported_efforts: ["none"],
        writes: [
          { target_path: "/a", values: { low: "low" } },
          { target_path: "/b", values: { low: true } },
        ],
      }),
    ).toBe("custom");
  });

  it("infers token budget only when both writes can round-trip graphically", () => {
    const translation = (
      effort: unknown,
      budget: unknown,
    ): ReasoningTranslation => ({
      unsupported_efforts: ["none", "minimal", "medium", "high", "xhigh", "max"],
      writes: [
        { target_path: "/reasoning_effort", values: { low: effort } },
        { target_path: "/thinking_token_budget", values: { low: budget } },
      ],
    });

    expect(inferReasoningStrategy(translation("low", 1024))).toBe("token_budget");
    expect(inferReasoningStrategy(translation({ level: 1 }, 1024))).toBe("custom");
    expect(inferReasoningStrategy(translation("low", -1))).toBe("custom");
    expect(inferReasoningStrategy(translation("low", 1.5))).toBe("custom");
    expect(
      inferReasoningStrategy(translation("low", Number.MAX_SAFE_INTEGER + 1)),
    ).toBe("custom");
  });

  it("builds native mappings and moves every rejected effort out of the write", () => {
    const decisions = Object.fromEntries(
      REASONING_EFFORTS.map((effort) => [
        effort,
        effort === "none" || effort === "max"
          ? reject
          : { mode: "map" as const, value: `provider-${effort}` },
      ]),
    );

    const translation = buildNativeTranslation("/reasoning_effort", decisions);

    expect(translation.unsupported_efforts).toEqual(["none", "max"]);
    expect(translation.writes).toEqual([
      {
        target_path: "/reasoning_effort",
        values: {
          minimal: "provider-minimal",
          low: "provider-low",
          medium: "provider-medium",
          high: "provider-high",
          xhigh: "provider-xhigh",
        },
      },
    ]);
  });

  it("builds token budgets with both required writes and identical mapped keys", () => {
    const decisions = Object.fromEntries(
      REASONING_EFFORTS.map((effort) => [
        effort,
        effort === "high"
          ? { mode: "map" as const, effort: "high", budget: 8192 }
          : reject,
      ]),
    );

    expect(buildTokenBudgetTranslation(decisions)).toEqual({
      unsupported_efforts: ["none", "minimal", "low", "medium", "xhigh", "max"],
      writes: [
        { target_path: "/reasoning_effort", values: { high: "high" } },
        { target_path: "/thinking_token_budget", values: { high: 8192 } },
      ],
    });
  });

  it("builds explicit binary On, Off, and Reject decisions", () => {
    const decisions = Object.fromEntries(
      REASONING_EFFORTS.map((effort) => [
        effort,
        effort === "none" ? "off" : effort === "max" ? "reject" : "on",
      ]),
    );

    expect(
      buildBinaryTranslation("/chat_template_kwargs/thinking", decisions),
    ).toEqual({
      unsupported_efforts: ["max"],
      writes: [
        {
          target_path: "/chat_template_kwargs/thinking",
          values: {
            none: false,
            minimal: true,
            low: true,
            medium: true,
            high: true,
            xhigh: true,
          },
        },
      ],
    });
  });

  it("validates complete accounting, shared keys, paths, and integer budgets", () => {
    const invalid = validateReasoningTranslation({
      unsupported_efforts: ["none"],
      writes: [
        { target_path: "", values: { low: "low", medium: "medium" } },
        { target_path: "/thinking_token_budget", values: { low: -1.5 } },
      ],
    });

    expect(invalid.valid).toBe(false);
    expect(invalid.errors.join(" ")).toMatch(/all seven efforts/i);
    expect(invalid.errors.join(" ")).toMatch(/same mapped efforts/i);
    expect(invalid.errors.join(" ")).toMatch(/target path/i);
    expect(invalid.errors.join(" ")).toMatch(/non-negative.*integer/i);
  });

  it("rejects translations with no mapped effort", () => {
    expect(
      validateReasoningTranslation({
        unsupported_efforts: [...REASONING_EFFORTS],
        writes: [{ target_path: "/thinking", values: {} }],
      }),
    ).toEqual(
      expect.objectContaining({
        valid: false,
        errors: expect.arrayContaining([expect.stringMatching(/at least one effort/i)]),
      }),
    );
  });

  it("rejects target paths that Onwards cannot apply", () => {
    const unsupported = ["none", "minimal", "medium", "high", "xhigh", "max"] as const;
    const validateWrites = (writes: Array<{ target_path: string; values: { low: unknown } }>) =>
      validateReasoningTranslation({ unsupported_efforts: unsupported, writes });

    expect(
      validateWrites([{ target_path: "thinking", values: { low: true } }]).errors.join(" "),
    ).toMatch(/absolute JSON pointer/i);
    expect(
      validateWrites([{ target_path: "/messages/0/content", values: { low: true } }]).errors.join(" "),
    ).toMatch(/reasoning-related/i);
    expect(
      validateWrites([
        { target_path: "/thinking", values: { low: true } },
        { target_path: "/thinking", values: { low: false } },
      ]).errors.join(" "),
    ).toMatch(/unique/i);
    expect(
      validateWrites([
        { target_path: "/thinking", values: { low: true } },
        { target_path: "/thinking/type", values: { low: "enabled" } },
      ]).errors.join(" "),
    ).toMatch(/must not overlap/i);
    expect(
      validateWrites([
        { target_path: "/thinking_token_budget", values: { low: 1024 } },
      ]).errors.join(" "),
    ).toMatch(/requires.*reasoning_effort/i);
  });

  it("requires custom token budgets to be non-negative safe integers", () => {
    const translation = (budget: number) => ({
      unsupported_efforts: ["none", "minimal", "medium", "high", "xhigh", "max"],
      writes: [
        { target_path: "/reasoning_effort", values: { low: "low" } },
        { target_path: "/thinking_token_budget", values: { low: budget } },
      ],
    });

    expect(validateReasoningTranslation(translation(Number.MAX_SAFE_INTEGER)).valid).toBe(
      true,
    );
    expect(
      validateReasoningTranslation(translation(Number.MAX_SAFE_INTEGER + 1)),
    ).toEqual(
      expect.objectContaining({
        valid: false,
        errors: expect.arrayContaining([
          expect.stringMatching(/safe integers/i),
        ]),
      }),
    );
  });

  it("normalizes an all-inherit model overlay to null", () => {
    expect(
      normalizeReasoningTranslationOverrides({
        chat_completions: { mode: "inherit" },
        responses: { mode: "inherit" },
      }),
    ).toBeNull();
  });

  it("combines every write into the selected effort preview", () => {
    expect(
      buildUpstreamPreview(
        {
          unsupported_efforts: ["none", "minimal", "medium", "high", "xhigh", "max"],
          writes: [
            { target_path: "/reasoning_effort", values: { low: "low" } },
            { target_path: "/thinking_token_budget", values: { low: 2048 } },
          ],
        },
        "low",
      ),
    ).toEqual({ reasoning_effort: "low", thinking_token_budget: 2048 });
  });

  it("builds literal previews for prototype-named paths without prototype pollution", () => {
    const pollutionKey = "reasoningPreviewPolluted";
    const prototype = Object.prototype as Record<string, unknown>;
    delete prototype[pollutionKey];

    try {
      const preview = buildUpstreamPreview(
        {
          unsupported_efforts: ["none", "minimal", "medium", "high", "xhigh", "max"],
          writes: [
            {
              target_path: `/reasoning/__proto__/${pollutionKey}`,
              values: { low: "yes" },
            },
            {
              target_path: "/thinking/constructor/prototype/providerFlag",
              values: { low: true },
            },
          ],
        },
        "low",
      );

      expect(Object.prototype.hasOwnProperty.call(prototype, pollutionKey)).toBe(false);
      expect(preview).toEqual(
        JSON.parse(
          `{"reasoning":{"__proto__":{"${pollutionKey}":"yes"}},"thinking":{"constructor":{"prototype":{"providerFlag":true}}}}`,
        ),
      );
    } finally {
      delete prototype[pollutionKey];
    }
  });
});
