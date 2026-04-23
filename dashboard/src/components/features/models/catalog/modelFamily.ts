import type { Model, ModelTariff } from "../../../../api/control-layer/types";

/**
 * Utilities for grouping model variants into families on the catalog page.
 *
 * A "family" is a group of variants that share a common base (e.g. all the
 * "Qwen 3.5" sizes / quantizations). The user sees a single collapsed row
 * per family and can expand to see the specific variants.
 *
 * The approach is heuristic: we split the model alias on `-` and treat the
 * tokens up to (but not including) the first size token (e.g. `397B`, `20b`,
 * `A17B`) as the family name, and everything from the size token onwards as
 * the variant label.
 */

export interface FamilyInfo {
  /** Stable case-insensitive key used for grouping. */
  key: string;
  /** Human-friendly family label (e.g. "Qwen 3.5"). */
  label: string;
  /** Human-friendly variant label (e.g. "397B A17B FP8"). */
  variantLabel: string;
}

const SIZE_TOKEN_RE = /^\d+(\.\d+)?[bm]$|^a\d+b$/i;

function isSizeToken(token: string): boolean {
  return SIZE_TOKEN_RE.test(token);
}

/**
 * Insert spaces between letters and adjacent digits so that e.g. "Qwen3.5"
 * becomes "Qwen 3.5". Hyphens are preserved (they are converted to spaces by
 * callers that want a fully-humanized label).
 */
function humanizeToken(token: string): string {
  return token.replace(/([A-Za-z])(\d)/g, "$1 $2").replace(/(\d)([A-Za-z])/g, "$1 $2");
}

/**
 * Derive family / variant info from a model.
 *
 * Groups by provider + family stem so that "Alibaba/Qwen3.5-*" models get
 * lumped together but "Alibaba/Qwen3-*" and "Alibaba/Qwen3-VL-*" stay
 * separate (since VL is not a size token and so is part of the family stem).
 */
export function getFamilyInfo(model: Model): FamilyInfo {
  const alias = model.alias || model.model_name || "";
  const slashIdx = alias.lastIndexOf("/");
  const aliasProvider = slashIdx >= 0 ? alias.slice(0, slashIdx) : "";
  const stem = slashIdx >= 0 ? alias.slice(slashIdx + 1) : alias;
  const provider =
    (model.metadata?.provider || aliasProvider || "").trim() || "Unknown";

  const tokens = stem.split("-").filter(Boolean);
  if (tokens.length === 0) {
    return {
      key: `${provider}::${alias}`.toLowerCase(),
      label: alias || "Unknown",
      variantLabel: "",
    };
  }

  let firstSizeIdx = tokens.findIndex(isSizeToken);
  // Must have at least one family token before the size token; if the alias
  // starts with a size token we treat the full stem as the family.
  if (firstSizeIdx <= 0) firstSizeIdx = -1;

  const familyTokens =
    firstSizeIdx === -1 ? tokens : tokens.slice(0, firstSizeIdx);
  const variantTokens =
    firstSizeIdx === -1 ? [] : tokens.slice(firstSizeIdx);

  const familyStem = familyTokens.join("-");

  const key = `${provider}::${familyStem}`.toLowerCase();

  // Family label: humanize each token and join with spaces. e.g. "Qwen3.5" ->
  // "Qwen 3.5", "gpt-oss" -> "gpt oss".
  const label = familyTokens.map(humanizeToken).join(" ") || stem;

  const variantLabel = variantTokens.map(humanizeToken).join(" ");

  return { key, label, variantLabel };
}

/**
 * Return true if the alias contains a recognisable quantization / format
 * marker (e.g. FP8, INT4).
 */
const QUANT_TOKENS = new Set([
  "fp8",
  "fp16",
  "bf16",
  "int4",
  "int8",
  "awq",
  "gptq",
]);

export function getFormatBadges(model: Model): string[] {
  const badges: string[] = [];
  if (model.metadata?.quantization) {
    badges.push(model.metadata.quantization);
    return badges;
  }
  const stem =
    model.alias.split("/").pop() || model.model_name.split("/").pop() || "";
  const tokens = stem.split("-");
  for (const token of tokens) {
    if (QUANT_TOKENS.has(token.toLowerCase())) {
      badges.push(token.toUpperCase());
    }
  }
  return badges;
}

/**
 * Pricing contexts surfaced on the catalog. The Doubleword platform exposes
 * both "Async" (realtime / 1h batch) and "Batch" (24h+) tariffs and users
 * want to flip between them at the top of the table.
 */
export type PricingContext = "async" | "batch";

/**
 * Rank a tariff for the given pricing context. Lower score = better match.
 *
 * The ranking is intentionally total so that the result of
 * `pickTariffForContext` is independent of the order in which the backend
 * returns tariffs. When multiple tariffs match the ideal shape (e.g. two
 * realtime tariffs) we break ties by input price (cheaper wins) so behaviour
 * is still deterministic.
 *
 * Preference order:
 *
 *   Async context
 *     1. realtime
 *     2. batch + 1h
 *     3. any other batch (24h / 48h / null) — fallback so the cell isn't blank
 *     4. anything else visible
 *
 *   Batch context
 *     1. batch + 24h
 *     2. batch + 48h
 *     3. batch + null  (legacy rows that predate completion_window)
 *     4. batch + 1h    (fallback only)
 *     5. realtime      (fallback only)
 *     6. anything else visible
 */
function tariffPriority(
  tariff: Pick<ModelTariff, "api_key_purpose" | "completion_window">,
  context: PricingContext,
): number {
  const purpose = tariff.api_key_purpose ?? "realtime";
  const window = tariff.completion_window;

  if (context === "async") {
    if (purpose === "realtime") return 0;
    if (purpose === "batch" && window === "1h") return 1;
    if (purpose === "batch") return 2;
    return 10;
  }

  // context === "batch"
  if (purpose === "batch" && window === "24h") return 0;
  if (purpose === "batch" && window === "48h") return 1;
  if (purpose === "batch" && window == null) return 2;
  if (purpose === "batch" && window === "1h") return 3;
  if (purpose === "realtime") return 4;
  return 10;
}

/**
 * Pick the tariff that best represents the given pricing context for a model.
 *
 * Visible tariffs (non-playground, active) are scored with `tariffPriority`
 * and the lowest-scoring one is returned. If multiple tariffs tie on score
 * (e.g. a model with two realtime rows) the cheaper input price wins so the
 * catalog's "from $X/M" aggregation stays stable.
 *
 * Returns `null` when the model has no user-facing tariffs at all — callers
 * render a dash in that case.
 */
export function pickTariffForContext(
  tariffs: ModelTariff[] | undefined | null,
  context: PricingContext,
): ModelTariff | null {
  if (!tariffs || tariffs.length === 0) return null;
  const visible = tariffs.filter(
    (t) => t.api_key_purpose !== "playground" && t.is_active !== false,
  );
  if (visible.length === 0) return null;

  let best: ModelTariff | null = null;
  let bestScore = Infinity;
  let bestPrice = Infinity;
  for (const t of visible) {
    const score = tariffPriority(t, context);
    if (score > bestScore) continue;
    const price = parseFloat(t.input_price_per_token);
    const priceKey = Number.isFinite(price) ? price : Infinity;
    if (score < bestScore || priceKey < bestPrice) {
      best = t;
      bestScore = score;
      bestPrice = priceKey;
    }
  }
  return best;
}

export interface AggregatedFamily {
  key: string;
  label: string;
  /** Provider key shared by all children (or the first child's provider if mixed). */
  provider: string;
  providerLabel: string;
  providerIcon?: string | null;
  variants: Model[];
  /** Min / max intelligence index across variants. */
  intelligenceMin: number | null;
  intelligenceMax: number | null;
  /** Max context window across variants. */
  contextMax: number | null;
  /** Cheapest input price (per token) across variants for the current pricing context. */
  priceFrom: number | null;
  /** Latest release date across variants (YYYY-MM-DD). */
  releasedAt: string | null;
  /** Union of display capabilities across variants. */
  capabilities: string[];
  /** True if any child was released within the "new" window. */
  hasNewVariant: boolean;
}

export interface AggregateOptions {
  newCutoff: string; // YYYY-MM-DD
  context: PricingContext;
  providerLabelOf: (provider: string) => string;
  providerIconOf: (provider: string) => string | null | undefined;
  displayCapabilitiesOf: (model: Model) => string[];
}

/**
 * Reduce a flat list of models into family groups ordered by a sensible
 * default (latest release first, then label ascending).
 */
export function aggregateFamilies(
  models: Model[],
  options: AggregateOptions,
): AggregatedFamily[] {
  const map = new Map<string, AggregatedFamily>();

  for (const model of models) {
    const info = getFamilyInfo(model);
    const providerKey = (model.metadata?.provider?.trim() || "Other").toLowerCase();
    const providerLabel = options.providerLabelOf(providerKey);
    const providerIcon = options.providerIconOf(providerKey);

    let family = map.get(info.key);
    if (!family) {
      family = {
        key: info.key,
        label: info.label,
        provider: providerKey,
        providerLabel,
        providerIcon,
        variants: [],
        intelligenceMin: null,
        intelligenceMax: null,
        contextMax: null,
        priceFrom: null,
        releasedAt: null,
        capabilities: [],
        hasNewVariant: false,
      };
      map.set(info.key, family);
    }

    family.variants.push(model);

    const intel = model.metadata?.intelligence_index ?? null;
    if (intel != null) {
      family.intelligenceMin =
        family.intelligenceMin == null
          ? intel
          : Math.min(family.intelligenceMin, intel);
      family.intelligenceMax =
        family.intelligenceMax == null
          ? intel
          : Math.max(family.intelligenceMax, intel);
    }

    const ctx = model.metadata?.context_window ?? null;
    if (ctx != null) {
      family.contextMax =
        family.contextMax == null ? ctx : Math.max(family.contextMax, ctx);
    }

    const tariff = pickTariffForContext(model.tariffs, options.context);
    if (tariff) {
      const price = parseFloat(tariff.input_price_per_token);
      if (Number.isFinite(price)) {
        family.priceFrom =
          family.priceFrom == null ? price : Math.min(family.priceFrom, price);
      }
    }

    const released = model.metadata?.released_at ?? null;
    if (released) {
      family.releasedAt =
        !family.releasedAt || released > family.releasedAt
          ? released
          : family.releasedAt;
      if (released >= options.newCutoff) {
        family.hasNewVariant = true;
      }
    }

    for (const cap of options.displayCapabilitiesOf(model)) {
      if (!family.capabilities.includes(cap)) {
        family.capabilities.push(cap);
      }
    }
  }

  const result = Array.from(map.values());
  result.sort((a, b) => {
    if (a.releasedAt && b.releasedAt && a.releasedAt !== b.releasedAt) {
      return b.releasedAt.localeCompare(a.releasedAt);
    }
    if (a.releasedAt && !b.releasedAt) return -1;
    if (!a.releasedAt && b.releasedAt) return 1;
    return a.label.localeCompare(b.label);
  });

  // Sort variants within each family: by intelligence desc, then size desc
  for (const family of result) {
    family.variants.sort((a, b) => {
      const ai = a.metadata?.intelligence_index ?? -Infinity;
      const bi = b.metadata?.intelligence_index ?? -Infinity;
      if (ai !== bi) return bi - ai;
      return a.alias.localeCompare(b.alias);
    });
  }

  return result;
}
