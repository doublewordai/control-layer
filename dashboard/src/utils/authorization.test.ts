import { describe, it, expect } from "vitest";
import {
  hasPermission,
  canAccessRoute,
  getFirstAccessibleRoute,
} from "./authorization";

const configWithAsyncEnabled = {
  region: "test-region",
  organization: "test-org",
  payment_enabled: true,
  docs_url: "https://example.com/docs",
  onwards: { strict_mode: false },
  batches: {
    enabled: true,
    allowed_completion_windows: ["1h", "24h"],
    allowed_url_paths: [],
    async_requests: {
      enabled: true,
      completion_window: "1h",
    },
  },
};

const configWithAsyncDisabled = {
  ...configWithAsyncEnabled,
  batches: {
    ...configWithAsyncEnabled.batches,
    async_requests: {
      ...configWithAsyncEnabled.batches.async_requests,
      enabled: false,
    },
  },
};

const configWithBatchesDisabled = {
  ...configWithAsyncEnabled,
  batches: {
    ...configWithAsyncEnabled.batches,
    enabled: false,
  },
};

describe("hasPermission", () => {
  it("returns true when user has a role that grants permission", () => {
    expect(hasPermission(["PlatformManager"], "models")).toBe(true);
    expect(hasPermission(["PlatformManager"], "users-groups")).toBe(true);
    expect(hasPermission(["StandardUser"], "playground")).toBe(true);
    expect(hasPermission(["RequestViewer"], "analytics")).toBe(true);
  });

  it("returns false when user does not have a role that grants permission", () => {
    expect(hasPermission(["StandardUser"], "users-groups")).toBe(false);
    expect(hasPermission(["StandardUser"], "analytics")).toBe(false);
    expect(hasPermission(["RequestViewer"], "batches")).toBe(false);
  });

  it("returns true when any role in the array grants permission", () => {
    expect(hasPermission(["StandardUser", "RequestViewer"], "analytics")).toBe(
      true,
    );
    expect(hasPermission(["StandardUser", "BatchAPIUser"], "batches")).toBe(
      true,
    );
  });

  it("returns false for empty roles array", () => {
    expect(hasPermission([], "models")).toBe(false);
  });

  it("handles BatchAPIUser role correctly", () => {
    expect(hasPermission(["BatchAPIUser"], "batches")).toBe(true);
    expect(hasPermission(["BatchAPIUser"], "api-keys")).toBe(true);
    expect(hasPermission(["BatchAPIUser"], "analytics")).toBe(false);
  });

  it("handles ConnectionsUser role correctly", () => {
    // ConnectionsUser only grants connections + batches
    expect(hasPermission(["ConnectionsUser"], "connections")).toBe(true);
    expect(hasPermission(["ConnectionsUser"], "batches")).toBe(true);
    // Everything else comes from StandardUser, not ConnectionsUser
    expect(hasPermission(["ConnectionsUser"], "profile")).toBe(false);
    expect(hasPermission(["ConnectionsUser"], "api-keys")).toBe(false);
    expect(hasPermission(["ConnectionsUser"], "models")).toBe(false);
    expect(hasPermission(["ConnectionsUser"], "analytics")).toBe(false);
    expect(hasPermission(["ConnectionsUser"], "users-groups")).toBe(false);
  });

  it("ConnectionsUser combined with StandardUser grants both permission sets", () => {
    // Roles are additive — StandardUser brings models/api-keys/playground,
    // ConnectionsUser adds connections/batches
    expect(
      hasPermission(["StandardUser", "ConnectionsUser"], "connections"),
    ).toBe(true);
    expect(
      hasPermission(["StandardUser", "ConnectionsUser"], "models"),
    ).toBe(true);
    expect(
      hasPermission(["StandardUser", "ConnectionsUser"], "playground"),
    ).toBe(true);
  });
});

describe("canAccessRoute", () => {
  it("returns true for routes user has permission to access", () => {
    expect(canAccessRoute(["PlatformManager"], "/models")).toBe(true);
    expect(canAccessRoute(["PlatformManager"], "/users-groups")).toBe(true);
    expect(canAccessRoute(["StandardUser"], "/api-keys")).toBe(true);
  });

  it("returns false for routes user does not have permission to access", () => {
    expect(canAccessRoute(["StandardUser"], "/users-groups")).toBe(false);
    expect(canAccessRoute(["StandardUser"], "/analytics")).toBe(false);
  });

  it("allows ConnectionsUser to access /connections and /batches", () => {
    expect(canAccessRoute(["ConnectionsUser"], "/connections")).toBe(true);
    expect(canAccessRoute(["ConnectionsUser"], "/batches")).toBe(true);
  });

  it("denies async and batches routes when disabled in config", () => {
    expect(
      canAccessRoute(["ConnectionsUser"], "/async", configWithAsyncDisabled),
    ).toBe(false);
    expect(
      canAccessRoute(["ConnectionsUser"], "/batches", configWithBatchesDisabled),
    ).toBe(false);
  });

  it("denies ConnectionsUser access to admin routes", () => {
    expect(canAccessRoute(["ConnectionsUser"], "/users-groups")).toBe(false);
    expect(canAccessRoute(["ConnectionsUser"], "/analytics")).toBe(false);
    expect(canAccessRoute(["ConnectionsUser"], "/endpoints")).toBe(false);
    expect(canAccessRoute(["ConnectionsUser"], "/settings")).toBe(false);
  });

  it("returns true for unknown routes (no permission required)", () => {
    expect(canAccessRoute(["StandardUser"], "/unknown-route")).toBe(true);
    expect(canAccessRoute([], "/some-new-page")).toBe(true);
  });
});

describe("getFirstAccessibleRoute", () => {
  it("returns /models as first choice for PlatformManager", () => {
    expect(getFirstAccessibleRoute(["PlatformManager"])).toBe("/models");
  });

  it("returns /models as first choice for BatchAPIUser", () => {
    expect(getFirstAccessibleRoute(["BatchAPIUser"])).toBe("/models");
  });

  it("returns /models as first choice for StandardUser", () => {
    expect(getFirstAccessibleRoute(["StandardUser"])).toBe("/models");
  });

  it("returns /models for RequestViewer who can access models but not async or batches", () => {
    expect(getFirstAccessibleRoute(["RequestViewer"])).toBe("/models");
  });

  it("returns /async before /batches when models are unavailable", () => {
    expect(
      getFirstAccessibleRoute(["ConnectionsUser"], configWithAsyncEnabled),
    ).toBe("/async");
  });

  it("skips /async when async requests are disabled", () => {
    expect(
      getFirstAccessibleRoute(["ConnectionsUser"], configWithAsyncDisabled),
    ).toBe("/batches");
  });

  it("falls back to /profile when both async and batches are disabled", () => {
    expect(
      getFirstAccessibleRoute(["ConnectionsUser"], configWithBatchesDisabled),
    ).toBe("/profile");
  });

  it("returns /profile as fallback for empty roles", () => {
    expect(getFirstAccessibleRoute([])).toBe("/profile");
  });
});
