import { describe, it, expect } from "vitest";
import {
  ORG_MEMBER_ROLES,
  formatOrgRoleForDisplay,
  getOrgRoleDescription,
} from "./roles";

describe("organization role helpers", () => {
  it("orders roles from least to most privileged", () => {
    expect(ORG_MEMBER_ROLES).toEqual(["member", "admin", "owner"]);
  });

  it("formats org roles for display", () => {
    expect(formatOrgRoleForDisplay("member")).toBe("Member");
    expect(formatOrgRoleForDisplay("admin")).toBe("Admin");
    expect(formatOrgRoleForDisplay("owner")).toBe("Owner");
  });

  it("provides a non-empty description for every org role", () => {
    for (const role of ORG_MEMBER_ROLES) {
      expect(getOrgRoleDescription(role).length).toBeGreaterThan(0);
    }
  });

  it("describes the owner-only ability to promote owners", () => {
    expect(getOrgRoleDescription("owner")).toMatch(/Owner role/i);
    expect(getOrgRoleDescription("admin")).toMatch(/cannot promote/i);
  });
});
