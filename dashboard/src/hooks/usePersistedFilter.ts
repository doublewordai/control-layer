import { useCallback } from "react";
import { useSearchParams } from "react-router-dom";

const STORAGE_KEY_PREFIX = "filters:";

type FilterValue = string | string[];
type Updater<T extends FilterValue> = T | ((prev: T) => T);

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
 * Clear specific persisted filter params from URL and localStorage for a scope.
 *
 * Only removes the named keys — anything else stored under the same scope
 * is left alone, so unrelated callers sharing the scope aren't affected by a
 * "clear filters" action.
 */
export function clearPersistedFilters(
  scope: string,
  setSearchParams: ReturnType<typeof useSearchParams>[1],
  paramNames: string[],
) {
  const defaults = loadDefaults(scope);
  let mutated = false;
  for (const name of paramNames) {
    if (name in defaults) {
      delete defaults[name];
      mutated = true;
    }
  }
  if (mutated) {
    if (Object.keys(defaults).length === 0) {
      localStorage.removeItem(storageKey(scope));
    } else {
      localStorage.setItem(storageKey(scope), JSON.stringify(defaults));
    }
  }

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
 * Supports both single-value (string) and multi-value (string[]) filters,
 * and accepts either a value or an updater function `(prev) => next` so
 * callers can safely update based on the current value without risking
 * a stale closure when several updates land in the same render.
 *
 * @example
 * ```tsx
 * const [provider, setProvider] = usePersistedFilter("models", "endpoint", "all");
 * const [groups, setGroups] = usePersistedFilter("models", "groups", EMPTY_GROUPS);
 * setGroups((prev) => prev.includes(id) ? prev.filter(x => x !== id) : [...prev, id]);
 * ```
 */
export function usePersistedFilter(
  scope: string,
  paramName: string,
  fallback: string,
): [string, (value: Updater<string>) => void];
export function usePersistedFilter(
  scope: string,
  paramName: string,
  fallback: string[],
): [string[], (value: Updater<string[]>) => void];
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
    (next: Updater<FilterValue>) => {
      setSearchParams(
        (prev) => {
          // Compute the resolved current value from the *latest* URL +
          // localStorage state, not from the closure — this is what makes
          // the updater form safe across rapid successive setState calls.
          const currentDefaults = loadDefaults(scope);
          const currentUrlValue = prev.get(paramName);
          let current: FilterValue;
          if (currentUrlValue !== null) {
            current = isArray
              ? currentUrlValue === ""
                ? []
                : currentUrlValue.split(",")
              : currentUrlValue;
          } else if (paramName in currentDefaults) {
            current = currentDefaults[paramName];
          } else {
            current = fallback;
          }

          const resolved =
            typeof next === "function"
              ? (next as (prev: FilterValue) => FilterValue)(current)
              : next;

          saveDefault(scope, paramName, resolved, fallback);

          const out = new URLSearchParams(prev);
          const serialized = serialize(resolved);
          const fallbackSerialized = serialize(fallback);

          if (serialized === fallbackSerialized) {
            out.delete(paramName);
          } else {
            out.set(paramName, serialized);
          }
          return out;
        },
        { replace: true },
      );
    },
    [scope, paramName, fallback, isArray, setSearchParams],
  );

  return [value, setValue];
}
