import { useCallback } from "react";
import { useSearchParams } from "react-router-dom";

const STORAGE_KEY_PREFIX = "filters:";

type FilterValue = string | string[];

function storageKey(scope: string): string {
  return `${STORAGE_KEY_PREFIX}${scope}`;
}

/**
 * Read all persisted filter defaults from localStorage for a given scope.
 */
function loadDefaults(scope: string): Record<string, FilterValue> {
  try {
    const stored = localStorage.getItem(storageKey(scope));
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
function saveDefault(
  scope: string,
  key: string,
  value: FilterValue,
  fallback: FilterValue,
) {
  const defaults = loadDefaults(scope);
  const isDefault = JSON.stringify(value) === JSON.stringify(fallback);
  if (isDefault) {
    delete defaults[key];
  } else {
    defaults[key] = value;
  }
  if (Object.keys(defaults).length === 0) {
    localStorage.removeItem(storageKey(scope));
  } else {
    localStorage.setItem(storageKey(scope), JSON.stringify(defaults));
  }
}

function serialize(value: FilterValue): string {
  return Array.isArray(value) ? value.join(",") : value;
}

/**
 * Clear all persisted filter params from URL and localStorage for a scope.
 * Useful for a "clear all filters" action that avoids the race condition
 * of calling multiple individual setters back-to-back.
 */
export function clearPersistedFilters(
  scope: string,
  setSearchParams: ReturnType<typeof useSearchParams>[1],
  paramNames: string[],
) {
  localStorage.removeItem(storageKey(scope));

  setSearchParams(
    (prev) => {
      const next = new URLSearchParams(prev);
      for (const name of paramNames) {
        next.delete(name);
      }
      return next;
    },
    { replace: true },
  );
}

/**
 * Hook for filter state that uses URL search params as source of truth,
 * falling back to localStorage-persisted defaults when a param is absent.
 *
 * - URL params present -> use them (shareable links work)
 * - URL params absent -> fall back to localStorage defaults
 * - Changes are written to both URL params and localStorage
 *
 * Each call must specify a `scope` so that different pages don't collide
 * in localStorage (e.g. `"models"`, `"batches"`, `"responses"`).
 *
 * Supports both single-value (string) and multi-value (string[]) filters.
 *
 * @example
 * ```tsx
 * const [provider, setProvider] = usePersistedFilter("models", "endpoint", "all");
 * const [groups, setGroups] = usePersistedFilter("models", "groups", EMPTY_GROUPS);
 * ```
 */
export function usePersistedFilter(
  scope: string,
  paramName: string,
  fallback: string,
): [string, (value: string) => void];
export function usePersistedFilter(
  scope: string,
  paramName: string,
  fallback: string[],
): [string[], (value: string[]) => void];
export function usePersistedFilter(
  scope: string,
  paramName: string,
  fallback: any,
): any {
  const [searchParams, setSearchParams] = useSearchParams();
  const isArray = Array.isArray(fallback);

  const defaults = loadDefaults(scope);
  const urlValue = searchParams.get(paramName);

  let value: FilterValue;
  if (urlValue !== null) {
    if (isArray) {
      value = urlValue === "" ? [] : urlValue.split(",");
    } else {
      value = urlValue;
    }
  } else if (paramName in defaults) {
    value = defaults[paramName];
  } else {
    value = fallback;
  }

  const setValue = useCallback(
    (newValue: string | string[]) => {
      saveDefault(scope, paramName, newValue, fallback);

      setSearchParams(
        (prev) => {
          const next = new URLSearchParams(prev);
          const serialized = serialize(newValue);
          const fallbackSerialized = serialize(fallback);

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
    [scope, paramName, fallback, setSearchParams],
  );

  return [value, setValue];
}
