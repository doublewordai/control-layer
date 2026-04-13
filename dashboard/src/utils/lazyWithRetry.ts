import { lazy, type ComponentType, type LazyExoticComponent } from "react";

type AnyComponent = ComponentType<any>;

const RELOAD_KEY = "dashboard-chunk-reload-attempted";

// In-memory fallback guard for environments where sessionStorage is completely
// unavailable (Safari private mode, restrictive CSP). Without this, storageGet
// always returns null and storageSet is a no-op, creating an infinite reload loop.
let reloadAttempted = false;

// sessionStorage can throw SecurityError in Safari private mode and some
// restrictive CSP environments even when `typeof sessionStorage !== "undefined"`.
// These helpers swallow those errors so the retry/reload logic never crashes.
function storageGet(key: string): string | null {
  try {
    return sessionStorage.getItem(key);
  } catch {
    return null;
  }
}
function storageSet(key: string, value: string): void {
  try {
    sessionStorage.setItem(key, value);
  } catch {
    // storage unavailable — reload guard will be skipped
  }
}
function storageRemove(key: string): void {
  try {
    sessionStorage.removeItem(key);
  } catch {
    // storage unavailable — no-op
  }
}

/**
 * Wraps `React.lazy` with resilience for failed dynamic imports.
 *
 * A dynamic import can fail for two common reasons:
 *   1. Transient network errors — these recover on a simple retry.
 *   2. The user is on stale HTML after a deploy, so the old chunk hashes no
 *      longer exist on the server. A hard reload pulls fresh HTML that
 *      references the current chunks.
 *
 * We try once, retry once in-place, and only then fall back to a single hard
 * reload (guarded by sessionStorage to prevent reload loops). On a successful
 * load we clear the reload flag so the mechanism is available again later in
 * the session.
 */
export function lazyWithRetry<T extends AnyComponent>(
  factory: () => Promise<{ default: T }>,
): LazyExoticComponent<T> {
  return lazy(async () => {
    try {
      const mod = await factory();
      reloadAttempted = false;
      storageRemove(RELOAD_KEY);
      return mod;
    } catch (initialError) {
      try {
        const mod = await factory();
        reloadAttempted = false;
        storageRemove(RELOAD_KEY);
        return mod;
      } catch (retryError) {
        if (
          typeof window !== "undefined" &&
          !reloadAttempted &&
          !storageGet(RELOAD_KEY)
        ) {
          reloadAttempted = true;
          storageSet(RELOAD_KEY, "1");
          window.location.reload();
          // Return a never-resolving promise so Suspense keeps showing its
          // fallback until the reload takes effect, rather than surfacing the
          // error to the boundary.
          return new Promise<{ default: T }>(() => {});
        }
        console.error("Failed to load route chunk:", initialError, retryError);
        throw retryError;
      }
    }
  });
}
