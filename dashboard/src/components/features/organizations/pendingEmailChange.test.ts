import { describe, it, expect } from "vitest";
import { describeWaitingOn, waitingOn } from "./pendingEmailChange";

const base = {
  new_email: "new-contact@acme.com",
  expires_at: "2025-06-02T12:00:00Z",
};

describe("waitingOn", () => {
  it("lists both addresses when neither side has confirmed", () => {
    expect(waitingOn(base, "old-contact@acme.com")).toEqual([
      "old-contact@acme.com",
      "new-contact@acme.com",
    ]);
    expect(describeWaitingOn(base, "old-contact@acme.com")).toBe(
      "waiting on both old-contact@acme.com and new-contact@acme.com to confirm",
    );
  });

  it("lists only the new address once the current one has confirmed", () => {
    const pending = { ...base, old_email_confirmed_at: "2025-06-01T13:00:00Z" };
    expect(waitingOn(pending, "old-contact@acme.com")).toEqual(["new-contact@acme.com"]);
    expect(describeWaitingOn(pending, "old-contact@acme.com")).toBe(
      "waiting on new-contact@acme.com to confirm",
    );
  });

  it("lists only the current address once the new one has confirmed", () => {
    const pending = { ...base, new_email_confirmed_at: "2025-06-01T13:00:00Z" };
    expect(waitingOn(pending, "old-contact@acme.com")).toEqual(["old-contact@acme.com"]);
    expect(describeWaitingOn(pending, "old-contact@acme.com")).toBe(
      "waiting on old-contact@acme.com to confirm",
    );
  });

  it("reports nothing outstanding when both have confirmed", () => {
    const pending = {
      ...base,
      new_email_confirmed_at: "2025-06-01T13:00:00Z",
      old_email_confirmed_at: "2025-06-01T13:05:00Z",
    };
    expect(waitingOn(pending, "old-contact@acme.com")).toEqual([]);
    expect(describeWaitingOn(pending, "old-contact@acme.com")).toMatch(/confirmed/);
  });
});
