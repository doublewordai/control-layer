import { Braces } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type {
  ReasoningEffort,
  ReasoningSurfaceOverride,
  ReasoningTranslation,
  ReasoningTranslationConfig,
  ReasoningTranslationOverrides,
} from "../../../api/control-layer/types";
import { Button } from "../../ui/button";
import { Input } from "../../ui/input";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "../../ui/tabs";
import { Textarea } from "../../ui/textarea";
import {
  REASONING_EFFORTS,
  buildBinaryTranslation,
  buildNativeTranslation,
  buildTokenBudgetTranslation,
  buildUpstreamPreview,
  getMappedEfforts,
  inferReasoningStrategy,
  normalizeReasoningTranslationOverrides,
  validateReasoningTranslation,
  type BinaryEffortDecision,
  type NativeEffortDecision,
} from "./reasoningTranslation";

type Surface = keyof ReasoningTranslationConfig;

const SURFACES: Array<{ key: Surface; label: string; canonical: string }> = [
  {
    key: "chat_completions",
    label: "Chat Completions",
    canonical: "reasoning_effort",
  },
  { key: "responses", label: "Responses", canonical: "reasoning.effort" },
];

const NOOP = () => undefined;

type NativeDraft = {
  strategy: "native";
  targetPath: string;
  decisions: Record<ReasoningEffort, NativeEffortDecision>;
};

type TokenBudgetDraftDecision =
  | { mode: "map"; effort: string; budget: string }
  | { mode: "reject" };

type TokenBudgetDraft = {
  strategy: "token_budget";
  decisions: Record<ReasoningEffort, TokenBudgetDraftDecision>;
};

type BinaryDraft = {
  strategy: "binary";
  targetPath: string;
  decisions: Record<ReasoningEffort, BinaryEffortDecision>;
};

type CustomDraft = { strategy: "custom"; json: string };
type TranslationDraft = NativeDraft | TokenBudgetDraft | BinaryDraft | CustomDraft;

type DraftResolution =
  | { valid: true; translation: ReasoningTranslation; error: null }
  | { valid: false; translation: null; error: string };

function effortRecord<T>(
  value: (effort: ReasoningEffort) => T,
): Record<ReasoningEffort, T> {
  return Object.fromEntries(
    REASONING_EFFORTS.map((effort) => [effort, value(effort)]),
  ) as Record<ReasoningEffort, T>;
}

function defaultNativeTranslation(): ReasoningTranslation {
  return buildNativeTranslation(
    "/reasoning_effort",
    effortRecord((effort) => ({ mode: "map", value: effort })),
  );
}

function hasMappedValue(translation: ReasoningTranslation, effort: ReasoningEffort) {
  return Object.prototype.hasOwnProperty.call(
    translation.writes[0]?.values ?? {},
    effort,
  );
}

function draftFromTranslation(translation: ReasoningTranslation): TranslationDraft {
  const strategy = inferReasoningStrategy(translation);
  if (strategy === "native") {
    const write = translation.writes[0];
    return {
      strategy,
      targetPath: write.target_path,
      decisions: effortRecord((effort) =>
        hasMappedValue(translation, effort)
          ? { mode: "map", value: String(write.values[effort]) }
          : { mode: "reject" },
      ),
    };
  }

  if (strategy === "token_budget") {
    const effortWrite = translation.writes.find(
      (write) => write.target_path === "/reasoning_effort",
    )!;
    const budgetWrite = translation.writes.find(
      (write) => write.target_path === "/thinking_token_budget",
    )!;
    return {
      strategy,
      decisions: effortRecord((effort) =>
        hasMappedValue(translation, effort)
          ? {
              mode: "map",
              effort: String(effortWrite.values[effort] ?? ""),
              budget:
                budgetWrite.values[effort] === undefined
                  ? ""
                  : String(budgetWrite.values[effort]),
            }
          : { mode: "reject" },
      ),
    };
  }

  if (strategy === "binary") {
    const write = translation.writes[0];
    return {
      strategy,
      targetPath: write.target_path,
      decisions: effortRecord((effort) => {
        if (!hasMappedValue(translation, effort)) return "reject";
        return write.values[effort] === true ? "on" : "off";
      }),
    };
  }

  return { strategy: "custom", json: JSON.stringify(translation, null, 2) };
}

function newDraft(
  strategy: TranslationDraft["strategy"],
  current: ReasoningTranslation,
): TranslationDraft {
  if (strategy === "custom") {
    return { strategy, json: JSON.stringify(current, null, 2) };
  }
  if (strategy === "token_budget") {
    return {
      strategy,
      decisions: effortRecord((effort) => ({
        mode: "map",
        effort,
        budget: "",
      })),
    };
  }
  if (strategy === "binary") {
    return {
      strategy,
      targetPath: "/thinking",
      decisions: effortRecord((effort) =>
        effort === "none" ? "off" : "on",
      ),
    };
  }
  return draftFromTranslation(defaultNativeTranslation());
}

function validateBuiltTranslation(
  translation: ReasoningTranslation,
): DraftResolution {
  const validation = validateReasoningTranslation(translation);
  return validation.valid
    ? { valid: true, translation, error: null }
    : { valid: false, translation: null, error: validation.errors.join(" ") };
}

function resolveDraft(draft: TranslationDraft): DraftResolution {
  if (draft.strategy === "custom") {
    let parsed: unknown;
    try {
      parsed = JSON.parse(draft.json);
    } catch {
      return { valid: false, translation: null, error: "Enter valid JSON." };
    }
    const validation = validateReasoningTranslation(parsed);
    return validation.valid
      ? {
          valid: true,
          translation: parsed as ReasoningTranslation,
          error: null,
        }
      : {
          valid: false,
          translation: null,
          error: validation.errors.join(" "),
        };
  }

  if (draft.strategy === "native") {
    if (!draft.targetPath.trim()) {
      return {
        valid: false,
        translation: null,
        error: "Enter a provider target path.",
      };
    }
    return validateBuiltTranslation(
      buildNativeTranslation(draft.targetPath, draft.decisions),
    );
  }

  if (draft.strategy === "token_budget") {
    const mapped = REASONING_EFFORTS.filter(
      (effort) => draft.decisions[effort].mode === "map",
    );
    if (mapped.length === 0) {
      return {
        valid: false,
        translation: null,
        error: "Map at least one effort.",
      };
    }
    for (const effort of mapped) {
      const decision = draft.decisions[effort];
      if (decision.mode !== "map") continue;
      if (!/^\d+$/.test(decision.budget)) {
        return {
          valid: false,
          translation: null,
          error: "Enter a non-negative integer budget for every mapped effort.",
        };
      }
      const budget = Number(decision.budget);
      if (!Number.isSafeInteger(budget)) {
        return {
          valid: false,
          translation: null,
          error: "Enter a non-negative integer budget for every mapped effort.",
        };
      }
    }
    return validateBuiltTranslation(
      buildTokenBudgetTranslation(
        effortRecord((effort) => {
          const decision = draft.decisions[effort];
          return decision.mode === "reject"
            ? decision
            : {
                mode: "map",
                effort: decision.effort,
                budget: Number(decision.budget),
              };
        }),
      ),
    );
  }

  if (!draft.targetPath.trim()) {
    return {
      valid: false,
      translation: null,
      error: "Enter a provider target path.",
    };
  }
  return validateBuiltTranslation(
    buildBinaryTranslation(draft.targetPath, draft.decisions),
  );
}

interface SurfaceStrategyEditorProps {
  surface: Surface;
  label: string;
  value: ReasoningTranslation;
  onChange: (value: ReasoningTranslation) => void;
  onValidityChange: (valid: boolean) => void;
}

function SurfaceStrategyEditor({
  surface,
  label,
  value,
  onChange,
  onValidityChange,
}: SurfaceStrategyEditorProps) {
  const [draft, setDraft] = useState<TranslationDraft>(() =>
    draftFromTranslation(value),
  );
  const [selectedEffort, setSelectedEffort] =
    useState<ReasoningEffort>("none");
  const valueSignature = JSON.stringify(value);
  const observedSignature = useRef(valueSignature);
  const lastEmittedSignature = useRef<string | null>(null);

  useEffect(() => {
    if (valueSignature === observedSignature.current) return;
    observedSignature.current = valueSignature;
    if (valueSignature !== lastEmittedSignature.current) {
      setDraft(draftFromTranslation(value));
    }
    lastEmittedSignature.current = null;
  }, [value, valueSignature]);

  const resolution = useMemo(() => resolveDraft(draft), [draft]);
  useEffect(() => {
    onValidityChange(resolution.valid);
  }, [onValidityChange, resolution.valid]);

  const updateDraft = (next: TranslationDraft) => {
    setDraft(next);
    const nextResolution = resolveDraft(next);
    if (nextResolution.valid) {
      const signature = JSON.stringify(nextResolution.translation);
      observedSignature.current = signature;
      lastEmittedSignature.current = signature;
      onChange(nextResolution.translation);
    }
  };

  const mappedEfforts = resolution.valid
    ? getMappedEfforts(resolution.translation)
    : [];
  const previewEffort = mappedEfforts.includes(selectedEffort)
    ? selectedEffort
    : mappedEfforts[0];

  return (
    <div className="mt-3 space-y-3 border-t border-slate-200 pt-3">
      <div className="grid gap-3 sm:grid-cols-[minmax(0,1fr)_minmax(0,1fr)]">
        <div>
          <label
            htmlFor={`${surface}-strategy`}
            className="text-xs font-medium text-slate-700"
          >
            Strategy
          </label>
          <select
            id={`${surface}-strategy`}
            aria-label="Strategy"
            value={draft.strategy}
            onChange={(event) =>
              updateDraft(
                newDraft(
                  event.target.value as TranslationDraft["strategy"],
                  value,
                ),
              )
            }
            className="mt-1 h-9 w-full rounded-md border border-slate-300 bg-white px-3 text-sm shadow-sm focus:border-slate-500 focus:outline-none focus:ring-2 focus:ring-slate-200"
          >
            <option value="native">Native effort</option>
            <option value="token_budget">Token budget</option>
            <option value="binary">Binary thinking</option>
            <option value="custom">Custom JSON</option>
          </select>
        </div>

        {draft.strategy === "native" && (
          <div>
            <label
              htmlFor={`${surface}-native-path`}
              className="text-xs font-medium text-slate-700"
            >
              Provider target path
            </label>
            <Input
              id={`${surface}-native-path`}
              className="mt-1 font-mono text-xs"
              value={draft.targetPath}
              onChange={(event) =>
                updateDraft({ ...draft, targetPath: event.target.value })
              }
            />
          </div>
        )}

        {draft.strategy === "binary" && (
          <div>
            <label
              htmlFor={`${surface}-binary-path`}
              className="text-xs font-medium text-slate-700"
            >
              Provider target path
            </label>
            <Input
              id={`${surface}-binary-path`}
              list={`${surface}-binary-paths`}
              className="mt-1 font-mono text-xs"
              value={draft.targetPath}
              onChange={(event) =>
                updateDraft({ ...draft, targetPath: event.target.value })
              }
            />
            <datalist id={`${surface}-binary-paths`}>
              <option value="/thinking" />
              <option value="/chat_template_kwargs/thinking" />
              <option value="/chat_template_kwargs/enable_thinking" />
            </datalist>
          </div>
        )}
      </div>

      {draft.strategy === "token_budget" && (
        <div className="rounded-md border border-amber-200 bg-amber-50 px-3 py-2 text-xs leading-5 text-amber-900">
          Writes to <code>/reasoning_effort</code> and{" "}
          <code>/thinking_token_budget</code>. Onwards rejects a request unless
          the applicable OpenAI output limit is greater than the configured
          absolute budget.
        </div>
      )}

      {draft.strategy !== "custom" && (
        <div className="overflow-hidden rounded-md border border-slate-200 bg-white">
          <div className="hidden grid-cols-[5.5rem_6.5rem_minmax(0,1fr)_minmax(0,1fr)] gap-2 border-b border-slate-200 bg-slate-50 px-3 py-2 text-[10px] font-semibold uppercase tracking-wide text-slate-500 sm:grid">
            <span>Effort</span>
            <span>Decision</span>
            <span>Provider value</span>
            <span>{draft.strategy === "token_budget" ? "Budget" : ""}</span>
          </div>
          {REASONING_EFFORTS.map((effort) => {
            if (draft.strategy === "native") {
              const decision = draft.decisions[effort];
              return (
                <div
                  key={effort}
                  className="grid gap-2 border-b border-slate-100 px-3 py-2 last:border-b-0 sm:grid-cols-[5.5rem_6.5rem_minmax(0,1fr)] sm:items-center"
                >
                  <code className="text-xs font-semibold text-slate-700">
                    {effort}
                  </code>
                  <select
                    aria-label={`${effort} decision`}
                    value={decision.mode}
                    onChange={(event) =>
                      updateDraft({
                        ...draft,
                        decisions: {
                          ...draft.decisions,
                          [effort]:
                            event.target.value === "reject"
                              ? { mode: "reject" }
                              : { mode: "map", value: effort },
                        },
                      })
                    }
                    className="h-8 rounded-md border border-slate-300 bg-white px-2 text-xs"
                  >
                    <option value="map">Map</option>
                    <option value="reject">Reject</option>
                  </select>
                  {decision.mode === "map" && (
                    <Input
                      aria-label={`${effort} provider effort`}
                      value={decision.value}
                      className="h-8 font-mono text-xs"
                      onChange={(event) =>
                        updateDraft({
                          ...draft,
                          decisions: {
                            ...draft.decisions,
                            [effort]: {
                              mode: "map",
                              value: event.target.value,
                            },
                          },
                        })
                      }
                    />
                  )}
                </div>
              );
            }

            if (draft.strategy === "token_budget") {
              const decision = draft.decisions[effort];
              return (
                <div
                  key={effort}
                  className="grid gap-2 border-b border-slate-100 px-3 py-2 last:border-b-0 sm:grid-cols-[5.5rem_6.5rem_minmax(0,1fr)_minmax(0,1fr)] sm:items-center"
                >
                  <code className="text-xs font-semibold text-slate-700">
                    {effort}
                  </code>
                  <select
                    aria-label={`${effort} decision`}
                    value={decision.mode}
                    onChange={(event) =>
                      updateDraft({
                        ...draft,
                        decisions: {
                          ...draft.decisions,
                          [effort]:
                            event.target.value === "reject"
                              ? { mode: "reject" }
                              : { mode: "map", effort, budget: "" },
                        },
                      })
                    }
                    className="h-8 rounded-md border border-slate-300 bg-white px-2 text-xs"
                  >
                    <option value="map">Map</option>
                    <option value="reject">Reject</option>
                  </select>
                  {decision.mode === "map" && (
                    <>
                      <Input
                        aria-label={`${effort} provider effort`}
                        value={decision.effort}
                        className="h-8 font-mono text-xs"
                        onChange={(event) =>
                          updateDraft({
                            ...draft,
                            decisions: {
                              ...draft.decisions,
                              [effort]: {
                                ...decision,
                                effort: event.target.value,
                              },
                            },
                          })
                        }
                      />
                      <Input
                        aria-label={`${effort} token budget`}
                        inputMode="numeric"
                        min="0"
                        step="1"
                        type="number"
                        value={decision.budget}
                        className="h-8 font-mono text-xs"
                        onChange={(event) =>
                          updateDraft({
                            ...draft,
                            decisions: {
                              ...draft.decisions,
                              [effort]: {
                                ...decision,
                                budget: event.target.value,
                              },
                            },
                          })
                        }
                      />
                    </>
                  )}
                </div>
              );
            }

            const decision = draft.decisions[effort];
            return (
              <div
                key={effort}
                className="grid gap-2 border-b border-slate-100 px-3 py-2 last:border-b-0 sm:grid-cols-[5.5rem_9rem_minmax(0,1fr)] sm:items-center"
              >
                <code className="text-xs font-semibold text-slate-700">
                  {effort}
                </code>
                <select
                  aria-label={`${effort} decision`}
                  value={decision}
                  onChange={(event) =>
                    updateDraft({
                      ...draft,
                      decisions: {
                        ...draft.decisions,
                        [effort]: event.target.value as BinaryEffortDecision,
                      },
                    })
                  }
                  className="h-8 rounded-md border border-slate-300 bg-white px-2 text-xs"
                >
                  <option value="on">On</option>
                  <option value="off">Off</option>
                  <option value="reject">Reject</option>
                </select>
                <span className="text-xs text-slate-500">
                  {decision === "reject"
                    ? "Request effort is rejected"
                    : `Writes ${decision === "on" ? "true" : "false"}`}
                </span>
              </div>
            );
          })}
        </div>
      )}

      {draft.strategy === "custom" && (
        <div>
          <label
            htmlFor={`${surface}-custom-json`}
            className="text-xs font-medium text-slate-700"
          >
            Complete ReasoningTranslation JSON
          </label>
          <Textarea
            id={`${surface}-custom-json`}
            aria-label="Custom translation JSON"
            value={draft.json}
            rows={13}
            spellCheck={false}
            className="mt-1 font-mono text-xs"
            onChange={(event) =>
              updateDraft({ strategy: "custom", json: event.target.value })
            }
          />
        </div>
      )}

      {!resolution.valid && (
        <p role="alert" className="text-xs font-medium text-red-600">
          {resolution.error}
        </p>
      )}

      {resolution.valid && previewEffort && (
        <div className="rounded-md bg-slate-950 p-3 text-slate-100">
          <div className="mb-2 flex flex-wrap items-center justify-between gap-2">
            <p className="text-[10px] font-semibold uppercase tracking-wider text-slate-400">
              Upstream JSON preview
            </p>
            <select
              aria-label={`${label} preview effort`}
              value={previewEffort}
              onChange={(event) =>
                setSelectedEffort(event.target.value as ReasoningEffort)
              }
              className="h-7 rounded border border-slate-700 bg-slate-900 px-2 text-xs text-slate-100"
            >
              {mappedEfforts.map((effort) => (
                <option key={effort} value={effort}>
                  {effort}
                </option>
              ))}
            </select>
          </div>
          <pre className="overflow-x-auto text-xs leading-5">
            {JSON.stringify(
              buildUpstreamPreview(resolution.translation, previewEffort),
              null,
              2,
            )}
          </pre>
        </div>
      )}
    </div>
  );
}

function ModeButton({
  active,
  label,
  accessibleLabel,
  onClick,
}: {
  active: boolean;
  label: string;
  accessibleLabel: string;
  onClick: () => void;
}) {
  return (
    <Button
      type="button"
      size="sm"
      variant={active ? "default" : "outline"}
      aria-label={accessibleLabel}
      aria-pressed={active}
      onClick={onClick}
    >
      {label}
    </Button>
  );
}

function EditorFrame({
  title,
  description,
  children,
}: {
  title: string;
  description: string;
  children: React.ReactNode;
}) {
  return (
    <div className="rounded-lg border border-slate-200 bg-white p-3 sm:p-4">
      <div className="flex items-start gap-2">
        <Braces className="mt-0.5 h-4 w-4 shrink-0 text-slate-600" />
        <div>
          <p className="text-sm font-semibold text-slate-900">{title}</p>
          <p className="mt-1 text-xs leading-5 text-slate-500">{description}</p>
        </div>
      </div>
      {children}
    </div>
  );
}

function SurfaceTabs({
  renderSurface,
}: {
  renderSurface: (surface: (typeof SURFACES)[number]) => React.ReactNode;
}) {
  return (
    <Tabs defaultValue="chat_completions" className="mt-4">
      <TabsList className="grid h-9 w-full grid-cols-2">
        {SURFACES.map((surface) => (
          <TabsTrigger
            key={surface.key}
            value={surface.key}
            className="px-2 text-xs sm:text-sm"
          >
            {surface.label}
          </TabsTrigger>
        ))}
      </TabsList>
      {SURFACES.map((surface) => (
        <TabsContent
          key={surface.key}
          value={surface.key}
          forceMount
          className="data-[state=inactive]:hidden"
        >
          {renderSurface(surface)}
        </TabsContent>
      ))}
    </Tabs>
  );
}

export interface ReasoningTranslationEditorProps {
  value: ReasoningTranslationConfig | null;
  onChange: (value: ReasoningTranslationConfig | null) => void;
  onValidityChange?: (valid: boolean) => void;
}

export function ReasoningTranslationEditor({
  value,
  onChange,
  onValidityChange = NOOP,
}: ReasoningTranslationEditorProps) {
  const [validity, setValidity] = useState<Record<Surface, boolean>>({
    chat_completions: true,
    responses: true,
  });
  const setChatValidity = useCallback(
    (valid: boolean) =>
      setValidity((current) => ({ ...current, chat_completions: valid })),
    [],
  );
  const setResponsesValidity = useCallback(
    (valid: boolean) =>
      setValidity((current) => ({ ...current, responses: valid })),
    [],
  );
  const valid =
    (!value?.chat_completions || validity.chat_completions) &&
    (!value?.responses || validity.responses);

  useEffect(() => onValidityChange(valid), [onValidityChange, valid]);

  const setSurface = (surface: Surface, translation?: ReasoningTranslation) => {
    const next = { ...(value ?? {}) };
    if (translation) next[surface] = translation;
    else delete next[surface];
    if (!translation) {
      setValidity((current) => ({ ...current, [surface]: true }));
    }
    onChange(Object.keys(next).length === 0 ? null : next);
  };

  return (
    <EditorFrame
      title="Reasoning translation"
      description="Set endpoint defaults independently for Chat Completions and Responses. Every OpenAI reasoning effort must be mapped or rejected."
    >
      <SurfaceTabs
        renderSurface={(surface) => {
          const translation = value?.[surface.key];
          return (
            <section className="rounded-md border border-slate-200 bg-slate-50/60 p-3">
              <div className="flex flex-wrap items-center justify-between gap-2">
                <div>
                  <p className="text-xs font-medium text-slate-700">
                    Endpoint mode
                  </p>
                  <p className="text-[11px] text-slate-500">
                    Canonical field: <code>{surface.canonical}</code>
                  </p>
                </div>
                <div className="flex gap-2">
                  <ModeButton
                    active={!translation}
                    label="No mapping"
                    accessibleLabel={`No mapping ${surface.label}`}
                    onClick={() => setSurface(surface.key)}
                  />
                  <ModeButton
                    active={Boolean(translation)}
                    label="Configure"
                    accessibleLabel={`Configure ${surface.label}`}
                    onClick={() =>
                      setSurface(
                        surface.key,
                        translation ?? defaultNativeTranslation(),
                      )
                    }
                  />
                </div>
              </div>
              {!translation ? (
                <p className="mt-3 rounded-md border border-dashed border-slate-300 bg-white px-3 py-2 text-xs leading-5 text-slate-600">
                  Passes the canonical OpenAI field through unchanged. This does
                  not turn reasoning off.
                </p>
              ) : (
                <SurfaceStrategyEditor
                  surface={surface.key}
                  label={surface.label}
                  value={translation}
                  onChange={(next) => setSurface(surface.key, next)}
                  onValidityChange={
                    surface.key === "chat_completions"
                      ? setChatValidity
                      : setResponsesValidity
                  }
                />
              )}
            </section>
          );
        }}
      />
    </EditorFrame>
  );
}

export interface ReasoningTranslationOverridesEditorProps {
  value: ReasoningTranslationOverrides | null;
  onChange: (value: ReasoningTranslationOverrides | null) => void;
  endpointDefault?: ReasoningTranslationConfig | null;
  onValidityChange?: (valid: boolean) => void;
}

export function ReasoningTranslationOverridesEditor({
  value,
  onChange,
  endpointDefault = null,
  onValidityChange = NOOP,
}: ReasoningTranslationOverridesEditorProps) {
  const overrides: ReasoningTranslationOverrides = value ?? {
    chat_completions: { mode: "inherit" },
    responses: { mode: "inherit" },
  };
  const [validity, setValidity] = useState<Record<Surface, boolean>>({
    chat_completions: true,
    responses: true,
  });
  const setChatValidity = useCallback(
    (valid: boolean) =>
      setValidity((current) => ({ ...current, chat_completions: valid })),
    [],
  );
  const setResponsesValidity = useCallback(
    (valid: boolean) =>
      setValidity((current) => ({ ...current, responses: valid })),
    [],
  );
  const valid = SURFACES.every(({ key }) => {
    const override = overrides[key];
    return override.mode !== "override" || validity[key];
  });

  useEffect(() => onValidityChange(valid), [onValidityChange, valid]);

  const setSurface = (surface: Surface, override: ReasoningSurfaceOverride) => {
    if (override.mode !== "override") {
      setValidity((current) => ({ ...current, [surface]: true }));
    }
    onChange(
      normalizeReasoningTranslationOverrides({
        ...overrides,
        [surface]: override,
      }),
    );
  };

  return (
    <EditorFrame
      title="Reasoning translation overrides"
      description="Choose whether each API surface inherits its endpoint default, passes through unchanged, or uses a model-specific mapping."
    >
      <SurfaceTabs
        renderSurface={(surface) => {
          const override = overrides[surface.key];
          const inherited = endpointDefault?.[surface.key];
          return (
            <section className="rounded-md border border-slate-200 bg-slate-50/60 p-3">
              <div className="flex flex-wrap items-center justify-between gap-2">
                <div>
                  <p className="text-xs font-medium text-slate-700">Model mode</p>
                  <p className="text-[11px] text-slate-500">
                    Endpoint default: {inherited ? "configured" : "no mapping"}
                  </p>
                </div>
                <div className="flex flex-wrap gap-2">
                  <ModeButton
                    active={override.mode === "inherit"}
                    label="Inherit"
                    accessibleLabel={`Inherit ${surface.label}`}
                    onClick={() =>
                      setSurface(surface.key, { mode: "inherit" })
                    }
                  />
                  <ModeButton
                    active={override.mode === "disabled"}
                    label="No mapping"
                    accessibleLabel={`No mapping ${surface.label}`}
                    onClick={() =>
                      setSurface(surface.key, { mode: "disabled" })
                    }
                  />
                  <ModeButton
                    active={override.mode === "override"}
                    label="Override"
                    accessibleLabel={`Override ${surface.label}`}
                    onClick={() =>
                      setSurface(surface.key, {
                        mode: "override",
                        translation:
                          override.mode === "override"
                            ? override.translation
                            : defaultNativeTranslation(),
                      })
                    }
                  />
                </div>
              </div>

              {override.mode === "inherit" && (
                <p className="mt-3 rounded-md border border-dashed border-slate-300 bg-white px-3 py-2 text-xs leading-5 text-slate-600">
                  Uses the {inherited ? "configured" : "unmapped"} endpoint
                  behavior for this surface.
                </p>
              )}
              {override.mode === "disabled" && (
                <p className="mt-3 rounded-md border border-dashed border-slate-300 bg-white px-3 py-2 text-xs leading-5 text-slate-600">
                  Passes the canonical OpenAI field through unchanged and does
                  not turn reasoning off.
                </p>
              )}
              {override.mode === "override" && (
                <SurfaceStrategyEditor
                  surface={surface.key}
                  label={surface.label}
                  value={override.translation}
                  onChange={(translation) =>
                    setSurface(surface.key, { mode: "override", translation })
                  }
                  onValidityChange={
                    surface.key === "chat_completions"
                      ? setChatValidity
                      : setResponsesValidity
                  }
                />
              )}
            </section>
          );
        }}
      />
    </EditorFrame>
  );
}
