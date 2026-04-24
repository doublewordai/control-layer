import React, { useState, useMemo } from "react";
import type { Model } from "../../../../api/control-layer";

const PROVIDER_COLORS: Record<string, string> = {
  Anthropic: "#6366f1",
  OpenAI: "#10b981",
  Google: "#3b82f6",
  DeepSeek: "#0ea5e9",
  Alibaba: "#f59e0b",
  Meta: "#8b5cf6",
  Mistral: "#f97316",
  Cohere: "#ec4899",
};

function getProviderColor(provider: string): string {
  if (PROVIDER_COLORS[provider]) return PROVIDER_COLORS[provider];
  let hash = 0;
  for (let i = 0; i < provider.length; i++) {
    hash = provider.charCodeAt(i) + ((hash << 5) - hash);
  }
  return `hsl(${Math.abs(hash) % 360}, 55%, 50%)`;
}

interface SwimlaneCardProps {
  model: Model;
  subtitle: string;
  onTap: () => void;
}

const SwimlaneCard: React.FC<SwimlaneCardProps> = ({
  model,
  subtitle,
  onTap,
}) => {
  const provider = model.metadata?.provider || "Other";
  const color = getProviderColor(provider);

  return (
    <button
      className="shrink-0 w-[130px] bg-white border border-gray-200 rounded-xl overflow-hidden text-left active:scale-[0.97] transition-transform"
      onClick={onTap}
    >
      <div className="h-1 w-full" style={{ background: color }} />
      <div className="px-3 pt-2.5 pb-3">
        <div
          className={`h-1.5 w-1.5 rounded-full mb-1.5 ${
            model.status?.last_success === true
              ? "bg-green-500"
              : model.status?.last_success === false
                ? "bg-red-500"
                : "bg-gray-300"
          }`}
        />
        <div className="text-xs font-semibold text-gray-900 truncate">
          {model.alias}
        </div>
        <div className="text-[10px] text-gray-500 mt-0.5 truncate">
          {subtitle}
        </div>
      </div>
    </button>
  );
};

interface SwimlaneProps {
  title: string;
  models: Model[];
  subtitleFn: (model: Model) => string;
  onNavigate: (modelId: string) => void;
}

const Swimlane: React.FC<SwimlaneProps> = ({
  title,
  models,
  subtitleFn,
  onNavigate,
}) => (
  <div className="mt-5">
    <h3 className="text-sm font-semibold text-doubleword-neutral-900 px-4 mb-2">
      {title}
    </h3>
    <div className="flex gap-3 overflow-x-auto px-4 pb-1 swimlane-scroll">
      {models.map((model) => (
        <SwimlaneCard
          key={model.id}
          model={model}
          subtitle={subtitleFn(model)}
          onTap={() => onNavigate(model.id)}
        />
      ))}
    </div>
  </div>
);

function capsLabel(model: Model): string {
  const caps = model.capabilities?.slice(0, 2);
  if (!caps || caps.length === 0) return model.metadata?.provider || "";
  return caps.map((c) => c.charAt(0).toUpperCase() + c.slice(1)).join(", ");
}

export interface MobileModelsViewProps {
  models: Model[];
  onNavigate: (modelId: string) => void;
}

export const MobileModelsView: React.FC<MobileModelsViewProps> = ({
  models,
  onNavigate,
}) => {
  const [capFilter, setCapFilter] = useState<string>("all");

  const capabilities = useMemo(() => {
    const caps = new Set<string>();
    models.forEach((m) => m.capabilities?.forEach((c) => caps.add(c)));
    return Array.from(caps).sort();
  }, [models]);

  const filtered = useMemo(() => {
    if (capFilter === "all") return models;
    return models.filter((m) => m.capabilities?.includes(capFilter));
  }, [models, capFilter]);

  const newModels = useMemo(
    () =>
      [...filtered]
        .filter((m) => m.metadata?.released_at)
        .sort((a, b) =>
          (b.metadata?.released_at || "").localeCompare(
            a.metadata?.released_at || "",
          ),
        )
        .slice(0, 4),
    [filtered],
  );

  const providerGroups = useMemo(() => {
    const groups: Record<string, Model[]> = {};
    filtered.forEach((m) => {
      const provider = m.metadata?.provider || "Other";
      if (!groups[provider]) groups[provider] = [];
      groups[provider].push(m);
    });
    return Object.entries(groups).sort((a, b) => b[1].length - a[1].length);
  }, [filtered]);

  return (
    <div className="pb-6">
      {/* Capability filter chips */}
      <div className="flex gap-2 overflow-x-auto px-4 pt-1 pb-3 swimlane-scroll">
        <button
          className={`shrink-0 px-3.5 py-1.5 rounded-full text-xs font-medium transition-colors ${
            capFilter === "all"
              ? "bg-doubleword-background-dark text-white"
              : "bg-gray-100 text-gray-600 border border-gray-200"
          }`}
          onClick={() => setCapFilter("all")}
        >
          All
        </button>
        {capabilities.map((cap) => (
          <button
            key={cap}
            className={`shrink-0 px-3.5 py-1.5 rounded-full text-xs font-medium transition-colors ${
              capFilter === cap
                ? "bg-doubleword-background-dark text-white"
                : "bg-gray-100 text-gray-600 border border-gray-200"
            }`}
            onClick={() => setCapFilter(cap)}
          >
            {cap.charAt(0).toUpperCase() + cap.slice(1)}
          </button>
        ))}
      </div>

      {filtered.length === 0 ? (
        <div className="text-center py-12 text-gray-500 text-sm">
          No models with this capability
        </div>
      ) : (
        <>
          {newModels.length > 0 && (
            <Swimlane
              title="New"
              models={newModels}
              subtitleFn={(m) => m.metadata?.provider || ""}
              onNavigate={onNavigate}
            />
          )}

          {providerGroups.map(([provider, providerModels]) => (
            <Swimlane
              key={provider}
              title={provider}
              models={providerModels}
              subtitleFn={capsLabel}
              onNavigate={onNavigate}
            />
          ))}
        </>
      )}
    </div>
  );
};
