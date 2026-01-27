import { describe, it, expect } from "vitest";
import {
  hasPermission,
  canAccessRoute,
  getFirstAccessibleRoute,
} from "./authorization";

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

  it("returns true for unknown routes (no permission required)", () => {
    expect(canAccessRoute(["StandardUser"], "/unknown-route")).toBe(true);
    expect(canAccessRoute([], "/some-new-page")).toBe(true);
  });
});

describe("getFirstAccessibleRoute", () => {
  it("returns /batches as first choice for PlatformManager", () => {
    expect(getFirstAccessibleRoute(["PlatformManager"])).toBe("/batches");
  });

  it("returns /batches as first choice for BatchAPIUser", () => {
    expect(getFirstAccessibleRoute(["BatchAPIUser"])).toBe("/batches");
  });

  it("returns /models as first choice for StandardUser (no batches access)", () => {
    expect(getFirstAccessibleRoute(["StandardUser"])).toBe("/models");
  });

  it("returns /models for RequestViewer who can access models but not batches", () => {
    expect(getFirstAccessibleRoute(["RequestViewer"])).toBe("/models");
  });

  it("returns /profile as fallback for empty roles", () => {
    expect(getFirstAccessibleRoute([])).toBe("/profile");
  });
});
