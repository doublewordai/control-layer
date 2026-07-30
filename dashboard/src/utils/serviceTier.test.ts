import { describe, expect, it } from "vitest";
import {
  getCompletionWindowLabel,
  getServiceTierLabel,
} from "./serviceTier";

describe("service tier labels", () => {
  it("labels background service tiers", () => {
    expect(getServiceTierLabel("background")).toBe("Background");
  });

  it("labels background completion windows", () => {
    expect(getCompletionWindowLabel("background", "1h")).toBe("Background");
  });
});
