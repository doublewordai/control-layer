import { useState, useMemo, useEffect, Fragment } from "react";
import { useNavigate } from "react-router-dom";
import {
  Search,
  ChevronDown,
  ChevronUp,
  ArrowUpDown,
  X,
  Check,
  MessageSquare,
  Eye,
  Layers,
  Brain,
  Code,
} from "lucide-react";
import { useModels, useGroups } from "../../../../api/control-layer";
import type {
  Model,
  ModelSortField,
  SortDirection,
} from "../../../../api/control-layer/types";
import { useAuthorization } from "../../../../utils";
import {
  formatContextLength,
  formatTariffPrice,
  getTariffDisplayName,
  getUserFacingTariffs,
} from "../../../../utils/formatters";
import { isPlaygroundDenied } from "../../../../utils/modelAccess";
import { IntelligenceBars, EmbeddingScore } from "../IntelligenceIndicator";
import { Input } from "../../../ui/input";
import { Badge } from "../../../ui/badge";
import { Button } from "../../../ui/button";
import { Popover, PopoverContent, PopoverTrigger } from "../../../ui/popover";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "../../../ui/table";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "../../../ui/tooltip";
import {
  HoverCard,
  HoverCardTrigger,
  HoverCardContent,
} from "../../../ui/hover-card";
import { Skeleton } from "../../../ui/skeleton";
import { ApiExamples } from "../../../modals";

const EVERYONE_GROUP_ID = "00000000-0000-0000-0000-000000000000";

const MODEL_TYPE_SECTIONS: { type: string; label: string }[] = [
  { type: "CHAT", label: "Generation Models" },
  { type: "EMBEDDINGS", label: "Embedding Models" },
  { type: "RERANKER", label: "Reranker Models" },
];

const CAPABILITY_CONFIG: Record<
  string,
  { icon: React.FC<{ className?: string }>; label: string; color: string }
> = {
  text: { icon: MessageSquare, label: "Text generation", color: "text-gray-400" },
  vision: { icon: Eye, label: "Vision / image input", color: "text-gray-400" },
  reasoning: { icon: Brain, label: "Reasoning", color: "text-gray-400" },
  embeddings: { icon: Layers, label: "Text embeddings", color: "text-gray-400" },
};

const DEFAULT_SORT_DIRECTIONS: Record<ModelSortField, SortDirection> = {
  alias: "asc",
  provider: "asc",
  intelligence_index: "desc",
  released_at: "desc",
  context_window: "desc",
  price_from: "asc",
  created_at: "desc",
};

function formatReleaseDate(dateStr: string): string {
  const date = new Date(dateStr + "T00:00:00");
  return date.toLocaleDateString("en-US", { month: "short", year: "numeric" });
}

function getCheapestInputPrice(tariffs: Model["tariffs"]): string | null {
  if (!tariffs) return null;
  const visible = getUserFacingTariffs(tariffs);
  if (visible.length === 0) return null;
  let cheapest = Infinity;
  for (const t of visible) {
    const price = parseFloat(t.input_price_per_token);
    if (price < cheapest) cheapest = price;
  }
  if (!isFinite(cheapest)) return null;
  return formatTariffPrice(String(cheapest));
}

/** Derive display capabilities from model type + backend capabilities. */
function getDisplayCapabilities(model: Model): string[] {
  const caps: string[] = [];
  // Implicit capability from model type
  if (model.model_type === "CHAT") caps.push("text");
  else if (model.model_type === "EMBEDDINGS") caps.push("embeddings");
  // Explicit capabilities from backend (vision, reasoning, etc.)
  if (model.capabilities) {
    for (const c of model.capabilities) {
      if (c !== "text" && c !== "embeddings" && !caps.includes(c)) {
        caps.push(c);
      }
    }
  }
  return caps;
}

function CapabilityIcons({ capabilities }: { capabilities: string[] }) {
  return (
    <div className="flex items-center gap-2">
      {capabilities.map((cap) => {
        const config = CAPABILITY_CONFIG[cap];
        if (!config) return null;
        const Icon = config.icon;
        return (
          <Tooltip key={cap}>
            <TooltipTrigger asChild>
              <Icon className={`w-4 h-4 ${config.color}`} />
            </TooltipTrigger>
            <TooltipContent side="top" className="text-xs">
              {config.label}
            </TooltipContent>
          </Tooltip>
        );
      })}
    </div>
  );
}

function SortButton({
  field,
  label,
  sortField,
  sortDirection,
  onSort,
}: {
  field: ModelSortField;
  label: string;
  sortField: ModelSortField | null;
  sortDirection: SortDirection | null;
  onSort: (field: ModelSortField) => void;
}) {
  const isActive = sortField === field;
  return (
    <button
      onClick={(e) => {
        e.preventDefault();
        onSort(field);
      }}
      className="inline-flex items-center gap-1 hover:text-foreground transition-colors group"
    >
      {label}
      {isActive ? (
        sortDirection === "asc" ? (
          <ChevronUp className="w-3.5 h-3.5" />
        ) : (
          <ChevronDown className="w-3.5 h-3.5" />
        )
      ) : (
        <ArrowUpDown className="w-3.5 h-3.5 opacity-0 group-hover:opacity-40 transition-opacity" />
      )}
    </button>
  );
}

function PricingTiers({ tariffs }: { tariffs: Model["tariffs"] }) {
  if (!tariffs) return null;
  const tiers = getUserFacingTariffs(tariffs);
  if (tiers.length === 0) return null;

  return (
    <div className="flex gap-4 flex-wrap">
      {tiers.map((t) => (
        <div key={t.id} className="bg-gray-50 rounded-lg px-4 py-2.5">
          <p className="text-xs font-medium text-gray-500 uppercase tracking-wide mb-1">
            {getTariffDisplayName(t.api_key_purpose, t.completion_window)}
          </p>
          <p className="text-sm tabular-nums">
            {formatTariffPrice(t.input_price_per_token)}/M in
            <span className="text-gray-400 mx-1">&middot;</span>
            {formatTariffPrice(t.output_price_per_token)}/M out
          </p>
        </div>
      ))}
    </div>
  );
}

function ExpandedContent({ model }: { model: Model }) {
  const summary = model.metadata?.extra?.summary ?? null;
  const useCases = model.metadata?.extra?.use_cases ?? [];

  return (
    <div className="px-2 py-4 space-y-4">
      {summary && (
        <p className="text-sm text-gray-600 max-w-3xl">{summary}</p>
      )}

      {model.tariffs && model.tariffs.length > 0 && (
        <PricingTiers tariffs={model.tariffs} />
      )}

      {useCases.length > 0 && (
        <div className="flex items-center gap-2 flex-wrap">
          <span className="text-xs text-gray-500">Best for:</span>
          {useCases.map((item) => (
            <Badge
              key={item}
              variant="secondary"
              className="text-xs font-normal"
            >
              {item}
            </Badge>
          ))}
        </div>
      )}
    </div>
  );
}

function ModelRow({
  model,
  isChat,
  isLatest,
  isExpanded,
  onToggleExpand,
  onClick,
  onApiClick,
}: {
  model: Model;
  isChat: boolean;
  isLatest: boolean;
  isExpanded: boolean;
  onToggleExpand: () => void;
  onClick: () => void;
  onApiClick: () => void;
}) {
  const navigate = useNavigate();
  const visibleTariffs = model.tariffs ? getUserFacingTariffs(model.tariffs) : [];
  const cheapestPrice =
    visibleTariffs.length > 0 ? getCheapestInputPrice(model.tariffs) : null;

  const playgroundAvailable = !isPlaygroundDenied(model);
  const colCount = 9;

  return (
    <Fragment>
      <TableRow
        className="cursor-pointer hover:bg-muted/50 transition-colors [&>td]:py-3"
        onClick={onClick}
      >
        <TableCell className="w-8 px-2">
          <button
            onClick={(e) => {
              e.stopPropagation();
              onToggleExpand();
            }}
            className="p-1 text-gray-400 hover:text-gray-600 transition-colors rounded"
            aria-label={isExpanded ? "Collapse row" : "Expand row"}
          >
            {isExpanded ? (
              <ChevronUp className="w-4 h-4" />
            ) : (
              <ChevronDown className="w-4 h-4" />
            )}
          </button>
        </TableCell>
        <TableCell>
          <div className="flex items-center gap-2">
            <span className="font-medium">{model.alias}</span>
            {isLatest && (
              <span className="inline-flex items-center rounded-full bg-blue-100 text-blue-800 px-2 py-0.5 text-[10px] font-semibold tracking-wide uppercase">
                New
              </span>
            )}
          </div>
        </TableCell>
        <TableCell className="text-muted-foreground">
          {model.metadata?.provider || "\u2014"}
        </TableCell>
        <TableCell>
          <CapabilityIcons capabilities={getDisplayCapabilities(model)} />
        </TableCell>
        <TableCell>
          {isChat ? (
            model.metadata?.intelligence_index != null ? (
              <IntelligenceBars value={model.metadata.intelligence_index} metadata={model.metadata} />
            ) : (
              <span className="text-muted-foreground">{"\u2014"}</span>
            )
          ) : (
            <EmbeddingScore metadata={model.metadata} />
          )}
        </TableCell>
        <TableCell className="tabular-nums text-muted-foreground whitespace-nowrap">
          {cheapestPrice && visibleTariffs.length > 0 ? (
            <HoverCard openDelay={150} closeDelay={100}>
              <HoverCardTrigger asChild>
                <span className="cursor-default border-b border-dotted border-gray-300">
                  <span className="text-xs text-gray-400">from </span>
                  {cheapestPrice}/M
                </span>
              </HoverCardTrigger>
              <HoverCardContent side="bottom" align="start" className="w-auto p-3">
                <div className="space-y-1.5">
                  {visibleTariffs.map((t) => (
                    <div key={t.id} className="flex items-baseline justify-between gap-4 text-xs tabular-nums">
                      <span className="text-muted-foreground">
                        {getTariffDisplayName(t.api_key_purpose, t.completion_window)}
                      </span>
                      <span>
                        {formatTariffPrice(t.input_price_per_token)}
                        <span className="text-muted-foreground mx-0.5">/</span>
                        {formatTariffPrice(t.output_price_per_token)}
                      </span>
                    </div>
                  ))}
                  <p className="text-[10px] text-muted-foreground pt-0.5">per 1M tokens · input / output</p>
                </div>
              </HoverCardContent>
            </HoverCard>
          ) : (
            "\u2014"
          )}
        </TableCell>
        <TableCell className="tabular-nums text-muted-foreground">
          {model.metadata?.context_window
            ? formatContextLength(model.metadata.context_window)
            : "\u2014"}
        </TableCell>
        <TableCell className="text-muted-foreground whitespace-nowrap">
          {model.metadata?.released_at
            ? formatReleaseDate(model.metadata.released_at)
            : "\u2014"}
        </TableCell>
        <TableCell className="text-right pr-3 lg:pr-6">
          <div className="flex items-center justify-end gap-1.5">
            <Button
              variant="ghost"
              size="sm"
              onClick={(e) => {
                e.stopPropagation();
                onApiClick();
              }}
              className="text-xs h-7 px-2 text-gray-500 hover:text-gray-700"
            >
              <Code className="h-3.5 w-3.5" />
              <span className="hidden lg:inline ml-1">API</span>
            </Button>
            {playgroundAvailable && (
              <Button
                variant="outline"
                size="sm"
                onClick={(e) => {
                  e.stopPropagation();
                  navigate(
                    `/playground?model=${encodeURIComponent(model.id)}`,
                  );
                }}
                className="text-xs h-7 px-2.5 border-blue-200 text-blue-700 hover:bg-blue-50 hover:text-blue-800"
              >
                Try it &rarr;
              </Button>
            )}
          </div>
        </TableCell>
      </TableRow>
      {isExpanded && (
        <TableRow className="hover:bg-transparent">
          <TableCell colSpan={colCount} className="pt-0 pb-4 pl-10">
            <ExpandedContent model={model} />
          </TableCell>
        </TableRow>
      )}
    </Fragment>
  );
}

function SectionTable({
  models,
  isChat,
  expandedRows,
  onToggleExpand,
  onRowClick,
  onApiClick,
  sortField,
  sortDirection,
  onSort,
}: {
  models: Model[];
  isChat: boolean;
  expandedRows: Set<string>;
  onToggleExpand: (id: string) => void;
  onRowClick: (id: string) => void;
  onApiClick: (model: Model) => void;
  sortField: ModelSortField | null;
  sortDirection: SortDirection | null;
  onSort: (field: ModelSortField) => void;
}) {
  const newModelIds = useMemo(() => {
    const cutoff = new Date();
    cutoff.setMonth(cutoff.getMonth() - 3);
    const cutoffStr = cutoff.toISOString().slice(0, 10);
    const ids = new Set<string>();
    for (const m of models) {
      if (m.metadata?.released_at && m.metadata.released_at >= cutoffStr) {
        ids.add(m.id);
      }
    }
    return ids;
  }, [models]);

  const sortProps = { sortField, sortDirection, onSort };

  return (
    <div className="border rounded-lg overflow-hidden">
      <div className="overflow-x-auto">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead className="w-8 px-2" />
              <TableHead className="min-w-[200px]">
                <SortButton field="alias" label="Name" {...sortProps} />
              </TableHead>
              <TableHead className="min-w-[100px]">
                <SortButton field="provider" label="Provider" {...sortProps} />
              </TableHead>
              <TableHead className="min-w-[100px]">
                Capabilities
              </TableHead>
              <TableHead className="min-w-[130px]">
                <SortButton field="intelligence_index" label="Intelligence" {...sortProps} />
              </TableHead>
              <TableHead className="min-w-[110px]">
                <SortButton field="price_from" label="Cost" {...sortProps} />
              </TableHead>
              <TableHead className="min-w-[80px]">
                <SortButton field="context_window" label="Context" {...sortProps} />
              </TableHead>
              <TableHead className="min-w-[80px]">
                <SortButton field="released_at" label="Released" {...sortProps} />
              </TableHead>
              <TableHead className="w-16" />
            </TableRow>
          </TableHeader>
          <TableBody>
            {models.map((model) => (
              <ModelRow
                key={model.id}
                model={model}
                isChat={isChat}
                isLatest={newModelIds.has(model.id)}
                isExpanded={expandedRows.has(model.id)}
                onToggleExpand={() => onToggleExpand(model.id)}
                onClick={() => onRowClick(model.id)}
                onApiClick={() => onApiClick(model)}
              />
            ))}
          </TableBody>
        </Table>
      </div>
    </div>
  );
}

function LoadingSkeleton() {
  return (
    <div className="space-y-8">
      <div>
        <Skeleton className="h-6 w-48 mb-3" />
        <div className="border rounded-lg overflow-hidden">
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead className="w-8 px-2" />
                <TableHead>Name</TableHead>
                <TableHead>Provider</TableHead>
                <TableHead>Capabilities</TableHead>
                <TableHead>Intelligence</TableHead>
                <TableHead>Cost</TableHead>
                <TableHead>Context</TableHead>
                <TableHead>Released</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {Array.from({ length: 5 }).map((_, i) => (
                <TableRow key={i}>
                  <TableCell className="w-8 px-2">
                    <Skeleton className="h-4 w-4" />
                  </TableCell>
                  <TableCell>
                    <Skeleton className="h-5 w-48" />
                  </TableCell>
                  <TableCell>
                    <Skeleton className="h-5 w-20" />
                  </TableCell>
                  <TableCell>
                    <Skeleton className="h-5 w-16" />
                  </TableCell>
                  <TableCell>
                    <Skeleton className="h-5 w-20" />
                  </TableCell>
                  <TableCell>
                    <Skeleton className="h-5 w-16" />
                  </TableCell>
                  <TableCell>
                    <Skeleton className="h-5 w-12" />
                  </TableCell>
                  <TableCell>
                    <Skeleton className="h-5 w-14" />
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </div>
      </div>
    </div>
  );
}

export const ModelCatalog: React.FC = () => {
  const navigate = useNavigate();
  const { hasPermission } = useAuthorization();
  const canManageGroups = hasPermission("manage-groups");

  const [searchQuery, setSearchQuery] = useState("");
  const [debouncedSearch, setDebouncedSearch] = useState("");
  const [selectedGroups, setSelectedGroups] = useState<string[]>([
    EVERYONE_GROUP_ID,
  ]);
  const [expandedRows, setExpandedRows] = useState<Set<string>>(() => {
    try {
      const stored = sessionStorage.getItem("catalog-expanded");
      return stored ? new Set(JSON.parse(stored) as string[]) : new Set();
    } catch {
      return new Set();
    }
  });
  const [sortField, setSortField] = useState<ModelSortField | null>(null);
  const [sortDirection, setSortDirection] = useState<SortDirection | null>(
    null,
  );
  const [apiExamplesModel, setApiExamplesModel] = useState<Model | null>(null);

  // Debounce search for server-side filtering
  useEffect(() => {
    const timer = setTimeout(() => setDebouncedSearch(searchQuery), 250);
    return () => clearTimeout(timer);
  }, [searchQuery]);

  const { data: groupsData } = useGroups({
    limit: 100,
    enabled: canManageGroups,
  });
  const groups = groupsData?.data || [];

  const groupFilter =
    canManageGroups && selectedGroups.length > 0
      ? selectedGroups.join(",")
      : undefined;

  const { data, isLoading } = useModels({
    limit: 100,
    group: groupFilter,
    include: "pricing",
    is_composite: true,
    search: debouncedSearch || undefined,
    sort: sortField ?? undefined,
    sort_direction: sortDirection ?? undefined,
  });

  const handleSort = (field: ModelSortField) => {
    const defaultDir = DEFAULT_SORT_DIRECTIONS[field];
    if (sortField === field) {
      if (sortDirection === defaultDir) {
        setSortDirection(defaultDir === "asc" ? "desc" : "asc");
      } else {
        setSortField(null);
        setSortDirection(null);
      }
    } else {
      setSortField(field);
      setSortDirection(defaultDir);
    }
  };

  // Group models by type into sections
  const sections = useMemo(() => {
    const allModels = data?.data || [];
    return MODEL_TYPE_SECTIONS.map((section) => ({
      ...section,
      models: allModels.filter((m) => m.model_type === section.type),
    })).filter((section) => section.models.length > 0);
  }, [data]);

  const toggleExpand = (id: string) => {
    setExpandedRows((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      sessionStorage.setItem("catalog-expanded", JSON.stringify([...next]));
      return next;
    });
  };

  const hasAnyFilters = !!debouncedSearch;

  return (
    <div className="p-4 md:p-6">
      {/* Header */}
      <div className="mb-6">
        <div className="flex flex-col md:flex-row md:items-center md:justify-between gap-4">
          <h1 className="text-2xl md:text-3xl font-bold text-doubleword-neutral-900">
            Models
          </h1>
          <div className="flex items-center gap-3">
            {canManageGroups && (
              <div className="flex items-center gap-2">
              <span className="text-sm text-muted-foreground">Group:</span>
              <Popover>
                <PopoverTrigger asChild>
                  <button
                    className="inline-flex items-center justify-between rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background focus:outline-none focus:ring-2 focus:ring-ring focus:ring-offset-2 min-w-[160px]"
                    aria-label="Filter by group"
                  >
                    <span className="flex-1 text-left truncate">
                      {selectedGroups.length === 0 ? (
                        <span className="text-muted-foreground">
                          All groups
                        </span>
                      ) : selectedGroups.length === 1 ? (
                        <span>
                          {groups.find((g) => g.id === selectedGroups[0])
                            ?.name || "Everyone"}
                        </span>
                      ) : (
                        <span className="flex gap-1 flex-wrap">
                          {selectedGroups.map((groupId) => {
                            const group = groups.find(
                              (g) => g.id === groupId,
                            );
                            return group ? (
                              <span
                                key={groupId}
                                className="inline-flex items-center gap-1 px-2 py-0.5 rounded-md bg-secondary text-secondary-foreground text-xs"
                              >
                                {group.name}
                                <X
                                  className="h-3 w-3 cursor-pointer hover:opacity-70"
                                  onClick={(e) => {
                                    e.stopPropagation();
                                    setSelectedGroups(
                                      selectedGroups.filter(
                                        (id) => id !== groupId,
                                      ),
                                    );
                                  }}
                                />
                              </span>
                            ) : null;
                          })}
                        </span>
                      )}
                    </span>
                    <ChevronDown className="h-4 w-4 opacity-50 shrink-0 ml-2" />
                  </button>
                </PopoverTrigger>
                <PopoverContent align="end" className="w-56 p-0">
                  <div className="max-h-64 overflow-y-auto">
                    {groups.length === 0 ? (
                      <div className="p-3 text-sm text-muted-foreground">
                        No groups available
                      </div>
                    ) : (
                      groups.map((group) => {
                        const isSelected = selectedGroups.includes(group.id);
                        return (
                          <button
                            key={group.id}
                            onClick={() => {
                              if (isSelected) {
                                setSelectedGroups(
                                  selectedGroups.filter(
                                    (id) => id !== group.id,
                                  ),
                                );
                              } else {
                                setSelectedGroups([
                                  ...selectedGroups,
                                  group.id,
                                ]);
                              }
                            }}
                            className="w-full flex items-center gap-2 rounded-sm py-1.5 pl-2 pr-2 text-sm hover:bg-accent hover:text-accent-foreground transition-colors text-left cursor-default"
                          >
                            <div className="w-4 h-4 shrink-0 flex items-center justify-center">
                              {isSelected && (
                                <Check className="h-4 w-4 text-primary" />
                              )}
                            </div>
                            <span>{group.name}</span>
                          </button>
                        );
                      })
                    )}
                  </div>
                </PopoverContent>
              </Popover>
              </div>
            )}
            <div className="relative">
              <Search className="absolute left-3 top-1/2 transform -translate-y-1/2 text-gray-400 w-4 h-4 z-10 pointer-events-none" />
              <Input
                type="text"
                placeholder="Search models..."
                value={searchQuery}
                onChange={(e) => setSearchQuery(e.target.value)}
                className="pl-10 w-full md:w-64"
                aria-label="Search models"
              />
            </div>
          </div>
        </div>
      </div>

      {/* Content */}
      {isLoading ? (
        <LoadingSkeleton />
      ) : sections.length === 0 ? (
        <div className="border rounded-lg py-16 text-center text-muted-foreground">
          {searchQuery || hasAnyFilters
            ? "No models matching your filters"
            : "No models available"}
        </div>
      ) : (
        <div className="space-y-8">
          {sections.map((section) => (
            <div key={section.type}>
              <h2 className="text-lg font-semibold text-gray-900 mb-3">
                {section.label}
              </h2>
              <SectionTable
                models={section.models}
                isChat={section.type === "CHAT"}
                expandedRows={expandedRows}
                onToggleExpand={toggleExpand}
                onRowClick={(id) => navigate(`/models/${id}`)}
                onApiClick={(model) => setApiExamplesModel(model)}
                sortField={sortField}
                sortDirection={sortDirection}
                onSort={handleSort}
              />
            </div>
          ))}
        </div>
      )}

      <ApiExamples
        isOpen={apiExamplesModel !== null}
        onClose={() => setApiExamplesModel(null)}
        model={apiExamplesModel}
      />
    </div>
  );
};

export default ModelCatalog;
