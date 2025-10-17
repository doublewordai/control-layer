import { createContext } from "react";
import type { FeatureFlags, AppSettings, DemoConfig } from "./types";

/**
 * Context type for the Settings system
 */
export interface SettingsContextType {
  /** Current application settings */
  settings: AppSettings;
  /** Toggle a feature flag on/off */
  toggleFeature: (feature: keyof FeatureFlags, enabled: boolean) => void;
  /** Check if a feature flag is currently enabled */
  isFeatureEnabled: (feature: keyof FeatureFlags) => boolean;
  /** Update demo mode configuration */
  updateDemoConfig: (config: Partial<DemoConfig>) => void;
  /** Whether MSW (Mock Service Worker) is ready to handle requests */
  isMswReady: boolean;
  /** Set MSW ready state */
  setMswReady: (ready: boolean) => void;
}

/**
 * React context for application settings and feature flags
 */
export const SettingsContext = createContext<SettingsContextType | undefined>(
  undefined,
);
