import { useCallback } from "react";
import { useSearchParams } from "react-router-dom";

const STORAGE_KEY = "models-filters";

type FilterValue = string | string[];

interface FilterDefaults {
  [key: string]: FilterValue;
}

/**
 * Read all persisted filter defaults from localStorage.
 */
function loadDefaults(): Record<string, FilterValue> {
  try {
    const stored = localStorage.getItem(STORAGE_KEY);
    if (stored) {
      const parsed = JSON.parse(stored);
      if (parsed && typeof parsed === "object") return parsed;
    }
  } catch {
    // ignore corrupt data
  }
  return {};
}

/**
 * Save a single filter key to the persisted defaults.
 * Removes the key when value equals the provided default.
 */
function saveDefault(key: string, value: FilterValue, fallback: FilterValue) {
  const defaults = loadDefaults();
  const isDefault =
    JSON.stringify(value) === JSON.stringify(fallback);
  if (isDefault) {
    delete defaults[key];
  } else {
    defaults[key] = value;
  }
  if (Object.keys(defaults).length === 0) {
    localStorage.removeItem(STORAGE_KEY);
  } else {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(defaults));
  }
}

/**
 * Hook for filter state that uses URL search params as source of truth,
 * falling back to localStorage-persisted defaults when a param is absent.
 *
 * - URL params present → use them (shareable links work)
 * - URL params absent → fall back to localStorage defaults
 * - Changes are written to both URL params and localStorage
 *
 * Supports both single-value (string) and multi-value (string[]) filters.
 *
 * @example
 * ```tsx
 * const [provider, setProvider] = usePersistedFilter("endpoint", "all");
 * const [groups, setGroups] = usePersistedFilter("groups", []);
 * ```
 */
export function usePersistedFilter(
  paramName: string,
  fallback: string,
): [string, (value: string) => void];
export function usePersistedFilter(
  paramName: string,
  fallback: string[],
): [string[], (value: string[]) => void];
export function usePersistedFilter(
  paramName: string,
  fallback: FilterValue,
): [FilterValue, (value: FilterValue) => void] {
  const [searchParams, setSearchParams] = useSearchParams();
  const isArray = Array.isArray(fallback);

  const defaults = loadDefaults();
  const urlValue = searchParams.get(paramName);

  let value: FilterValue;
  if (urlValue !== null) {
    // URL param present — parse it
    if (isArray) {
      value = urlValue === "" ? [] : urlValue.split(",");
    } else {
      value = urlValue;
    }
  } else if (paramName in defaults) {
    // No URL param — use persisted default
    value = defaults[paramName];
  } else {
    // Nothing persisted — use fallback
    value = fallback;
  }

  const setValue = useCallback(
    (newValue: FilterValue) => {
      // Persist to localStorage as the new default
      saveDefault(paramName, newValue, fallback);

      // Update URL params
      setSearchParams(
        (prev) => {
          const next = new URLSearchParams(prev);
          const serialized = Array.isArray(newValue)
            ? newValue.join(",")
            : newValue;
          const fallbackSerialized = Array.isArray(fallback)
            ? fallback.join(",")
            : fallback;

          if (serialized === fallbackSerialized) {
            next.delete(paramName);
          } else {
            next.set(paramName, serialized);
          }
          return next;
        },
        { replace: true },
      );
    },
    [paramName, fallback, setSearchParams],
  );

  return [value, setValue] as [FilterValue, (value: FilterValue) => void];
}
