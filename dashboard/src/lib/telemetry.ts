/**
 * Vendor-neutral telemetry facade.
 *
 * This module is the single entry point for reporting client-side errors,
 * events, and user identity from the dashboard. It deliberately does not
 * import or reference any specific telemetry SDK. Instead, it dispatches
 * `CustomEvent`s on `window` that an external bootstrap script can subscribe
 * to and forward to whichever backend an operator chooses (self-hosted or
 * otherwise).
 *
 * Operators who want telemetry can add a listener like:
 *
 *     window.addEventListener("dashboard:telemetry", (event) => {
 *       const { type, payload } = event.detail;
 *       // forward to your telemetry backend
 *     });
 *
 * If no listener is attached the dispatches are harmless no-ops, so the
 * dashboard ships with zero telemetry by default.
 */

export type TelemetryEventType =
  | "exception"
  | "event"
  | "measurement"
  | "identify"
  | "context";

export interface TelemetryExceptionPayload {
  message: string;
  name: string;
  stack?: string;
  /** Where this exception was caught (react boundary, window.onerror, ...). */
  source:
    | "react-error-boundary"
    | "react-route-boundary"
    | "window-error"
    | "unhandled-rejection"
    | "manual";
  /** React component stack if available. */
  componentStack?: string;
  /** Route pathname at the time the exception was captured. */
  route?: string;
  /** Arbitrary extra context selected by the caller. */
  context?: Record<string, unknown>;
}

export interface TelemetryEventPayload {
  name: string;
  properties?: Record<string, unknown>;
}

export interface TelemetryMeasurementPayload {
  name: string;
  value: number;
  unit?: string;
  properties?: Record<string, unknown>;
}

export interface TelemetryIdentifyPayload {
  /** Stable opaque identifier for the current user, or null to reset. */
  id: string | null;
  /** Non-PII traits only. Email and similar should not be sent here. */
  traits?: Record<string, unknown>;
}

export interface TelemetryContextPayload {
  context: Record<string, unknown>;
}

export type TelemetryDetail =
  | { type: "exception"; payload: TelemetryExceptionPayload }
  | { type: "event"; payload: TelemetryEventPayload }
  | { type: "measurement"; payload: TelemetryMeasurementPayload }
  | { type: "identify"; payload: TelemetryIdentifyPayload }
  | { type: "context"; payload: TelemetryContextPayload };

export const TELEMETRY_EVENT_NAME = "dashboard:telemetry";

function dispatch(detail: TelemetryDetail) {
  if (typeof window === "undefined") {
    return;
  }
  try {
    window.dispatchEvent(
      new CustomEvent<TelemetryDetail>(TELEMETRY_EVENT_NAME, { detail }),
    );
  } catch (err) {
    // Telemetry must never break the app. Surface in dev only.
    if (import.meta.env.DEV) {
      console.warn("telemetry dispatch failed", err);
    }
  }
}

function normalizeError(error: unknown): {
  message: string;
  name: string;
  stack?: string;
} {
  if (error instanceof Error) {
    return {
      message: error.message,
      name: error.name,
      stack: error.stack,
    };
  }
  return {
    message: typeof error === "string" ? error : "Unknown error",
    name: "NonError",
  };
}

/** Report an exception caught by an error boundary or global handler. */
export function captureException(
  error: unknown,
  options: {
    source: TelemetryExceptionPayload["source"];
    componentStack?: string;
    context?: Record<string, unknown>;
  },
): void {
  const normalized = normalizeError(error);
  const route =
    typeof window !== "undefined" ? window.location?.pathname : undefined;

  dispatch({
    type: "exception",
    payload: {
      ...normalized,
      source: options.source,
      componentStack: options.componentStack,
      route,
      context: options.context,
    },
  });
}

/** Record a discrete application event. */
export function trackEvent(
  name: string,
  properties?: Record<string, unknown>,
): void {
  dispatch({ type: "event", payload: { name, properties } });
}

/** Record a numeric measurement (e.g. a web vital or a timing). */
export function recordMeasurement(
  name: string,
  value: number,
  options: { unit?: string; properties?: Record<string, unknown> } = {},
): void {
  dispatch({
    type: "measurement",
    payload: { name, value, unit: options.unit, properties: options.properties },
  });
}

/**
 * Associate subsequent telemetry with a user. Pass `null` to clear.
 * The `id` should be an opaque identifier such as a user UUID — avoid PII.
 */
export function identifyUser(
  id: string | null,
  traits?: Record<string, unknown>,
): void {
  dispatch({ type: "identify", payload: { id, traits } });
}

/**
 * Attach global context (build SHA, environment, route, ...) that should be
 * included with every subsequent telemetry event. Callers should prefer this
 * over passing context to every `captureException` call.
 */
export function setTelemetryContext(context: Record<string, unknown>): void {
  dispatch({ type: "context", payload: { context } });
}
