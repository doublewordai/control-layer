/**
 * Available feature flags that can be toggled in the application
 */
export interface FeatureFlags {
  /** Enable demo mode with mock data and service worker */
  demo: boolean;
  /** Enable development user switcher for testing different roles */
  devUser: boolean;
}

/**
 * Complete application settings configuration
 */
export interface AppSettings {
  /** Base URL for API requests */
  apiBaseUrl: string;
  /** Feature flag toggles */
  features: FeatureFlags;
  /** User email to impersonate via X-Doubleword-User header in development */
  impersonateUserHeader?: string;
}
