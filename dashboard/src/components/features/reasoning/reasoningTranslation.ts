import type {
  ReasoningEffort,
  ReasoningTranslation,
  ReasoningTranslationOverrides,
  ReasoningWrite,
} from "../../../api/control-layer/types";

export const REASONING_EFFORTS: readonly ReasoningEffort[] = [
  "none",
  "minimal",
  "low",
  "medium",
  "high",
  "xhigh",
  "max",
];

export type ReasoningStrategy =
  | "native"
  | "token_budget"
  | "binary"
  | "custom";

export type NativeEffortDecision =
  | { mode: "map"; value: string }
  | { mode: "reject" };

export type TokenBudgetEffortDecision =
  | { mode: "map"; effort: string; budget: number }
  | { mode: "reject" };

export type BinaryEffortDecision = "on" | "off" | "reject";

export interface TranslationValidationResult {
  valid: boolean;
  errors: string[];
}

const effortSet = new Set<string>(REASONING_EFFORTS);
const MAX_TARGET_DEPTH = 8;
const MAX_VALUE_BYTES = 8192;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function sortedEffortKeys(values: unknown): string[] {
  if (!isRecord(values)) return [];
  return Object.keys(values).filter((key) => effortSet.has(key)).sort();
}

function sameKeys(left: string[], right: string[]) {
  return (
    left.length === right.length && left.every((key, index) => key === right[index])
  );
}

function validateTargetPath(path: string): string | null {
  if (!path.startsWith("/") || path.length === 1) {
    return "Target path must be an absolute JSON pointer.";
  }
  const segments = path.slice(1).split("/");
  if (
    segments.some(
      (segment) => !segment || !/^[A-Za-z0-9_-]+$/.test(segment),
    )
  ) {
    return "Target path segments may only contain letters, numbers, underscores, and hyphens.";
  }
  if (segments.length > MAX_TARGET_DEPTH) {
    return "Target path must not exceed 8 path segments.";
  }

  const [root] = segments;
  const allowed =
    ((root === "reasoning_effort" || root === "thinking_token_budget") &&
      segments.length === 1) ||
    root === "reasoning" ||
    root === "thinking" ||
    (root === "chat_template_kwargs" &&
      segments.length === 2 &&
      (segments[1] === "thinking" || segments[1] === "enable_thinking"));
  return allowed
    ? null
    : "Target path must address a reasoning-related request field.";
}

function targetPathsOverlap(left: string, right: string) {
  return right.startsWith(`${left}/`) || left.startsWith(`${right}/`);
}

function jsonDepth(value: unknown): number {
  if (Array.isArray(value)) {
    return 1 + Math.max(0, ...value.map(jsonDepth));
  }
  if (isRecord(value)) {
    return 1 + Math.max(0, ...Object.values(value).map(jsonDepth));
  }
  return 1;
}

function validateMappedValue(value: unknown): string | null {
  let serialized: string | undefined;
  try {
    serialized = JSON.stringify(value);
  } catch {
    return "Mapped values must be valid JSON.";
  }
  if (serialized === undefined) return "Mapped values must be valid JSON.";
  if (new TextEncoder().encode(serialized).length > MAX_VALUE_BYTES) {
    return "Mapped values must not exceed 8192 bytes.";
  }
  if (jsonDepth(value) > 8) {
    return "Mapped values must not exceed 8 levels.";
  }
  return null;
}

export function getMappedEfforts(
  translation: ReasoningTranslation,
): ReasoningEffort[] {
  const values = translation.writes[0]?.values ?? {};
  return REASONING_EFFORTS.filter((effort) =>
    Object.prototype.hasOwnProperty.call(values, effort),
  );
}

export function inferReasoningStrategy(
  translation: ReasoningTranslation,
): ReasoningStrategy {
  if (translation.writes.length === 2) {
    const effortWrite = translation.writes.find(
      (write) => write.target_path === "/reasoning_effort",
    );
    const budgetWrite = translation.writes.find(
      (write) => write.target_path === "/thinking_token_budget",
    );
    if (
      effortWrite &&
      budgetWrite &&
      sameKeys(
        sortedEffortKeys(effortWrite.values),
        sortedEffortKeys(budgetWrite.values),
      )
    ) {
      return "token_budget";
    }
  }

  if (translation.writes.length === 1) {
    const mappedValues = Object.values(translation.writes[0].values);
    if (mappedValues.length > 0 && mappedValues.every((value) => typeof value === "boolean")) {
      return "binary";
    }
    if (mappedValues.length > 0 && mappedValues.every((value) => typeof value === "string")) {
      return "native";
    }
  }

  return "custom";
}

function rejectedEfforts<T extends { mode: string } | string>(
  decisions: Partial<Record<ReasoningEffort, T>>,
  isRejected: (decision: T | undefined) => boolean,
) {
  return REASONING_EFFORTS.filter((effort) => isRejected(decisions[effort]));
}

export function buildNativeTranslation(
  targetPath: string,
  decisions: Partial<Record<ReasoningEffort, NativeEffortDecision>>,
): ReasoningTranslation {
  const values: Partial<Record<ReasoningEffort, unknown>> = {};
  for (const effort of REASONING_EFFORTS) {
    const decision = decisions[effort];
    if (decision?.mode === "map") values[effort] = decision.value;
  }
  return {
    unsupported_efforts: rejectedEfforts(
      decisions,
      (decision) => !decision || decision.mode === "reject",
    ),
    writes: [{ target_path: targetPath, values }],
  };
}

export function buildTokenBudgetTranslation(
  decisions: Partial<Record<ReasoningEffort, TokenBudgetEffortDecision>>,
): ReasoningTranslation {
  const efforts: Partial<Record<ReasoningEffort, unknown>> = {};
  const budgets: Partial<Record<ReasoningEffort, unknown>> = {};
  for (const effort of REASONING_EFFORTS) {
    const decision = decisions[effort];
    if (decision?.mode === "map") {
      efforts[effort] = decision.effort;
      budgets[effort] = decision.budget;
    }
  }
  return {
    unsupported_efforts: rejectedEfforts(
      decisions,
      (decision) => !decision || decision.mode === "reject",
    ),
    writes: [
      { target_path: "/reasoning_effort", values: efforts },
      { target_path: "/thinking_token_budget", values: budgets },
    ],
  };
}

export function buildBinaryTranslation(
  targetPath: string,
  decisions: Partial<Record<ReasoningEffort, BinaryEffortDecision>>,
): ReasoningTranslation {
  const values: Partial<Record<ReasoningEffort, unknown>> = {};
  for (const effort of REASONING_EFFORTS) {
    const decision = decisions[effort];
    if (decision === "on") values[effort] = true;
    if (decision === "off") values[effort] = false;
  }
  return {
    unsupported_efforts: REASONING_EFFORTS.filter(
      (effort) => !decisions[effort] || decisions[effort] === "reject",
    ),
    writes: [{ target_path: targetPath, values }],
  };
}

export function validateReasoningTranslation(
  value: unknown,
): TranslationValidationResult {
  const errors: string[] = [];
  if (!isRecord(value)) {
    return { valid: false, errors: ["Translation must be a JSON object."] };
  }

  const unsupported = value.unsupported_efforts;
  const writes = value.writes;
  if (!Array.isArray(unsupported)) {
    errors.push("unsupported_efforts must be an array.");
  }
  if (!Array.isArray(writes) || writes.length === 0) {
    errors.push("At least one write is required.");
  }

  const unsupportedKeys = Array.isArray(unsupported)
    ? unsupported.filter((effort): effort is string => typeof effort === "string")
    : [];
  if (
    unsupportedKeys.length !== (Array.isArray(unsupported) ? unsupported.length : 0) ||
    unsupportedKeys.some((effort) => !effortSet.has(effort)) ||
    new Set(unsupportedKeys).size !== unsupportedKeys.length
  ) {
    errors.push("unsupported_efforts must contain unique canonical efforts.");
  }

  const writeList = Array.isArray(writes) ? writes : [];
  const writeKeys: string[][] = [];
  const targetPaths: string[] = [];
  let hasReasoningEffort = false;
  let hasThinkingTokenBudget = false;
  for (const [index, write] of writeList.entries()) {
    if (!isRecord(write)) {
      errors.push(`Write ${index + 1} must be an object.`);
      writeKeys.push([]);
      continue;
    }
    if (typeof write.target_path !== "string" || write.target_path.trim() === "") {
      errors.push(`Write ${index + 1} must include a target path.`);
    } else {
      const targetPath = write.target_path;
      const pathError = validateTargetPath(targetPath);
      if (pathError) errors.push(pathError);
      if (targetPaths.includes(targetPath)) {
        errors.push("Target paths must be unique.");
      } else if (
        targetPaths.some((path) => targetPathsOverlap(path, targetPath))
      ) {
        errors.push("Target paths must not overlap.");
      }
      targetPaths.push(targetPath);
      hasReasoningEffort ||= targetPath === "/reasoning_effort";
      hasThinkingTokenBudget ||=
        targetPath === "/thinking_token_budget";
    }
    if (!isRecord(write.values)) {
      errors.push(`Write ${index + 1} values must be an object.`);
      writeKeys.push([]);
      continue;
    }
    const keys = Object.keys(write.values);
    if (keys.some((effort) => !effortSet.has(effort))) {
      errors.push(`Write ${index + 1} contains an unknown effort.`);
    }
    writeKeys.push(keys.filter((effort) => effortSet.has(effort)).sort());

    if (write.target_path === "/thinking_token_budget") {
      for (const budget of Object.values(write.values)) {
        if (typeof budget !== "number" || !Number.isInteger(budget) || budget < 0) {
          errors.push("Token budget values must be non-negative integers.");
          break;
        }
      }
    }
    for (const mappedValue of Object.values(write.values)) {
      const valueError = validateMappedValue(mappedValue);
      if (valueError) {
        errors.push(valueError);
        break;
      }
    }
  }

  if (hasThinkingTokenBudget && !hasReasoningEffort) {
    errors.push("thinking_token_budget requires a reasoning_effort write.");
  }

  const mapped = writeKeys[0] ?? [];
  if (mapped.length === 0) {
    errors.push("At least one effort must be mapped.");
  }
  if (writeKeys.some((keys) => !sameKeys(keys, mapped))) {
    errors.push("All writes must contain the same mapped efforts.");
  }

  const unsupportedSet = new Set(unsupportedKeys);
  const overlaps = mapped.some((effort) => unsupportedSet.has(effort));
  const accounted = new Set([...unsupportedKeys, ...mapped]);
  if (overlaps || REASONING_EFFORTS.some((effort) => !accounted.has(effort))) {
    errors.push("All seven efforts must be accounted for exactly once.");
  }

  return { valid: errors.length === 0, errors };
}

export function normalizeReasoningTranslationOverrides(
  value: ReasoningTranslationOverrides | null,
): ReasoningTranslationOverrides | null {
  if (
    !value ||
    (value.chat_completions.mode === "inherit" &&
      value.responses.mode === "inherit")
  ) {
    return null;
  }
  return value;
}

function decodePointerSegment(segment: string) {
  return segment.replace(/~1/g, "/").replace(/~0/g, "~");
}

function writePreviewValue(
  root: Record<string, unknown>,
  write: ReasoningWrite,
  effort: ReasoningEffort,
) {
  if (!Object.prototype.hasOwnProperty.call(write.values, effort)) return;
  const segments = write.target_path
    .split("/")
    .slice(1)
    .map(decodePointerSegment)
    .filter(Boolean);
  if (segments.length === 0) return;

  let cursor = root;
  for (const segment of segments.slice(0, -1)) {
    const existing = cursor[segment];
    if (!isRecord(existing)) cursor[segment] = {};
    cursor = cursor[segment] as Record<string, unknown>;
  }
  cursor[segments.at(-1)!] = write.values[effort];
}

export function buildUpstreamPreview(
  translation: ReasoningTranslation,
  effort: ReasoningEffort,
): Record<string, unknown> {
  const preview: Record<string, unknown> = {};
  for (const write of translation.writes) {
    writePreviewValue(preview, write, effort);
  }
  return preview;
}
