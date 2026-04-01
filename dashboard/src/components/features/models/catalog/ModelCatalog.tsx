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
import { useModels, useGroups, useProviderDisplayConfigs } from "../../../../api/control-layer";
import type {
  Model,
  ModelDisplayCategory,
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
import { Tabs, TabsContent, TabsList, TabsTrigger } from "../../../ui/tabs";
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
import { CatalogIcon } from "./CatalogIcon";
import { getModelOrder } from "./catalogPresentation";

const EVERYONE_GROUP_ID = "00000000-0000-0000-0000-000000000000";

type CatalogTab = ModelDisplayCategory;

const MODEL_TYPE_SECTIONS: { type: CatalogTab; label: string }[] = [
  { type: "generation", label: "Generation" },
  { type: "embedding", label: "Embedding" },
  { type: "ocr", label: "OCR" },
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

const DEFAULT_SORT_DIRECTIONS: Partial<Record<ModelSortField, SortDirection>> = {
  alias: "asc",
  intelligence_index: "desc",
  released_at: "desc",
  context_window: "desc",
  price_from: "asc",
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

function getCheapestInputPriceValue(tariffs: Model["tariffs"]): number | null {
  if (!tariffs) return null;
  const visible = getUserFacingTariffs(tariffs);
  if (visible.length === 0) return null;
  let cheapest = Infinity;
  for (const t of visible) {
    const price = parseFloat(t.input_price_per_token);
    if (price < cheapest) cheapest = price;
  }
  return Number.isFinite(cheapest) ? cheapest : null;
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

function getCatalogTabForModel(model: Model): CatalogTab | null {
  if (model.metadata?.display_category) {
    return model.metadata.display_category;
  }
  if (model.model_type === "EMBEDDINGS") return "embedding";
  if (model.model_type === "CHAT" || model.model_type === "RERANKER") {
    return "generation";
  }
  return null;
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
  const colCount = 8;

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
              className="text-xs h-7 px-2 text-gray-500 hover:text-gray-700 hover:bg-gray-100"
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
  tableKey,
  models,
  isChat,
  expandedRows,
  onToggleExpand,
  onRowClick,
  onApiClick,
  nameColumnWidth,
  sortField,
  sortDirection,
  onSort,
}: {
  tableKey: string;
  models: Model[];
  isChat: boolean;
  expandedRows: Set<string>;
  onToggleExpand: (id: string) => void;
  onRowClick: (id: string) => void;
  onApiClick: (model: Model) => void;
  nameColumnWidth: string;
  sortField: ModelSortField | null;
  sortDirection: SortDirection | null;
  onSort: (tableKey: string, field: ModelSortField) => void;
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

  const sortedModels = useMemo(() => {
    const directionMultiplier = sortDirection === "asc" ? 1 : -1;

    const compareReleasedAt = (a: Model, b: Model) =>
      (a.metadata?.released_at || "").localeCompare(b.metadata?.released_at || "");

    const compareNumbers = (a?: number | null, b?: number | null) => {
      if (a == null && b == null) return 0;
      if (a == null) return 1;
      if (b == null) return -1;
      return a - b;
    };

    return [...models].sort((a, b) => {
      let comparison = 0;

      switch (sortField) {
        case "alias":
          comparison = a.alias.localeCompare(b.alias);
          break;
        case "intelligence_index":
          comparison = compareNumbers(
            a.metadata?.intelligence_index,
            b.metadata?.intelligence_index,
          );
          break;
        case "price_from": {
          comparison = compareNumbers(
            getCheapestInputPriceValue(a.tariffs),
            getCheapestInputPriceValue(b.tariffs),
          );
          break;
        }
        case "context_window":
          comparison = compareNumbers(
            a.metadata?.context_window,
            b.metadata?.context_window,
          );
          break;
        case "released_at":
          comparison = compareReleasedAt(a, b);
          break;
        default:
          comparison = compareNumbers(
            a.metadata?.intelligence_index,
            b.metadata?.intelligence_index,
          );
          break;
      }

      if (comparison !== 0) {
        return comparison * directionMultiplier;
      }

      return a.alias.localeCompare(b.alias);
    });
  }, [models, sortDirection, sortField]);

  return (
    <div className="border rounded-lg overflow-hidden">
      <div className="overflow-x-auto">
        <Table className="table-fixed w-full">
          <colgroup>
            <col className="w-8" />
            <col style={{ width: nameColumnWidth }} />
            <col className="w-[120px]" />
            <col className="w-[140px]" />
            <col className="w-[120px]" />
            <col className="w-[104px]" />
            <col className="w-[104px]" />
            <col className="w-[112px]" />
          </colgroup>
          <TableHeader>
            <TableRow>
              <TableHead className="w-8 px-2" />
              <TableHead>
                <SortButton
                  field="alias"
                  label="Name"
                  sortField={sortField}
                  sortDirection={sortDirection}
                  onSort={(field) => onSort(tableKey, field)}
                />
              </TableHead>
              <TableHead>
                Capabilities
              </TableHead>
              <TableHead>
                <SortButton
                  field="intelligence_index"
                  label="Intelligence"
                  sortField={sortField}
                  sortDirection={sortDirection}
                  onSort={(field) => onSort(tableKey, field)}
                />
              </TableHead>
              <TableHead>
                <SortButton
                  field="price_from"
                  label="Cost"
                  sortField={sortField}
                  sortDirection={sortDirection}
                  onSort={(field) => onSort(tableKey, field)}
                />
              </TableHead>
              <TableHead>
                <SortButton
                  field="context_window"
                  label="Context"
                  sortField={sortField}
                  sortDirection={sortDirection}
                  onSort={(field) => onSort(tableKey, field)}
                />
              </TableHead>
              <TableHead>
                <SortButton
                  field="released_at"
                  label="Released"
                  sortField={sortField}
                  sortDirection={sortDirection}
                  onSort={(field) => onSort(tableKey, field)}
                />
              </TableHead>
              <TableHead className="w-[112px]" />
            </TableRow>
          </TableHeader>
          <TableBody>
            {sortedModels.map((model) => (
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
          <Table className="table-fixed w-full">
            <colgroup>
              <col className="w-8" />
              <col className="w-[32ch]" />
              <col className="w-[120px]" />
              <col className="w-[140px]" />
              <col className="w-[120px]" />
              <col className="w-[104px]" />
              <col className="w-[104px]" />
              <col className="w-[112px]" />
            </colgroup>
            <TableHeader>
              <TableRow>
                <TableHead className="w-8 px-2" />
                <TableHead>Name</TableHead>
                <TableHead>Capabilities</TableHead>
                <TableHead>Intelligence</TableHead>
                <TableHead>Cost</TableHead>
                <TableHead>Context</TableHead>
                <TableHead>Released</TableHead>
                <TableHead />
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
  const [activeTab, setActiveTab] = useState<CatalogTab>("generation");
  const [tableSorts, setTableSorts] = useState<
    Record<string, { field: ModelSortField; direction: SortDirection }>
  >({});
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
  });
  const { data: providerDisplayConfigs = [] } = useProviderDisplayConfigs();

  const handleSort = (tableKey: string, field: ModelSortField) => {
    const defaultDir = DEFAULT_SORT_DIRECTIONS[field] ?? "asc";
    setTableSorts((current) => {
      const existing = current[tableKey];
      if (!existing || existing.field !== field) {
        return {
          ...current,
          [tableKey]: { field, direction: defaultDir },
        };
      }

      return {
        ...current,
        [tableKey]: {
          field,
          direction: existing.direction === defaultDir
            ? (defaultDir === "asc" ? "desc" : "asc")
            : defaultDir,
        },
      };
    });
  };

  const nameColumnWidth = useMemo(() => {
    const maxAliasLength = Math.max(
      ...((data?.data || []).map((model) => model.alias.length)),
      24,
    );
    const widthInCh = Math.min(Math.max(maxAliasLength + 4, 28), 44);
    return `${widthInCh}ch`;
  }, [data?.data]);

  // Group models by catalog tab, then by provider.
  const sections = useMemo(() => {
    const allModels = data?.data || [];
    const providerConfigMap = new Map(
      providerDisplayConfigs.map((config) => [config.provider_key, config]),
    );
    return MODEL_TYPE_SECTIONS.map((section) => {
      const grouped = new Map<
        string,
        {
          key: string;
          label: string;
          icon?: string | null;
          sortOrder: number;
          models: Model[];
        }
      >();

      for (const model of allModels) {
        if (getCatalogTabForModel(model) !== section.type) continue;
        const providerLabel = model.metadata?.provider?.trim() || "Other";
        const providerKey = providerLabel.toLowerCase();
        const providerConfig = providerConfigMap.get(providerKey);
        const existing = grouped.get(providerKey);
        if (existing) {
          existing.models.push(model);
        } else {
          grouped.set(providerKey, {
            key: providerKey,
            label: providerConfig?.display_name || providerLabel,
            icon: providerConfig?.icon || undefined,
            sortOrder: providerConfig?.sort_order ?? Number.MAX_SAFE_INTEGER,
            models: [model],
          });
        }
      }

      const groups = [...grouped.values()]
        .map((group) => ({
          ...group,
          models: [...group.models].sort((a, b) => {
            const orderA = getModelOrder(a);
            const orderB = getModelOrder(b);
            if (orderA != null && orderB != null && orderA !== orderB) {
              return orderA - orderB;
            }
            if (orderA != null) return -1;
            if (orderB != null) return 1;
            return a.alias.localeCompare(b.alias);
          }),
        }))
        .sort((a, b) =>
          a.sortOrder === b.sortOrder
            ? a.label.localeCompare(b.label)
            : a.sortOrder - b.sortOrder,
        );

      return {
        ...section,
        groups,
      };
    }).filter((section) => section.groups.length > 0);
  }, [data?.data, providerDisplayConfigs]);

  useEffect(() => {
    if (sections.length === 0) return;
    if (!sections.some((section) => section.type === activeTab)) {
      setActiveTab(sections[0].type);
    }
  }, [activeTab, sections]);

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
        <Tabs
          value={activeTab}
          onValueChange={(value) => setActiveTab(value as CatalogTab)}
          className="space-y-4"
        >
          <TabsList className="w-full justify-start overflow-x-auto bg-transparent p-0">
            {sections.map((section) => (
              <TabsTrigger
                key={section.type}
                value={section.type}
                className="rounded-none bg-transparent px-0 py-2 mr-6 shadow-none data-[state=active]:bg-transparent data-[state=active]:shadow-none"
              >
                {section.label}
              </TabsTrigger>
            ))}
          </TabsList>

          {sections.map((section) => (
            <TabsContent key={section.type} value={section.type} className="space-y-4">
              {section.groups.map((group) => (
                <div key={group.key} className="space-y-3">
                  <div className="flex items-center gap-2 px-1">
                    <CatalogIcon
                      icon={group.icon || undefined}
                      label={group.label}
                      size="sm"
                      fallback="none"
                    />
                    <div className="min-w-0">
                      <h3 className="text-sm font-semibold text-gray-900">
                        {group.label}
                      </h3>
                    </div>
                  </div>
                  <SectionTable
                    tableKey={`${section.type}:${group.key}`}
                    models={group.models}
                    isChat={section.type !== "embedding"}
                    expandedRows={expandedRows}
                    onToggleExpand={toggleExpand}
                    onRowClick={(id) => navigate(`/models/${id}`)}
                    onApiClick={(model) => setApiExamplesModel(model)}
                    nameColumnWidth={nameColumnWidth}
                    sortField={
                      tableSorts[`${section.type}:${group.key}`]?.field ??
                      "intelligence_index"
                    }
                    sortDirection={
                      tableSorts[`${section.type}:${group.key}`]?.direction ??
                      "desc"
                    }
                    onSort={handleSort}
                  />
                </div>
              ))}
            </TabsContent>
          ))}
        </Tabs>
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
