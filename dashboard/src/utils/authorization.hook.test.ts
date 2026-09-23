import { renderHook } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import type { User } from "../api/control-layer/types";
import { useAuthorization } from "./authorization";

const fixture = vi.hoisted(() => ({
  user: undefined as Pick<User, "roles" | "is_admin"> | undefined,
}));

vi.mock("../api/control-layer/hooks", () => ({
  useUser: () => ({ data: fixture.user, isLoading: false }),
  useConfig: () => ({ data: { batches: { enabled: false } }, isLoading: false }),
}));

describe("useAuthorization legacy admin capabilities", () => {
  it.each([
    { roles: ["StandardUser"], is_admin: true, allowed: true },
    { roles: ["PlatformManager"], is_admin: false, allowed: true },
    { roles: ["StandardUser"], is_admin: false, allowed: false },
  ] as const)("honours roles $roles and legacy flag $is_admin", ({ roles, is_admin, allowed }) => {
    fixture.user = { roles: [...roles], is_admin };
    const { result } = renderHook(() => useAuthorization());
    expect(result.current.hasPermission("users-groups")).toBe(allowed);
    expect(result.current.canAccessRoute("/users-groups")).toBe(allowed);
    expect(result.current.canAccessRoute("/batches")).toBe(false);
    expect(result.current.userRoles).toEqual(is_admin ? [...roles, "PlatformManager"] : roles);
    // Derived manager capabilities must not mutate the stored role array.
    expect(fixture.user.roles).toEqual(roles);
  });

  it("does not expose manager controls before a user is loaded", () => {
    fixture.user = undefined;
    const { result } = renderHook(() => useAuthorization());
    expect(result.current.hasPermission("users-groups")).toBe(false);
  });
});
