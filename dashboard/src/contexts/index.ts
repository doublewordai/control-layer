/**
 * Context Modules
 *
 * Provides unified context systems for application state management.
 */

// Settings Context - feature flags and app configuration
export { SettingsProvider } from "./settings/SettingsContext";
export { useSettings } from "./settings/hooks";
export type { FeatureFlags, AppSettings } from "./settings/types";

// User types (moved to clay API types)
// Note: User context replaced with TanStack Query hooks in api/clay
