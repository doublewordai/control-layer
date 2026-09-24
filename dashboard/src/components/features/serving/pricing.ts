import type {
  CachePricing,
  ModelTariff,
  OrganizationCacheTariff,
  TariffApiKeyPurpose,
} from "@/api/control-layer/types";

export function currentTariffs(
  rows: ModelTariff[],
  now = Date.now(),
): ModelTariff[] {
  return rows.filter(
    (row) =>
      Date.parse(row.valid_from) <= now &&
      (!row.valid_until || Date.parse(row.valid_until) > now),
  );
}

/** Display the same scope/purpose ordering as billing; an explicit zero is a match. */
export function resolveTokenPrice(
  rows: ModelTariff[],
  organization: string | undefined,
  servingClass: string,
  purpose: TariffApiKeyPurpose,
  window: string | null = null,
  now = Date.now(),
) {
  const resolvedClass = purpose === "batch" ? "standard" : servingClass;
  const candidates = currentTariffs(rows, now).filter(
    (row) =>
      (row.organization_id == null || row.organization_id === organization) &&
      (row.serving_class == null ||
        (organization !== undefined && row.serving_class === resolvedClass)) &&
      (row.api_key_purpose === purpose ||
        ((purpose === "playground" || purpose === "batch") &&
          row.api_key_purpose === "realtime")) &&
      (row.completion_window ?? null) ===
        (row.api_key_purpose === "batch" ? window : null),
  );
  const scope = (row: ModelTariff) =>
    row.organization_id ? (row.serving_class ? 0 : 1) : 2;
  // Exhaust exact batch prices in every scope before the realtime safety net.
  const phase = (row: ModelTariff) =>
    Number(purpose === "batch" && row.api_key_purpose === "realtime");
  return candidates.sort(
    (a, b) =>
      phase(a) - phase(b) ||
      scope(a) - scope(b) ||
      Number(a.api_key_purpose !== purpose) -
        Number(b.api_key_purpose !== purpose) ||
      Date.parse(b.valid_from) - Date.parse(a.valid_from) ||
      (a.id < b.id ? -1 : a.id > b.id ? 1 : 0),
  )[0];
}

export function priceSource(
  row:
    | { organization_id?: string | null; serving_class?: string | null }
    | undefined,
  selected: boolean,
): string {
  if (!row) return "No matching tariff";
  if (row.organization_id)
    return row.serving_class
      ? `Bespoke · ${row.serving_class}`
      : "Bespoke · all classes";
  return selected ? "Inherited · general model" : "General model";
}

export function resolveCachePrice(
  general: CachePricing | undefined,
  rows: OrganizationCacheTariff[],
  servingClass: string,
  now = Date.now(),
) {
  // An organisation multiplier cannot enable caching without a general cache tariff.
  if (!general?.enabled) return undefined;
  const current = rows
    .filter((row) => Date.parse(row.valid_from) <= now)
    .sort((a, b) => Date.parse(b.valid_from) - Date.parse(a.valid_from));
  const own =
    current.find((row) => row.serving_class === servingClass) ??
    current.find((row) => !row.serving_class);
  return { values: own ?? general, own };
}
