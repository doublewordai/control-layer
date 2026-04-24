import React, { useState, useMemo } from "react";
import type { Model, ProviderDisplayConfig } from "../../../../api/control-layer/types";
import { CatalogIcon } from "../catalog/CatalogIcon";

type FilterType = "category" | "capability";

const FILTERS: { key: string; label: string; type: FilterType }[] = [
  { key: "generation", label: "Generative", type: "category" },
  { key: "embedding", label: "Embedding", type: "category" },
  { key: "ocr", label: "OCR", type: "category" },
  { key: "vision", label: "Vision", type: "capability" },
  { key: "reasoning", label: "Reasoning", type: "capability" },
  { key: "enhanced_structured_generation", label: "Enhanced Structured Generation", type: "capability" },
];

function getModelCategory(model: Model): string {
  if (model.metadata?.display_category) return model.metadata.display_category;
  if (model.model_type === "EMBEDDINGS") return "embedding";
  return "generation";
}

function matchesFilter(model: Model, key: string, type: FilterType): boolean {
  if (type === "category") return getModelCategory(model) === key;
  return model.capabilities?.includes(key) ?? false;
}

function sortByNewest(models: Model[]): Model[] {
  return [...models].sort((a, b) =>
    (b.metadata?.released_at || "").localeCompare(a.metadata?.released_at || ""),
  );
}

interface SwimlaneCardProps {
  model: Model;
  onTap: () => void;
}

const SwimlaneCard: React.FC<SwimlaneCardProps> = ({
  model,
  onTap,
}) => (
  <button
    aria-label={`View ${model.display_name || model.alias}`}
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
    <ul role="list" className="flex gap-3 overflow-x-auto px-4 pb-1 swimlane-scroll list-none m-0">
      {models.map((model) => (
        <li key={model.id} role="listitem">
          <SwimlaneCard
            model={model}
            onTap={() => onNavigate(model.id)}
          />
        </li>
      ))}
    </ul>
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
  const [activeFilter, setActiveFilter] = useState<string>("all");

  const filtered = useMemo(() => {
    if (activeFilter === "all") return models;
    const filter = FILTERS.find((f) => f.key === activeFilter);
    if (!filter) return models;
    return models.filter((m) => matchesFilter(m, filter.key, filter.type));
  }, [models, activeFilter]);

  const newModels = useMemo(() => {
    const withDate = filtered.filter((m) => m.metadata?.released_at);
    if (withDate.length === 0) return [];
    return sortByNewest(withDate).slice(0, 3);
  }, [filtered]);

  const providerGroups = useMemo(() => {
    const groups: Record<string, { displayName: string; models: Model[] }> = {};
    filtered.forEach((m) => {
      const rawProvider = m.metadata?.provider || "Other";
      const key = rawProvider.toLowerCase().trim();
      if (!groups[key]) {
        const config = providerConfigMap.get(key);
        groups[key] = {
          displayName: config?.display_name || rawProvider,
          models: [],
        };
      }
      groups[key].models.push(m);
    });
    return Object.entries(groups)
      .sort((a, b) => b[1].models.length - a[1].models.length)
      .map(([key, { displayName, models: laneModels }]) => ({
        key,
        displayName,
        models: sortByNewest(laneModels),
      }));
  }, [filtered, providerConfigMap]);

  return (
    <div className="pb-6">
      {/* Filter chips */}
      <div className="flex gap-1 overflow-x-auto px-4 pt-1 pb-3 swimlane-scroll">
        <button
          aria-pressed={activeFilter === "all"}
          className={`shrink-0 px-3 py-1.5 rounded-md text-xs font-medium transition-colors ${
            activeFilter === "all"
              ? "bg-primary text-primary-foreground shadow-sm"
              : "bg-background text-muted-foreground hover:bg-muted"
          }`}
          onClick={() => setActiveFilter("all")}
        >
          All
        </button>
        {FILTERS.map(({ key, label }) => (
          <button
            key={key}
            aria-pressed={activeFilter === key}
            className={`shrink-0 px-3 py-1.5 rounded-md text-xs font-medium transition-colors ${
              activeFilter === key
                ? "bg-primary text-primary-foreground shadow-sm"
                : "bg-background text-muted-foreground hover:bg-muted"
            }`}
            onClick={() => setActiveFilter(key)}
          >
            {label}
          </button>
        ))}
      </div>

      {filtered.length === 0 ? (
        <div className="text-center py-12 text-gray-500 text-sm">
          No models match this filter
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

          {providerGroups.map(({ key, displayName, models: laneModels }) => {
            const config = providerConfigMap.get(key);
            return (
              <Swimlane
                key={key}
                title={displayName}
                titleIcon={config?.icon ?? key}
                models={laneModels}
                onNavigate={onNavigate}
              />
            );
          })}
        </>
      )}
    </div>
  );
};
