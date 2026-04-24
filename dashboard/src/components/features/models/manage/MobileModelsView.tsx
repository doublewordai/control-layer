import React, { useState, useMemo } from "react";
import type { Model, ProviderDisplayConfig } from "../../../../api/control-layer/types";
import { CatalogIcon } from "../catalog/CatalogIcon";

const CAPABILITY_FILTERS: { key: string; label: string }[] = [
  { key: "text", label: "Generative" },
  { key: "embeddings", label: "Embedding" },
  { key: "ocr", label: "OCR" },
  { key: "vision", label: "Vision" },
  { key: "reasoning", label: "Reasoning" },
  { key: "enhanced_structured_generation", label: "Enhanced Structured Generation" },
];

interface SwimlaneCardProps {
  model: Model;
  onTap: () => void;
}

const SwimlaneCard: React.FC<SwimlaneCardProps> = ({
  model,
  onTap,
}) => (
  <button
    className="shrink-0 w-[130px] bg-white border border-gray-200 rounded-xl overflow-hidden text-left active:scale-[0.97] transition-transform"
    onClick={onTap}
  >
    <div className="px-3 pt-2.5 pb-2.5">
      <div className="text-[11px] font-semibold text-gray-900 leading-tight break-words line-clamp-2">
        {model.display_name || model.alias}
      </div>
    </div>
  </button>
);

interface SwimlaneProps {
  title: string;
  titleIcon?: string;
  models: Model[];
  onNavigate: (modelId: string) => void;
}

const Swimlane: React.FC<SwimlaneProps> = ({
  title,
  titleIcon,
  models,
  onNavigate,
}) => (
  <div className="mt-5">
    <div className="flex items-center gap-2 px-4 mb-2">
      {titleIcon && (
        <CatalogIcon icon={titleIcon} label={title} size="sm" />
      )}
      <h3 className="text-sm font-semibold text-doubleword-neutral-900">
        {title}
      </h3>
    </div>
    <div className="flex gap-3 overflow-x-auto px-4 pb-1 swimlane-scroll">
      {models.map((model) => (
        <SwimlaneCard
          key={model.id}
          model={model}
          onTap={() => onNavigate(model.id)}
        />
      ))}
    </div>
  </div>
);

export interface MobileModelsViewProps {
  models: Model[];
  providerConfigMap: Map<string, ProviderDisplayConfig>;
  onNavigate: (modelId: string) => void;
}

export const MobileModelsView: React.FC<MobileModelsViewProps> = ({
  models,
  providerConfigMap,
  onNavigate,
}) => {
  const [capFilter, setCapFilter] = useState<string>("all");

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
      <div className="flex gap-1 overflow-x-auto px-4 pt-1 pb-3 swimlane-scroll">
        <button
          className={`shrink-0 px-3 py-1.5 rounded-md text-xs font-medium transition-colors ${
            capFilter === "all"
              ? "bg-primary text-primary-foreground shadow-sm"
              : "bg-background text-muted-foreground hover:bg-muted"
          }`}
          onClick={() => setCapFilter("all")}
        >
          All
        </button>
        {CAPABILITY_FILTERS.map(({ key, label }) => (
          <button
            key={key}
            className={`shrink-0 px-3 py-1.5 rounded-md text-xs font-medium transition-colors ${
              capFilter === key
                ? "bg-primary text-primary-foreground shadow-sm"
                : "bg-background text-muted-foreground hover:bg-muted"
            }`}
            onClick={() => setCapFilter(key)}
          >
            {label}
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
              onNavigate={onNavigate}
            />
          )}

          {providerGroups.map(([provider, providerModels]) => {
            const providerKey = provider.toLowerCase().trim();
            const config = providerConfigMap.get(providerKey);
            return (
              <Swimlane
                key={provider}
                title={config?.display_name || provider}
                titleIcon={config?.icon ?? providerKey}
                models={providerModels}
                onNavigate={onNavigate}
              />
            );
          })}
        </>
      )}
    </div>
  );
};
