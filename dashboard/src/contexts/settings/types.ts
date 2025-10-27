/**
 * Available feature flags that can be toggled in the application
 */
export interface FeatureFlags {
  /** Enable demo mode with mock data and service worker */
  demo: boolean;
  /** Auto-generate conversation titles by summarizing the first message */
  autoSummarizeTitles: boolean;
}

/**
 * Configuration for demo mode behavior
 */
export interface DemoConfig {
  /** Custom response template for playground chat completions */
  customResponse?: string;
  /** Whether to include the user's message in the response */
  includeUserMessage?: boolean;
}

/**
 * Complete application settings configuration
 */
export interface AppSettings {
  /** Base URL for API requests */
  apiBaseUrl: string;
  /** Feature flag toggles */
  features: FeatureFlags;
  /** Demo mode configuration */
  demoConfig?: DemoConfig;
}
