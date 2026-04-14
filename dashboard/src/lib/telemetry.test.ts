import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import {
  captureException,
  identifyUser,
  recordMeasurement,
  setTelemetryContext,
  trackEvent,
  TELEMETRY_EVENT_NAME,
  type TelemetryDetail,
} from "./telemetry";

describe("telemetry facade", () => {
  let handler: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    handler = vi.fn();
    window.addEventListener(
      TELEMETRY_EVENT_NAME,
      handler as EventListener,
    );
  });

  afterEach(() => {
    window.removeEventListener(
      TELEMETRY_EVENT_NAME,
      handler as EventListener,
    );
  });

  function lastDetail(): TelemetryDetail {
    const call = handler.mock.calls.at(-1);
    if (!call) {
      throw new Error("no telemetry event dispatched");
    }
    return (call[0] as CustomEvent<TelemetryDetail>).detail;
  }

  it("dispatches exceptions with normalized error fields", () => {
    const error = new Error("boom");
    captureException(error, { source: "manual" });

    expect(handler).toHaveBeenCalledOnce();
    const detail = lastDetail();
    expect(detail.type).toBe("exception");
    if (detail.type !== "exception") return;
    expect(detail.payload.name).toBe("Error");
    expect(detail.payload.message).toBe("boom");
    expect(detail.payload.source).toBe("manual");
    expect(detail.payload.stack).toBeDefined();
    expect(detail.payload.route).toBeDefined();
  });

  it("normalizes non-Error throwables", () => {
    captureException("plain string", { source: "manual" });
    const detail = lastDetail();
    if (detail.type !== "exception") throw new Error("wrong type");
    expect(detail.payload.name).toBe("NonError");
    expect(detail.payload.message).toBe("plain string");
  });

  it("forwards component stack and context on exception", () => {
    captureException(new Error("x"), {
      source: "react-error-boundary",
      componentStack: "at Foo\nat Bar",
      context: { tab: "analytics" },
    });
    const detail = lastDetail();
    if (detail.type !== "exception") throw new Error("wrong type");
    expect(detail.payload.componentStack).toBe("at Foo\nat Bar");
    expect(detail.payload.context).toEqual({ tab: "analytics" });
  });

  it("dispatches events, measurements, identify and context", () => {
    trackEvent("route.change", { to: "/models" });
    recordMeasurement("fcp", 1234, { unit: "ms" });
    identifyUser("u-1", { roles: ["StandardUser"] });
    setTelemetryContext({ build_sha: "abc" });

    expect(handler).toHaveBeenCalledTimes(4);
    const types = handler.mock.calls.map(
      (c) => (c[0] as CustomEvent<TelemetryDetail>).detail.type,
    );
    expect(types).toEqual(["event", "measurement", "identify", "context"]);
  });

  it("is a no-op when no listener is attached", () => {
    window.removeEventListener(
      TELEMETRY_EVENT_NAME,
      handler as EventListener,
    );
    expect(() => captureException(new Error("x"), { source: "manual" })).not.toThrow();
    expect(handler).not.toHaveBeenCalled();
  });

  it("accepts null identify payload to clear user", () => {
    identifyUser(null);
    const detail = lastDetail();
    if (detail.type !== "identify") throw new Error("wrong type");
    expect(detail.payload.id).toBeNull();
  });
});
