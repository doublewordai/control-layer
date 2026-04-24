import React, { useState, useMemo } from "react";
import {
  Eye,
  Layers,
  Brain,
  Braces,
  MessageSquare,
} from "lucide-react";
import type { Model, ProviderDisplayConfig } from "../../../../api/control-layer/types";
import { CatalogIcon } from "../catalog/CatalogIcon";

const CAPABILITY_ICONS: Record<string, React.FC<{ className?: string }>> = {
  text: MessageSquare,
  vision: Eye,
  reasoning: Brain,
  embeddings: Layers,
  enhanced_structured_generation: Braces,
};

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
      <div className="text-[11px] font-semibold text-gray-900 leading-tight break-words line-clamp-2 min-h-[28px]">
        {model.alias}
      </div>
      {model.capabilities && model.capabilities.length > 0 && (
        <div className="flex gap-1.5 mt-1.5">
          {model.capabilities.map((cap) => {
            const Icon = CAPABILITY_ICONS[cap];
            if (!Icon) return null;
            return <Icon key={cap} className="h-3 w-3 text-gray-400" />;
          })}
        </div>
      )}
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
