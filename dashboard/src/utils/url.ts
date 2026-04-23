/**
 * Allowlist of query parameter prefixes/names preserved across redirects.
 * Only marketing and tracking params survive; everything else is dropped
 * to avoid leaking sensitive values (tokens, codes, etc.) into target URLs.
 */
const PRESERVED_PARAM_PREFIXES = ["utm_"];
const PRESERVED_PARAM_NAMES = new Set(["gclid", "fbclid", "ref", "source"]);

function isPreservedParam(key: string): boolean {
  if (PRESERVED_PARAM_NAMES.has(key)) return true;
  return PRESERVED_PARAM_PREFIXES.some((prefix) => key.startsWith(prefix));
}

/**
 * Merges preserved query params (marketing/tracking) onto a target URL path,
 * excluding the `redirect` param and any sensitive keys.
 *
 * @param target - The redirect target path (may already contain a query string)
 * @param searchParams - The current URL search params to filter and merge
 * @returns The target path with preserved params appended
 */
export function mergePreservedParams(
  target: string,
  searchParams: URLSearchParams,
): string {
  const preserved = new URLSearchParams();
  searchParams.forEach((value, key) => {
    if (key !== "redirect" && isPreservedParam(key)) {
      preserved.set(key, value);
    }
  });

  const qs = preserved.toString();
  if (!qs) return target;

  const separator = target.includes("?") ? "&" : "?";
  return `${target}${separator}${qs}`;
}
