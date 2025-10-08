import { useContext } from "react";
import { SettingsContext, type SettingsContextType } from "./context";

/**
 * Hook to access application settings and feature flags
 * @throws {Error} When used outside of SettingsProvider
 * @returns Settings context with current settings and toggle functions
 */
export function useSettings(): SettingsContextType {
  const context = useContext(SettingsContext);
  if (context === undefined) {
    throw new Error("useSettings must be used within a SettingsProvider");
  }
  return context;
}
