import React, { useState, useEffect } from "react";
import { SettingsContext, type SettingsContextType } from "./context";
import type { FeatureFlags, AppSettings } from "./types";

/** LocalStorage key for persisting settings */
const STORAGE_KEY = "app-settings";

/** Default application settings */
const DEFAULT_SETTINGS: AppSettings = {
  apiBaseUrl: "/admin/api/v1",
  features: {
    demo: false,
    devUser: false,
  },
  impersonateUserHeader: "admin@example.com",
};

/**
 * Parse feature flags from URL query parameters
 * Supports: ?flags=demo,devUser&user=admin@example.com
 */
function parseUrlFlags(): Partial<AppSettings> {
  const urlParams = new URLSearchParams(window.location.search);
  const settings: Partial<AppSettings> = {};

  // Handle flags parameter (comma-separated list)
  const flagsParam = urlParams.get("flags");
  if (flagsParam !== null) {
    const flagList = flagsParam.split(",").map((f) => f.trim());

    settings.features = {
      demo: flagList.includes("demo"),
      devUser: flagList.includes("devUser"),
    };
  }

  // Handle user parameter for impersonation
  const userParam = urlParams.get("user");
  if (userParam !== null) {
    settings.impersonateUserHeader = userParam;
  }

  return settings;
}

/**
 * Load settings with priority: URL params > localStorage > defaults
 */
function loadSettings(): AppSettings {
  const urlSettings = parseUrlFlags();

  const storedSettings = localStorage.getItem(STORAGE_KEY);
  let localSettings: Partial<AppSettings> = {};

  if (storedSettings) {
    try {
      localSettings = JSON.parse(storedSettings);
    } catch {
      console.warn("Failed to parse stored settings, using defaults");
    }
  }

  return {
    apiBaseUrl:
      import.meta.env.VITE_API_BASE_URL ||
      localSettings.apiBaseUrl ||
      DEFAULT_SETTINGS.apiBaseUrl,
    features: {
      demo:
        urlSettings.features?.demo ??
        localSettings.features?.demo ??
        DEFAULT_SETTINGS.features.demo,
      devUser:
        urlSettings.features?.devUser ??
        localSettings.features?.devUser ??
        DEFAULT_SETTINGS.features.devUser,
    },
    impersonateUserHeader:
      urlSettings.impersonateUserHeader ??
      localSettings.impersonateUserHeader ??
      DEFAULT_SETTINGS.impersonateUserHeader,
  };
}

/**
 * Save settings to localStorage
 */
function saveSettings(settings: AppSettings): void {
  localStorage.setItem(STORAGE_KEY, JSON.stringify(settings));
}

/**
 * Settings context provider component
 * Manages application settings and feature flags with localStorage persistence
 */
export function SettingsProvider({ children }: { children: React.ReactNode }) {
  const [settings, setSettings] = useState<AppSettings>(loadSettings);
  const [isMswReady, setIsMswReady] = useState(false);

  useEffect(() => {
    saveSettings(settings);
  }, [settings]);

  // Reset MSW ready state when demo mode is toggled
  useEffect(() => {
    setIsMswReady(!settings.features.demo);
  }, [settings.features.demo]);

  const toggleFeature = async (
    feature: keyof FeatureFlags,
    enabled: boolean,
  ) => {
    setSettings((prev) => ({
      ...prev,
      features: {
        ...prev.features,
        [feature]: enabled,
      },
    }));

    // Handle service worker for demo mode
    if (feature === "demo") {
      if (!enabled && "serviceWorker" in navigator) {
        const registrations = await navigator.serviceWorker.getRegistrations();
        for (const registration of registrations) {
          await registration.unregister();
        }
        window.location.reload();
      } else if (
        enabled &&
        !("serviceWorker" in navigator && navigator.serviceWorker.controller)
      ) {
        window.location.reload();
      }
    }
  };

  const isFeatureEnabled = (feature: keyof FeatureFlags): boolean => {
    return settings.features[feature];
  };

  const setImpersonateUser = (email: string) => {
    setSettings((prev) => ({
      ...prev,
      impersonateUserHeader: email,
    }));
  };

  const setMswReady = (ready: boolean) => {
    setIsMswReady(ready);
  };

  const contextValue: SettingsContextType = {
    settings,
    toggleFeature,
    isFeatureEnabled,
    setImpersonateUser,
    isMswReady,
    setMswReady,
  };

  return (
    <SettingsContext.Provider value={contextValue}>
      {children}
    </SettingsContext.Provider>
  );
}
