import { useState, useMemo, useEffect, Fragment } from "react";
import { useNavigate } from "react-router-dom";
import {
  Search,
  ChevronDown,
  ChevronUp,
  ChevronRight,
  ArrowUpDown,
  X,
  Check,
  MessageSquare,
  Eye,
  Layers,
  Brain,
  Braces,
  Code,
  Copy,
  ArrowRight,
  Sparkles,
  Zap,
  Trophy,
} from "lucide-react";
import { useModels, useGroups, useProviderDisplayConfigs } from "../../../../api/control-layer";
import type {
  Model,
  ModelDisplayCategory,
  ModelSortField,
  ModelTariff,
  SortDirection,
} from "../../../../api/control-layer/types";
import { useAuthorization, copyToClipboard } from "../../../../utils";
import {
  formatContextLength,
  formatTariffPrice,
  getTariffDisplayName,
  getUserFacingTariffs,
} from "../../../../utils/formatters";
import { isPlaygroundDenied } from "../../../../utils/modelAccess";
import { IntelligenceBars, EmbeddingScore } from "../IntelligenceIndicator";
import { Input } from "../../../ui/input";
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
import { CatalogIcon } from "./CatalogIcon";
import {
  aggregateFamilies,
  getFormatBadges,
  pickTariffForContext,
  type AggregatedFamily,
  type PricingContext,
} from "./modelFamily";


const EVERYONE_GROUP_ID = "00000000-0000-0000-0000-000000000000";

const MODEL_PURPOSE_SECTIONS: { type: ModelDisplayCategory; label: string }[] = [
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
  enhanced_structured_generation: {
    icon: Braces,
    label: "Enhanced structured generation",
    color: "text-gray-400",
  },
  code: { icon: Code, label: "Code generation", color: "text-gray-400" },
};

/**
 * Filter options for the Capabilities dropdown. We expose a curated list
 * instead of every capability the backend reports because users think about
 * modalities ("I need a vision model") more than implementation-level flags.
 */
const FILTERABLE_CAPABILITIES: Array<{ key: string; label: string }> = [
  { key: "vision", label: "Vision" },
  { key: "reasoning", label: "Reasoning" },
  { key: "code", label: "Code" },
  { key: "enhanced_structured_generation", label: "Structured output" },
  { key: "embeddings", label: "Embeddings" },
];

const DEFAULT_SORT_DIRECTIONS: Partial<Record<ModelSortField, SortDirection>> = {
  alias: "asc",
  intelligence_index: "desc",
  released_at: "desc",
  context_window: "desc",
  price_from: "asc",
};

const NEW_CUTOFF_MONTHS = 3;

function formatReleaseDate(dateStr: string): string {
  const date = new Date(dateStr + "T00:00:00");
  return date.toLocaleDateString("en-US", { month: "short", year: "numeric" });
}

function getCheapestInputPriceValue(
  tariffs: ModelTariff[] | undefined | null,
  context: PricingContext,
): number | null {
  const tariff = pickTariffForContext(tariffs, context);
  if (!tariff) return null;
  const v = parseFloat(tariff.input_price_per_token);
  return Number.isFinite(v) ? v : null;
}

function formatPricePerMillion(pricePerToken: string | number): string {
  return formatTariffPrice(pricePerToken);
}

/** Derive display capabilities from model type + backend capabilities. */
function getDisplayCapabilities(model: Model): string[] {
  const caps: string[] = [];
  if (model.model_type === "CHAT") caps.push("text");
  else if (model.model_type === "EMBEDDINGS") caps.push("embeddings");
  if (model.capabilities) {
    for (const c of model.capabilities) {
      if (c !== "text" && c !== "embeddings" && !caps.includes(c)) {
        caps.push(c);
      }
    }
  }
  return caps;
}

function getCatalogTabForModel(model: Model): ModelDisplayCategory | null {
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
              <Icon
                aria-label={config.label}
                className={`w-4 h-4 ${config.color}`}
              />
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

/**
 * Dark-themed cost tooltip that shows the input / output breakdown for every
 * user-facing tariff belonging to a specific variant.
 */
function CostHoverCard({
  tariffs,
  children,
}: {
  tariffs: ModelTariff[];
  children: React.ReactNode;
}) {
  const visible = getUserFacingTariffs(tariffs);
  if (visible.length === 0) return <>{children}</>;
  return (
    <HoverCard openDelay={150} closeDelay={100}>
      <HoverCardTrigger asChild>{children}</HoverCardTrigger>
      <HoverCardContent
        side="bottom"
        align="start"
        className="w-auto p-3 bg-doubleword-neutral-900 text-gray-100 border-doubleword-neutral-800"
      >
        <div className="space-y-1.5 min-w-[220px]">
          <p className="text-[10px] uppercase tracking-wide text-gray-400 pb-1">
            Per 1M tokens · input / output
          </p>
          {visible.map((t) => (
            <div
              key={t.id}
              className="flex items-baseline justify-between gap-4 text-xs tabular-nums"
            >
              <span className="text-gray-400">
                {getTariffDisplayName(t.api_key_purpose, t.completion_window)}
              </span>
              <span className="font-medium">
                {formatTariffPrice(t.input_price_per_token)}
                <span className="text-gray-500 mx-1">·</span>
                {formatTariffPrice(t.output_price_per_token)}
              </span>
            </div>
          ))}
        </div>
      </HoverCardContent>
    </HoverCard>
  );
}

function FormatBadges({ model }: { model: Model }) {
  const badges = getFormatBadges(model);
  if (badges.length === 0) return null;
  return (
    <>
      {badges.map((b) => (
        <span
          key={b}
          className="inline-flex items-center shrink-0 rounded-full bg-gray-100 text-gray-600 px-2 py-0.5 text-[10px] font-semibold tracking-wide uppercase"
        >
          {b}
        </span>
      ))}
    </>
  );
}

function NewBadge() {
  return (
    <span className="inline-flex items-center shrink-0 rounded-full bg-blue-100 text-blue-800 px-2 py-0.5 text-[10px] font-semibold tracking-wide uppercase">
      New
    </span>
  );
}

/* -------------------------------------------------------------------------- */
/*                                  Featured                                  */
/* -------------------------------------------------------------------------- */

interface FeaturedCardSpec {
  key: "smartest" | "new_fp8" | "most_efficient";
  label: string;
  icon: React.FC<{ className?: string }>;
  model: Model;
}

/**
 * Pick up to three headline models for the Featured Releases strip.
 *
 * - "Smartest": highest intelligence_index overall.
 * - "New FP8": newest FP8 quantization release (falls back to any new release).
 * - "Most Efficient": cheapest async input price with a non-zero intelligence
 *   score, so we don't just highlight the cheapest embedding model.
 */
function selectFeaturedModels(
  models: Model[],
  newCutoff: string,
): FeaturedCardSpec[] {
  const generation = models.filter(
    (m) => getCatalogTabForModel(m) === "generation",
  );
  if (generation.length === 0) return [];

  const cards: FeaturedCardSpec[] = [];
  const used = new Set<string>();

  const smartest = [...generation]
    .filter((m) => m.metadata?.intelligence_index != null)
    .sort(
      (a, b) =>
        (b.metadata?.intelligence_index ?? 0) -
        (a.metadata?.intelligence_index ?? 0),
    )[0];
  if (smartest) {
    cards.push({
      key: "smartest",
      label: "Smartest",
      icon: Trophy,
      model: smartest,
    });
    used.add(smartest.id);
  }

  const newFp8 = [...generation]
    .filter(
      (m) =>
        !used.has(m.id) &&
        (m.metadata?.quantization?.toUpperCase() === "FP8" ||
          /FP8/i.test(m.alias)) &&
        m.metadata?.released_at &&
        m.metadata.released_at >= newCutoff,
    )
    .sort((a, b) =>
      (b.metadata?.released_at ?? "").localeCompare(a.metadata?.released_at ?? ""),
    )[0];
  if (newFp8) {
    cards.push({
      key: "new_fp8",
      label: "New FP8",
      icon: Sparkles,
      model: newFp8,
    });
    used.add(newFp8.id);
  }

  const mostEfficient = [...generation]
    .filter(
      (m) =>
        !used.has(m.id) &&
        (m.metadata?.intelligence_index ?? 0) > 0 &&
        getCheapestInputPriceValue(m.tariffs, "async") != null,
    )
    .sort((a, b) => {
      const ap = getCheapestInputPriceValue(a.tariffs, "async") ?? Infinity;
      const bp = getCheapestInputPriceValue(b.tariffs, "async") ?? Infinity;
      return ap - bp;
    })[0];
  if (mostEfficient) {
    cards.push({
      key: "most_efficient",
      label: "Most Efficient",
      icon: Zap,
      model: mostEfficient,
    });
  }

  return cards;
}

function FeaturedReleases({
  models,
  newCutoff,
}: {
  models: Model[];
  newCutoff: string;
}) {
  const navigate = useNavigate();
  const cards = useMemo(
    () => selectFeaturedModels(models, newCutoff),
    [models, newCutoff],
  );
  if (cards.length === 0) return null;

  return (
    <section aria-labelledby="featured-releases-heading" className="mb-6">
      <h2
        id="featured-releases-heading"
        className="text-[11px] font-semibold uppercase tracking-wider text-muted-foreground mb-2"
      >
        Featured Releases
      </h2>
      <div className="grid grid-cols-1 md:grid-cols-3 gap-3">
        {cards.map((card) => {
          const { model } = card;
          const Icon = card.icon;
          const provider = model.metadata?.provider ?? "";
          const intelligence = model.metadata?.intelligence_index;
          const destination = isPlaygroundDenied(model)
            ? `/models/${model.id}`
            : `/playground?model=${encodeURIComponent(model.id)}`;
          return (
            <button
              key={card.key}
              type="button"
              onClick={() => navigate(destination)}
              className="group flex flex-col items-start text-left rounded-lg border bg-white px-4 py-3 hover:border-doubleword-neutral-400 hover:shadow-sm transition-all"
              aria-label={`${card.label}: ${model.display_name || model.alias}`}
            >
              <div className="flex w-full items-center justify-between gap-2">
                <span className="inline-flex items-center gap-1.5 rounded-full bg-blue-50 text-blue-700 px-2 py-0.5 text-[10px] font-semibold uppercase tracking-wide">
                  <Icon className="w-3 h-3" />
                  {card.label}
                </span>
                <ArrowRight className="w-4 h-4 text-gray-300 group-hover:text-gray-500 transition-colors" />
              </div>
              <div className="mt-3 min-w-0 w-full">
                <p className="text-base font-semibold text-doubleword-neutral-900 truncate">
                  {model.display_name || model.alias.split("/").pop()}
                </p>
                <p className="mt-0.5 flex items-center gap-1.5 text-xs text-muted-foreground">
                  <span className="truncate">{provider || "\u2014"}</span>
                  {intelligence != null && (
                    <>
                      <span className="text-gray-300">&middot;</span>
                      <Brain className="w-3 h-3" aria-hidden="true" />
                      <span className="tabular-nums">
                        {Math.round(intelligence)}
                      </span>
                    </>
                  )}
                </p>
              </div>
            </button>
          );
        })}
      </div>
    </section>
  );
}

/* -------------------------------------------------------------------------- */
/*                                    Rows                                    */
/* -------------------------------------------------------------------------- */

function CopyAliasButton({ alias }: { alias: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <button
          type="button"
          className="shrink-0 p-0.5 text-gray-400 opacity-0 group-hover:opacity-100 focus-visible:opacity-100 hover:text-gray-600 transition-all"
          aria-label={copied ? "Copied" : `Copy model ID ${alias}`}
          onClick={async (e) => {
            e.stopPropagation();
            if (await copyToClipboard(alias)) {
              setCopied(true);
              setTimeout(() => setCopied(false), 1500);
            }
          }}
        >
          {copied ? (
            <Check className="h-3.5 w-3.5 text-green-600" />
          ) : (
            <Copy className="h-3.5 w-3.5" />
          )}
        </button>
      </TooltipTrigger>
      <TooltipContent>
        <span className="font-mono text-xs">{alias}</span>
      </TooltipContent>
    </Tooltip>
  );
}

function RowActions({
  model,
  onApiClick,
}: {
  model: Model;
  onApiClick: () => void;
}) {
  const navigate = useNavigate();
  const playgroundAvailable = !isPlaygroundDenied(model);
  return (
    <div className="flex items-center justify-end gap-1.5 max-[699px]:gap-1">
      {playgroundAvailable && (
        <Button
          variant="outline"
          size="sm"
          onClick={(e) => {
            e.stopPropagation();
            navigate(`/playground?model=${encodeURIComponent(model.id)}`);
          }}
          className="text-xs h-7 px-2.5 border-blue-200 text-blue-700 hover:bg-blue-50 hover:text-blue-800 max-[699px]:px-2"
        >
          <span className="max-[699px]:hidden">Try it &rarr;</span>
          <span className="hidden max-[699px]:inline">Try</span>
        </Button>
      )}
      <Button
        variant="ghost"
        size="sm"
        onClick={(e) => {
          e.stopPropagation();
          onApiClick();
        }}
        className="text-xs h-7 px-2 text-gray-500 hover:text-gray-700 hover:bg-gray-100 max-[699px]:px-1.5"
      >
        <Code className="h-3.5 w-3.5" />
        <span className="hidden ml-1 max-[699px]:inline lg:inline">API</span>
      </Button>
    </div>
  );
}

function FamilyParentRow({
  family,
  isExpanded,
  onToggleExpand,
}: {
  family: AggregatedFamily;
  isExpanded: boolean;
  onToggleExpand: () => void;
}) {
  const variantCount = family.variants.length;
  return (
    <TableRow
      className="group cursor-pointer hover:bg-muted/50 transition-colors [&>td]:py-2"
      onClick={onToggleExpand}
      data-testid={`family-row-${family.key}`}
    >
      <TableCell className="px-2 max-[699px]:w-8 max-[699px]:px-1.5">
        <button
          type="button"
          onClick={(e) => {
            e.stopPropagation();
            onToggleExpand();
          }}
          className="p-1 text-gray-400 hover:text-gray-600 transition-colors rounded"
          aria-label={
            isExpanded
              ? `Collapse ${family.label} variants`
              : `Expand ${family.label} variants`
          }
          aria-expanded={isExpanded}
        >
          {isExpanded ? (
            <ChevronDown className="w-4 h-4" />
          ) : (
            <ChevronRight className="w-4 h-4" />
          )}
        </button>
      </TableCell>
      <TableCell className="overflow-hidden max-[699px]:w-full">
        <div className="flex items-center gap-2 min-w-0">
          <span className="font-medium truncate">{family.label}</span>
          <span className="inline-flex items-center shrink-0 rounded-full bg-gray-100 text-gray-600 px-2 py-0.5 text-[10px] font-medium">
            {variantCount} {variantCount === 1 ? "variant" : "variants"}
          </span>
          {family.hasNewVariant && <NewBadge />}
        </div>
      </TableCell>
      <TableCell className="overflow-hidden max-[699px]:hidden">
        <div className="flex items-center gap-2 min-w-0">
          <CatalogIcon
            icon={family.providerIcon || undefined}
            label={family.providerLabel}
            size="sm"
            fallback="none"
          />
          <span className="text-sm text-muted-foreground truncate">
            {family.providerLabel}
          </span>
        </div>
      </TableCell>
      <TableCell className="hidden md:table-cell">
        <CapabilityIcons capabilities={family.capabilities} />
      </TableCell>
      <TableCell className="hidden md:table-cell tabular-nums text-xs text-muted-foreground">
        {family.intelligenceMin != null && family.intelligenceMax != null ? (
          family.intelligenceMin === family.intelligenceMax ? (
            <span>{Math.round(family.intelligenceMin)}</span>
          ) : (
            <span>
              {Math.round(family.intelligenceMin)}
              <span className="text-gray-400 mx-0.5">&ndash;</span>
              {Math.round(family.intelligenceMax)}
            </span>
          )
        ) : (
          <span className="text-muted-foreground">{"\u2014"}</span>
        )}
      </TableCell>
      <TableCell className="hidden md:table-cell tabular-nums text-muted-foreground text-xs">
        {family.priceFrom != null ? (
          <span>
            <span className="text-xs text-gray-400">from </span>
            {formatPricePerMillion(family.priceFrom)}
            <span className="text-gray-400">/M</span>
          </span>
        ) : (
          "\u2014"
        )}
      </TableCell>
      <TableCell className="hidden md:table-cell tabular-nums text-muted-foreground text-xs">
        {family.contextMax != null ? (
          <span>
            <span className="text-xs text-gray-400">up to </span>
            {formatContextLength(family.contextMax)}
          </span>
        ) : (
          "\u2014"
        )}
      </TableCell>
      <TableCell className="hidden md:table-cell text-muted-foreground text-xs">
        {family.releasedAt ? formatReleaseDate(family.releasedAt) : "\u2014"}
      </TableCell>
      <TableCell className="w-px whitespace-nowrap text-right pr-3 lg:pr-6 max-[699px]:sticky max-[699px]:right-0 max-[699px]:z-10 max-[699px]:px-1.5 max-[699px]:bg-background" />
    </TableRow>
  );
}

function VariantRow({
  model,
  isLatest,
  context,
  onClick,
  onApiClick,
}: {
  model: Model;
  isLatest: boolean;
  context: PricingContext;
  onClick: (model: Model) => void;
  onApiClick: (model: Model) => void;
}) {
  const visibleTariffs = model.tariffs ? getUserFacingTariffs(model.tariffs) : [];
  const chosenTariff = pickTariffForContext(model.tariffs, context);
  const isChat = getCatalogTabForModel(model) !== "embedding";

  const variantShortLabel =
    model.display_name || model.alias.split("/").pop() || model.alias;

  return (
    <TableRow
      className="group cursor-pointer bg-muted/20 hover:bg-muted/40 transition-colors [&>td]:py-1.5"
      onClick={() => onClick(model)}
    >
      <TableCell className="px-2 max-[699px]:w-8 max-[699px]:px-1.5" />
      <TableCell className="overflow-hidden max-[699px]:w-full max-[699px]:whitespace-normal">
        <div className="flex items-center gap-2 min-w-0 pl-4">
          <span className="text-sm text-gray-700 truncate">
            {variantShortLabel}
          </span>
          <FormatBadges model={model} />
          {isLatest && <NewBadge />}
          <CopyAliasButton alias={model.alias} />
        </div>
      </TableCell>
      <TableCell className="overflow-hidden max-[699px]:hidden text-xs text-muted-foreground" />
      <TableCell className="hidden md:table-cell">
        <CapabilityIcons capabilities={getDisplayCapabilities(model)} />
      </TableCell>
      <TableCell className="hidden md:table-cell">
        {isChat ? (
          model.metadata?.intelligence_index != null ? (
            <IntelligenceBars
              value={model.metadata.intelligence_index}
              metadata={model.metadata}
            />
          ) : (
            <span className="text-muted-foreground text-xs">{"\u2014"}</span>
          )
        ) : (
          <EmbeddingScore metadata={model.metadata} />
        )}
      </TableCell>
      <TableCell className="hidden md:table-cell tabular-nums text-xs text-muted-foreground">
        {chosenTariff && visibleTariffs.length > 0 ? (
          <CostHoverCard tariffs={visibleTariffs}>
            <span className="cursor-default border-b border-dotted border-gray-300">
              {formatTariffPrice(chosenTariff.input_price_per_token)}
              <span className="text-gray-400">/M</span>
            </span>
          </CostHoverCard>
        ) : (
          "\u2014"
        )}
      </TableCell>
      <TableCell className="hidden md:table-cell tabular-nums text-muted-foreground text-xs">
        {model.metadata?.context_window
          ? formatContextLength(model.metadata.context_window)
          : "\u2014"}
      </TableCell>
      <TableCell className="hidden md:table-cell text-muted-foreground text-xs">
        {model.metadata?.released_at
          ? formatReleaseDate(model.metadata.released_at)
          : "\u2014"}
      </TableCell>
      <TableCell className="w-px whitespace-nowrap text-right pr-3 lg:pr-6 max-[699px]:sticky max-[699px]:right-0 max-[699px]:z-10 max-[699px]:px-1.5 max-[699px]:bg-background">
        <RowActions model={model} onApiClick={() => onApiClick(model)} />
      </TableCell>
    </TableRow>
  );
}

/* -------------------------------------------------------------------------- */
/*                                   Table                                    */
/* -------------------------------------------------------------------------- */

interface SectionTableProps {
  tableKey: string;
  families: AggregatedFamily[];
  newCutoff: string;
  context: PricingContext;
  expandedFamilies: Set<string>;
  onToggleExpand: (key: string) => void;
  sortField: ModelSortField | null;
  sortDirection: SortDirection | null;
  onSort: (tableKey: string, field: ModelSortField) => void;
  onRowClick: (model: Model) => void;
  onApiClick: (model: Model) => void;
}

function SectionTable({
  tableKey,
  families,
  newCutoff,
  context,
  expandedFamilies,
  onToggleExpand,
  sortField,
  sortDirection,
  onSort,
  onRowClick,
  onApiClick,
}: SectionTableProps) {
  const sortedFamilies = useMemo(() => {
    if (!sortField) return families;
    const dir = sortDirection === "asc" ? 1 : -1;

    const cmpNum = (a?: number | null, b?: number | null) => {
      if (a == null && b == null) return 0;
      if (a == null) return 1;
      if (b == null) return -1;
      return a - b;
    };

    return [...families].sort((a, b) => {
      let c = 0;
      switch (sortField) {
        case "alias":
          c = a.label.localeCompare(b.label);
          break;
        case "intelligence_index":
          c = cmpNum(a.intelligenceMax, b.intelligenceMax);
          break;
        case "price_from":
          c = cmpNum(a.priceFrom, b.priceFrom);
          break;
        case "context_window":
          c = cmpNum(a.contextMax, b.contextMax);
          break;
        case "released_at":
          if (a.releasedAt && b.releasedAt)
            c = a.releasedAt.localeCompare(b.releasedAt);
          else if (a.releasedAt) c = -1;
          else if (b.releasedAt) c = 1;
          break;
      }
      if (c !== 0) return c * dir;
      return a.label.localeCompare(b.label);
    });
  }, [families, sortField, sortDirection]);

  return (
    <div className="border rounded-lg overflow-hidden">
      <div className="overflow-x-auto">
        <Table className="w-full max-[699px]:table-fixed">
          <TableHeader className="sticky top-0 z-20 bg-background">
            <TableRow>
              <TableHead className="px-2 max-[699px]:w-8 max-[699px]:px-1.5" />
              <TableHead className="max-[699px]:w-full">
                <SortButton
                  field="alias"
                  label="Name"
                  sortField={sortField}
                  sortDirection={sortDirection}
                  onSort={(f) => onSort(tableKey, f)}
                />
              </TableHead>
              <TableHead className="max-[699px]:hidden">Provider</TableHead>
              <TableHead className="hidden md:table-cell">Capabilities</TableHead>
              <TableHead className="hidden md:table-cell">
                <SortButton
                  field="intelligence_index"
                  label="Intelligence"
                  sortField={sortField}
                  sortDirection={sortDirection}
                  onSort={(f) => onSort(tableKey, f)}
                />
              </TableHead>
              <TableHead className="hidden md:table-cell">
                <SortButton
                  field="price_from"
                  label="Cost ($/M)"
                  sortField={sortField}
                  sortDirection={sortDirection}
                  onSort={(f) => onSort(tableKey, f)}
                />
              </TableHead>
              <TableHead className="hidden md:table-cell">
                <SortButton
                  field="context_window"
                  label="Context"
                  sortField={sortField}
                  sortDirection={sortDirection}
                  onSort={(f) => onSort(tableKey, f)}
                />
              </TableHead>
              <TableHead className="hidden md:table-cell">
                <SortButton
                  field="released_at"
                  label="Released"
                  sortField={sortField}
                  sortDirection={sortDirection}
                  onSort={(f) => onSort(tableKey, f)}
                />
              </TableHead>
              <TableHead className="max-[699px]:sticky max-[699px]:right-0 max-[699px]:z-10 max-[699px]:w-px max-[699px]:px-1.5 max-[699px]:bg-background" />
            </TableRow>
          </TableHeader>
          <TableBody>
            {sortedFamilies.map((family) => {
              const isExpanded = expandedFamilies.has(family.key);
              return (
                <Fragment key={family.key}>
                  <FamilyParentRow
                    family={family}
                    isExpanded={isExpanded}
                    onToggleExpand={() => onToggleExpand(family.key)}
                  />
                  {isExpanded &&
                    family.variants.map((variant) => (
                      <VariantRow
                        key={variant.id}
                        model={variant}
                        isLatest={
                          !!variant.metadata?.released_at &&
                          variant.metadata.released_at >= newCutoff
                        }
                        context={context}
                        onClick={onRowClick}
                        onApiClick={onApiClick}
                      />
                    ))}
                </Fragment>
              );
            })}
          </TableBody>
        </Table>
      </div>
    </div>
  );
}

/* -------------------------------------------------------------------------- */
/*                                   Loading                                  */
/* -------------------------------------------------------------------------- */

function LoadingSkeleton() {
  return (
    <div className="border rounded-lg overflow-hidden">
      <Table className="table-fixed w-full">
        <colgroup>
          <col style={{ width: "3%" }} />
          <col />
          <col style={{ width: "13%" }} />
          <col style={{ width: "10%" }} />
          <col style={{ width: "11%" }} />
          <col style={{ width: "10%" }} />
          <col style={{ width: "8%" }} />
          <col style={{ width: "8%" }} />
          <col style={{ width: "12%" }} />
        </colgroup>
        <TableHeader>
          <TableRow>
            <TableHead className="px-2 max-[699px]:w-8 max-[699px]:px-1.5" />
            <TableHead>Name</TableHead>
            <TableHead className="max-[699px]:hidden">Provider</TableHead>
            <TableHead>Capabilities</TableHead>
            <TableHead>Intelligence</TableHead>
            <TableHead>Cost</TableHead>
            <TableHead>Context</TableHead>
            <TableHead>Released</TableHead>
            <TableHead />
          </TableRow>
        </TableHeader>
        <TableBody>
          {Array.from({ length: 8 }).map((_, i) => (
            <TableRow key={i}>
              <TableCell className="w-8 px-2 max-[699px]:px-1.5">
                <Skeleton className="h-4 w-4" />
              </TableCell>
              <TableCell>
                <Skeleton className="h-5 w-48" />
              </TableCell>
              <TableCell className="max-[699px]:hidden">
                <div className="flex items-center gap-2">
                  <Skeleton className="h-5 w-5 rounded" />
                  <Skeleton className="h-5 w-20" />
                </div>
              </TableCell>
              <TableCell className="hidden md:table-cell">
                <Skeleton className="h-5 w-16" />
              </TableCell>
              <TableCell className="hidden md:table-cell">
                <Skeleton className="h-5 w-20" />
              </TableCell>
              <TableCell className="hidden md:table-cell">
                <Skeleton className="h-5 w-16" />
              </TableCell>
              <TableCell className="hidden md:table-cell">
                <Skeleton className="h-5 w-12" />
              </TableCell>
              <TableCell className="hidden md:table-cell">
                <Skeleton className="h-5 w-14" />
              </TableCell>
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </div>
  );
}

/* -------------------------------------------------------------------------- */
/*                                Filter widgets                              */
/* -------------------------------------------------------------------------- */

function FilterDropdown({
  label,
  value,
  options,
  onChange,
  ariaLabel,
}: {
  label: string;
  value: string | null;
  options: Array<{ value: string; label: string }>;
  onChange: (next: string | null) => void;
  ariaLabel: string;
}) {
  const selected = options.find((o) => o.value === value);
  return (
    <Popover>
      <PopoverTrigger asChild>
        <button
          type="button"
          aria-label={ariaLabel}
          className="inline-flex items-center justify-between gap-2 rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background focus:outline-none focus:ring-2 focus:ring-ring focus:ring-offset-2 min-w-[140px]"
        >
          <span className="truncate">
            {selected ? (
              selected.label
            ) : (
              <span className="text-muted-foreground">{label}</span>
            )}
          </span>
          <ChevronDown className="h-4 w-4 opacity-50 shrink-0" />
        </button>
      </PopoverTrigger>
      <PopoverContent align="start" className="w-56 p-0">
        <div className="max-h-64 overflow-y-auto">
          <button
            type="button"
            onClick={() => onChange(null)}
            className={`w-full flex items-center gap-2 rounded-sm py-1.5 pl-2 pr-2 text-sm hover:bg-accent hover:text-accent-foreground transition-colors text-left cursor-default ${
              value == null ? "bg-accent/60" : ""
            }`}
          >
            <div className="w-4 h-4 shrink-0 flex items-center justify-center">
              {value == null && <Check className="h-4 w-4 text-primary" />}
            </div>
            <span>{label}</span>
          </button>
          {options.map((opt) => {
            const active = value === opt.value;
            return (
              <button
                key={opt.value}
                type="button"
                onClick={() => onChange(opt.value)}
                className={`w-full flex items-center gap-2 rounded-sm py-1.5 pl-2 pr-2 text-sm hover:bg-accent hover:text-accent-foreground transition-colors text-left cursor-default ${
                  active ? "bg-accent/60" : ""
                }`}
              >
                <div className="w-4 h-4 shrink-0 flex items-center justify-center">
                  {active && <Check className="h-4 w-4 text-primary" />}
                </div>
                <span className="truncate">{opt.label}</span>
              </button>
            );
          })}
        </div>
      </PopoverContent>
    </Popover>
  );
}

function PricingToggle({
  value,
  onChange,
}: {
  value: PricingContext;
  onChange: (next: PricingContext) => void;
}) {
  return (
    <div
      role="tablist"
      aria-label="Pricing context"
      className="inline-flex items-center rounded-md border bg-muted/40 p-0.5 text-xs"
    >
      {([
        { key: "async", label: "Async Pricing" },
        { key: "batch", label: "Batch Pricing" },
      ] as const).map((opt) => {
        const active = value === opt.key;
        return (
          <button
            key={opt.key}
            role="tab"
            type="button"
            aria-selected={active}
            onClick={() => onChange(opt.key)}
            className={`px-3 py-1.5 rounded-[5px] transition-colors ${
              active
                ? "bg-background text-foreground shadow-sm"
                : "text-muted-foreground hover:text-foreground"
            }`}
          >
            {opt.label}
          </button>
        );
      })}
    </div>
  );
}

/* -------------------------------------------------------------------------- */
/*                                    Root                                    */
/* -------------------------------------------------------------------------- */

export const ModelCatalog: React.FC = () => {
  const navigate = useNavigate();
  const { hasPermission } = useAuthorization();
  const canManageGroups = hasPermission("manage-groups");

  const newCutoff = useMemo(() => {
    const cutoff = new Date();
    cutoff.setMonth(cutoff.getMonth() - NEW_CUTOFF_MONTHS);
    return cutoff.toISOString().slice(0, 10);
  }, []);

  const [searchQuery, setSearchQuery] = useState("");
  const [debouncedSearch, setDebouncedSearch] = useState("");
  const [providerFilter, setProviderFilter] = useState<string | null>(null);
  const [capabilityFilter, setCapabilityFilter] = useState<string | null>(null);
  const [pricingContext, setPricingContext] = useState<PricingContext>(() => {
    try {
      const stored = localStorage.getItem("catalog-pricing-context");
      return stored === "batch" ? "batch" : "async";
    } catch {
      return "async";
    }
  });
  const [selectedGroups, setSelectedGroups] = useState<string[]>([
    EVERYONE_GROUP_ID,
  ]);
  const [expandedFamilies, setExpandedFamilies] = useState<Set<string>>(() => {
    try {
      const stored = sessionStorage.getItem("catalog-expanded-families");
      return stored ? new Set(JSON.parse(stored) as string[]) : new Set();
    } catch {
      return new Set();
    }
  });
  const [tableSorts, setTableSorts] = useState<
    Record<string, { field: ModelSortField; direction: SortDirection }>
  >({});
  const [apiExamplesModel, setApiExamplesModel] = useState<Model | null>(null);

  useEffect(() => {
    const timer = setTimeout(() => setDebouncedSearch(searchQuery), 250);
    return () => clearTimeout(timer);
  }, [searchQuery]);

  useEffect(() => {
    try {
      localStorage.setItem("catalog-pricing-context", pricingContext);
    } catch {
      // ignore quota errors
    }
  }, [pricingContext]);

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
    sort: "released_at",
    sort_direction: "desc",
  });
  const { data: providerDisplayConfigs = [] } = useProviderDisplayConfigs();

  const providerConfigMap = useMemo(
    () => new Map(providerDisplayConfigs.map((c) => [c.provider_key, c])),
    [providerDisplayConfigs],
  );

  const providerLabelOf = useMemo(
    () => (providerKey: string) => {
      const cfg = providerConfigMap.get(providerKey);
      return cfg?.display_name ?? providerKey;
    },
    [providerConfigMap],
  );
  const providerIconOf = useMemo(
    () => (providerKey: string) =>
      providerConfigMap.get(providerKey)?.icon ?? null,
    [providerConfigMap],
  );

  const allModels = useMemo(() => data?.data ?? [], [data?.data]);

  // Derive provider filter options from the model list so we only show
  // providers that actually have at least one model in the current result.
  const providerOptions = useMemo(() => {
    const seen = new Map<string, string>();
    for (const m of allModels) {
      const key = (m.metadata?.provider?.trim() || "other").toLowerCase();
      if (!seen.has(key)) {
        seen.set(key, providerLabelOf(key));
      }
    }
    return Array.from(seen.entries())
      .map(([value, label]) => ({ value, label }))
      .sort((a, b) => a.label.localeCompare(b.label));
  }, [allModels, providerLabelOf]);

  // Apply provider + capability filters client-side so the existing server
  // hook doesn't need to change. The catalog is in-memory per PRD.
  const filteredModels = useMemo(() => {
    return allModels.filter((m) => {
      if (providerFilter) {
        const providerKey =
          (m.metadata?.provider?.trim() || "other").toLowerCase();
        if (providerKey !== providerFilter) return false;
      }
      if (capabilityFilter) {
        const caps = getDisplayCapabilities(m);
        if (!caps.includes(capabilityFilter)) return false;
      }
      return true;
    });
  }, [allModels, providerFilter, capabilityFilter]);

  const sections = useMemo(() => {
    return MODEL_PURPOSE_SECTIONS.map((section) => ({
      ...section,
      families: aggregateFamilies(
        filteredModels.filter((m) => getCatalogTabForModel(m) === section.type),
        {
          newCutoff,
          context: pricingContext,
          providerLabelOf,
          providerIconOf,
          displayCapabilitiesOf: getDisplayCapabilities,
        },
      ),
    })).filter((s) => s.families.length > 0);
  }, [filteredModels, newCutoff, pricingContext, providerLabelOf, providerIconOf]);

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
          direction:
            existing.direction === defaultDir
              ? defaultDir === "asc"
                ? "desc"
                : "asc"
              : defaultDir,
        },
      };
    });
  };

  const toggleExpandFamily = (key: string) => {
    setExpandedFamilies((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      try {
        sessionStorage.setItem(
          "catalog-expanded-families",
          JSON.stringify([...next]),
        );
      } catch {
        // ignore quota errors
      }
      return next;
    });
  };

  const hasAnyFilters =
    !!debouncedSearch || !!providerFilter || !!capabilityFilter;

  return (
    <div className="p-3 md:p-4">
      <div className="mb-4">
        <div className="flex flex-col gap-1">
          <h1 className="text-2xl md:text-3xl font-bold text-doubleword-neutral-900">
            Models
          </h1>
          <p className="text-sm text-muted-foreground">
            Discover and integrate the latest generation of foundation models.
          </p>
        </div>
      </div>

      {!isLoading && allModels.length > 0 && (
        <FeaturedReleases models={allModels} newCutoff={newCutoff} />
      )}

      <div className="mb-3 flex flex-wrap items-center gap-2 rounded-lg border bg-background p-2">
        <div className="relative flex-1 min-w-[180px] max-w-md">
          <Search className="absolute left-3 top-1/2 transform -translate-y-1/2 text-gray-400 w-4 h-4 z-10 pointer-events-none" />
          <Input
            type="text"
            placeholder="Search models, tags, or IDs..."
            value={searchQuery}
            onChange={(e) => setSearchQuery(e.target.value)}
            className="pl-10 w-full"
            aria-label="Search models"
          />
        </div>

        <FilterDropdown
          label="All providers"
          value={providerFilter}
          options={providerOptions}
          onChange={setProviderFilter}
          ariaLabel="Filter by provider"
        />

        <FilterDropdown
          label="All capabilities"
          value={capabilityFilter}
          options={FILTERABLE_CAPABILITIES.map((c) => ({
            value: c.key,
            label: c.label,
          }))}
          onChange={setCapabilityFilter}
          ariaLabel="Filter by capability"
        />

        <div className="ml-auto flex items-center gap-2">
          {canManageGroups && (
            <Popover>
              <PopoverTrigger asChild>
                <button
                  className="inline-flex items-center justify-between rounded-md border border-input bg-background px-3 py-2 text-sm ring-offset-background focus:outline-none focus:ring-2 focus:ring-ring focus:ring-offset-2 min-w-[140px]"
                  aria-label="Filter by group"
                >
                  <span className="flex-1 text-left truncate">
                    {selectedGroups.length === 0 ? (
                      <span className="text-muted-foreground">All groups</span>
                    ) : selectedGroups.length === 1 ? (
                      <span>
                        {groups.find((g) => g.id === selectedGroups[0])?.name ||
                          "Everyone"}
                      </span>
                    ) : (
                      <span className="flex gap-1 flex-wrap">
                        {selectedGroups.map((groupId) => {
                          const group = groups.find((g) => g.id === groupId);
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
                                selectedGroups.filter((id) => id !== group.id),
                              );
                            } else {
                              setSelectedGroups([...selectedGroups, group.id]);
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
          )}
          <PricingToggle value={pricingContext} onChange={setPricingContext} />
        </div>
      </div>

      {isLoading ? (
        <LoadingSkeleton />
      ) : sections.length === 0 ? (
        <div className="border rounded-lg py-16 text-center text-muted-foreground">
          {hasAnyFilters
            ? "No models matching your filters"
            : "No models available"}
        </div>
      ) : (
        <div className="space-y-6">
          {sections.map((section) => (
            <div key={section.type}>
              <h2 className="text-sm font-medium text-muted-foreground mb-1.5">
                {section.label}
              </h2>
              <SectionTable
                tableKey={section.type}
                families={section.families}
                newCutoff={newCutoff}
                context={pricingContext}
                expandedFamilies={expandedFamilies}
                onToggleExpand={toggleExpandFamily}
                sortField={tableSorts[section.type]?.field ?? null}
                sortDirection={tableSorts[section.type]?.direction ?? null}
                onSort={handleSort}
                onRowClick={(m) => navigate(`/models/${m.id}`)}
                onApiClick={(m) => setApiExamplesModel(m)}
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
