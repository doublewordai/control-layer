import { Braces, RotateCcw, WandSparkles } from "lucide-react";
import { useEffect, useState } from "react";
import type {
  ReasoningEffort,
  ReasoningTranslation,
  ReasoningTranslationConfig,
} from "../../../api/control-layer/types";
import { Button } from "../../ui/button";
import { Input } from "../../ui/input";

type Surface = keyof ReasoningTranslationConfig;

const EFFORTS: ReasoningEffort[] = [
  "none",
  "minimal",
  "low",
  "medium",
  "high",
  "xhigh",
  "max",
];

const SURFACES: Array<{ key: Surface; label: string; canonical: string }> = [
  { key: "chat_completions", label: "Chat Completions", canonical: "reasoning_effort" },
  { key: "responses", label: "Responses", canonical: "reasoning.effort" },
];

const SGLANG_PRESET: ReasoningTranslationConfig = {
  chat_completions: {
    target_path: "/chat_template_kwargs/thinking",
    values: {
      none: false,
      minimal: true,
      low: true,
      medium: true,
      high: true,
      xhigh: true,
      max: true,
    },
  },
};

function draftKey(surface: Surface, effort: ReasoningEffort) {
  return `${surface}:${effort}`;
}

function createDrafts(value: ReasoningTranslationConfig | null) {
  const drafts: Record<string, string> = {};
  for (const surface of SURFACES) {
    const translation = value?.[surface.key];
    for (const effort of EFFORTS) {
      if (translation && effort in translation.values) {
        drafts[draftKey(surface.key, effort)] = JSON.stringify(translation.values[effort]);
      }
    }
  }
  return drafts;
}

function previewBody(translation: ReasoningTranslation) {
  const effort = EFFORTS.find((candidate) => candidate in translation.values);
  if (!effort) return {};
  const segments = translation.target_path.split("/").filter(Boolean);
  const root: Record<string, unknown> = {};
  let cursor = root;
  for (const segment of segments.slice(0, -1)) {
    const next: Record<string, unknown> = {};
    cursor[segment] = next;
    cursor = next;
  }
  if (segments.length > 0) {
    cursor[segments.at(-1)!] = translation.values[effort];
  }
  return root;
}

export interface ReasoningTranslationEditorProps {
  value: ReasoningTranslationConfig | null;
  onChange: (value: ReasoningTranslationConfig | null) => void;
  allowInherit?: boolean;
}

export function ReasoningTranslationEditor({
  value,
  onChange,
  allowInherit = false,
}: ReasoningTranslationEditorProps) {
  const [drafts, setDrafts] = useState(() => createDrafts(value));
  const [errors, setErrors] = useState<Record<string, string>>({});

  useEffect(() => {
    setDrafts(createDrafts(value));
    setErrors({});
  }, [value]);

  if (allowInherit && value === null) {
    return (
      <div className="rounded-lg border border-dashed border-slate-300 bg-slate-50 p-4">
        <p className="text-sm font-medium text-slate-800">Inheriting the endpoint mapping</p>
        <p className="mt-1 text-xs text-slate-500">
          This model follows its provider endpoint. Add an override only when this deployment needs a different request shape.
        </p>
        <Button type="button" variant="outline" size="sm" className="mt-3" onClick={() => onChange(SGLANG_PRESET)}>
          Override endpoint default
        </Button>
      </div>
    );
  }

  const setSurface = (surface: Surface, translation: ReasoningTranslation | undefined) => {
    const next = { ...(value ?? {}) };
    if (translation) next[surface] = translation;
    else delete next[surface];
    onChange(Object.keys(next).length > 0 ? next : null);
  };

  const updateMappedValue = (surface: Surface, effort: ReasoningEffort, raw: string) => {
    const key = draftKey(surface, effort);
    setDrafts((current) => ({ ...current, [key]: raw }));
    let parsed: unknown;
    try {
      parsed = JSON.parse(raw);
    } catch {
      setErrors((current) => ({ ...current, [key]: "Enter valid JSON" }));
      return;
    }
    setErrors((current) => {
      const next = { ...current };
      delete next[key];
      return next;
    });
    const translation = value?.[surface];
    if (!translation) return;
    setSurface(surface, {
      ...translation,
      values: { ...translation.values, [effort]: parsed },
    });
  };

  return (
    <div className="space-y-4 rounded-xl border border-slate-200 bg-white p-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <div className="flex items-center gap-2 text-sm font-semibold text-slate-900">
            <Braces className="h-4 w-4 text-teal-600" />
            Reasoning translation
          </div>
          <p className="mt-1 max-w-2xl text-xs leading-5 text-slate-500">
            Public requests keep the OpenAI reasoning shape. Map each supported effort to the provider-specific field sent upstream.
          </p>
        </div>
        <div className="flex gap-2">
          {allowInherit && (
            <Button type="button" variant="ghost" size="sm" onClick={() => onChange(null)}>
              <RotateCcw className="mr-1.5 h-3.5 w-3.5" />
              Use endpoint default
            </Button>
          )}
          <Button type="button" variant="outline" size="sm" onClick={() => onChange(SGLANG_PRESET)}>
            <WandSparkles className="mr-1.5 h-3.5 w-3.5" />
            Use SGLang preset
          </Button>
        </div>
      </div>

      {SURFACES.map((surface) => {
        const translation = value?.[surface.key];
        return (
          <section key={surface.key} className="rounded-lg border border-slate-200 bg-slate-50/60 p-3">
            <div className="flex items-center justify-between gap-3">
              <div>
                <p className="text-sm font-medium text-slate-800">{surface.label}</p>
                <p className="text-xs text-slate-500">Canonical field: <code>{surface.canonical}</code></p>
              </div>
              <label className="flex items-center gap-2 text-xs font-medium text-slate-600">
                <input
                  type="checkbox"
                  aria-label={`Configure ${surface.label}`}
                  checked={Boolean(translation)}
                  onChange={(event) =>
                    setSurface(
                      surface.key,
                      event.target.checked
                        ? { target_path: "/thinking/type", values: { none: "disabled" } }
                        : undefined,
                    )
                  }
                />
                Configure
              </label>
            </div>

            {translation && (
              <div className="mt-3 space-y-3">
                <div>
                  <label htmlFor={`${surface.key}-target`} className="text-xs font-medium text-slate-700">
                    Provider target JSON pointer
                  </label>
                  <Input
                    id={`${surface.key}-target`}
                    value={translation.target_path}
                    className="mt-1 font-mono text-xs"
                    placeholder="/chat_template_kwargs/thinking"
                    onChange={(event) => setSurface(surface.key, { ...translation, target_path: event.target.value })}
                  />
                </div>

                <div className="grid gap-2 sm:grid-cols-2 xl:grid-cols-3">
                  {EFFORTS.map((effort) => {
                    const enabled = effort in translation.values;
                    const key = draftKey(surface.key, effort);
                    return (
                      <div key={effort} className="rounded-md border border-slate-200 bg-white p-2">
                        <label className="flex items-center gap-2 text-xs font-medium text-slate-700">
                          <input
                            type="checkbox"
                            checked={enabled}
                            onChange={(event) => {
                              const values = { ...translation.values };
                              if (event.target.checked) values[effort] = true;
                              else delete values[effort];
                              setSurface(
                                surface.key,
                                Object.keys(values).length > 0
                                  ? { ...translation, values }
                                  : undefined,
                              );
                            }}
                          />
                          {effort}
                        </label>
                        {enabled && (
                          <>
                            <Input
                              aria-label={`${surface.label} ${effort} value`}
                              value={drafts[key] ?? JSON.stringify(translation.values[effort])}
                              className="mt-2 h-8 font-mono text-xs"
                              onChange={(event) => updateMappedValue(surface.key, effort, event.target.value)}
                            />
                            {errors[key] && <p className="mt-1 text-xs text-red-600">{errors[key]}</p>}
                          </>
                        )}
                      </div>
                    );
                  })}
                </div>

                <div className="rounded-md bg-slate-950 p-3 text-slate-100">
                  <p className="mb-2 text-[10px] font-semibold uppercase tracking-wider text-slate-400">Example upstream fragment</p>
                  <pre className="overflow-x-auto text-xs">{JSON.stringify(previewBody(translation), null, 2)}</pre>
                </div>
              </div>
            )}
          </section>
        );
      })}
    </div>
  );
}
