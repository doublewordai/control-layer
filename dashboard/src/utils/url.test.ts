import { describe, it, expect } from "vitest";
import { mergePreservedParams } from "./url";

describe("mergePreservedParams fragment handling", () => {
  it("places utm params in the query string when target has no fragment", () => {
    const out = mergePreservedParams(
      "/users-groups",
      new URLSearchParams("utm_source=newsletter"),
    );
    const u = new URL(out, "http://localhost");
    expect(u.searchParams.get("utm_source")).toBe("newsletter");
    expect(u.hash).toBe("");
  });

  it("places utm params in the query string when target has a query AND fragment", () => {
    const out = mergePreservedParams(
      "/users-groups?tab=teams#pending",
      new URLSearchParams("utm_source=newsletter"),
    );
    const u = new URL(out, "http://localhost");
    expect(u.searchParams.get("utm_source")).toBe("newsletter");
    expect(u.searchParams.get("tab")).toBe("teams");
    expect(u.hash).toBe("#pending");
  });

  it("places utm params in the query string when target has only a fragment", () => {
    const out = mergePreservedParams(
      "/users-groups#pending",
      new URLSearchParams("utm_source=newsletter"),
    );
    const u = new URL(out, "http://localhost");
    expect(u.searchParams.get("utm_source")).toBe("newsletter");
    expect(u.hash).toBe("#pending");
  });

  it("places utm params before the fragment, not inside it", () => {
    const out = mergePreservedParams(
      "/users-groups?tab=teams#pending",
      new URLSearchParams("utm_source=newsletter&utm_medium=email"),
    );
    const u = new URL(out, "http://localhost");
    expect(u.searchParams.get("utm_source")).toBe("newsletter");
    expect(u.searchParams.get("utm_medium")).toBe("email");
    expect(u.searchParams.get("tab")).toBe("teams");
    expect(u.hash).toBe("#pending");
    expect(u.search).toBe("?tab=teams&utm_source=newsletter&utm_medium=email");
  });

  it("preserves the entire fragment when it contains multiple # characters", () => {
    const out = mergePreservedParams(
      "/users-groups#pending#section",
      new URLSearchParams("utm_source=newsletter"),
    );
    const u = new URL(out, "http://localhost");
    expect(u.searchParams.get("utm_source")).toBe("newsletter");
    expect(u.hash).toBe("#pending#section");
  });
});

describe("mergePreservedParams separator selection", () => {
  it("appends with ? when target has no query string", () => {
    const out = mergePreservedParams(
      "/users-groups",
      new URLSearchParams("utm_source=newsletter"),
    );
    expect(out).toBe("/users-groups?utm_source=newsletter");
  });

  it("appends with & when target already has a query string", () => {
    const out = mergePreservedParams(
      "/users-groups?tab=teams",
      new URLSearchParams("utm_source=newsletter"),
    );
    expect(out).toBe("/users-groups?tab=teams&utm_source=newsletter");
  });

  it("appends with & when target has a query string and a fragment", () => {
    const out = mergePreservedParams(
      "/users-groups?tab=teams#pending",
      new URLSearchParams("utm_source=newsletter"),
    );
    expect(out).toBe("/users-groups?tab=teams&utm_source=newsletter#pending");
  });

  it("appends with ? when target has only a fragment (no query)", () => {
    const out = mergePreservedParams(
      "/users-groups#pending",
      new URLSearchParams("utm_source=newsletter"),
    );
    expect(out).toBe("/users-groups?utm_source=newsletter#pending");
  });
});

describe("mergePreservedParams param filtering", () => {
  it("preserves utm_ prefixed params", () => {
    const out = mergePreservedParams(
      "/users-groups",
      new URLSearchParams(
        "utm_source=newsletter&utm_medium=email&utm_campaign=launch",
      ),
    );
    const u = new URL(out, "http://localhost");
    expect(u.searchParams.get("utm_source")).toBe("newsletter");
    expect(u.searchParams.get("utm_medium")).toBe("email");
    expect(u.searchParams.get("utm_campaign")).toBe("launch");
  });

  it("preserves named params: gclid, fbclid, ref, source", () => {
    const out = mergePreservedParams(
      "/users-groups",
      new URLSearchParams("gclid=abc&fbclid=def&ref=docs&source=ad"),
    );
    const u = new URL(out, "http://localhost");
    expect(u.searchParams.get("gclid")).toBe("abc");
    expect(u.searchParams.get("fbclid")).toBe("def");
    expect(u.searchParams.get("ref")).toBe("docs");
    expect(u.searchParams.get("source")).toBe("ad");
  });

  it("drops non-preserved params (e.g. tokens, codes) to avoid leaking sensitive values", () => {
    const out = mergePreservedParams(
      "/users-groups",
      new URLSearchParams("token=secret&code=1234&random=value"),
    );
    const u = new URL(out, "http://localhost");
    expect(u.searchParams.get("token")).toBeNull();
    expect(u.searchParams.get("code")).toBeNull();
    expect(u.searchParams.get("random")).toBeNull();
  });

  it("excludes the redirect param even alongside preserved params", () => {
    const out = mergePreservedParams(
      "/users-groups",
      new URLSearchParams(
        "redirect=/dashboard&utm_source=newsletter&token=leak",
      ),
    );
    const u = new URL(out, "http://localhost");
    expect(u.searchParams.get("redirect")).toBeNull();
    expect(u.searchParams.get("token")).toBeNull();
    expect(u.searchParams.get("utm_source")).toBe("newsletter");
  });

  it("preserves utm_redirect (utm_ prefix) while excluding exact redirect key", () => {
    const out = mergePreservedParams(
      "/users-groups",
      new URLSearchParams("redirect=/home&utm_redirect=keep"),
    );
    const u = new URL(out, "http://localhost");
    expect(u.searchParams.get("redirect")).toBeNull();
    expect(u.searchParams.get("utm_redirect")).toBe("keep");
  });

  it("keeps only the last value for repeated preserved params", () => {
    const out = mergePreservedParams(
      "/users-groups",
      new URLSearchParams("utm_source=a&utm_source=b"),
    );
    const u = new URL(out, "http://localhost");
    expect(u.searchParams.get("utm_source")).toBe("b");
  });

  it("merges preserved params onto existing target query without clobbering it", () => {
    const out = mergePreservedParams(
      "/users-groups?tab=teams&filter=active",
      new URLSearchParams("utm_source=newsletter&gclid=xyz"),
    );
    const u = new URL(out, "http://localhost");
    expect(u.searchParams.get("tab")).toBe("teams");
    expect(u.searchParams.get("filter")).toBe("active");
    expect(u.searchParams.get("utm_source")).toBe("newsletter");
    expect(u.searchParams.get("gclid")).toBe("xyz");
  });
});

describe("mergePreservedParams no-op / early return", () => {
  it("returns the target unchanged when there are no preserved params", () => {
    const out = mergePreservedParams(
      "/users-groups?tab=teams",
      new URLSearchParams("token=secret&random=value"),
    );
    expect(out).toBe("/users-groups?tab=teams");
  });

  it("returns the target unchanged (including fragment) when nothing is preserved", () => {
    const out = mergePreservedParams(
      "/users-groups?tab=teams#pending",
      new URLSearchParams("token=secret"),
    );
    expect(out).toBe("/users-groups?tab=teams#pending");
  });

  it("returns the target unchanged when searchParams is empty", () => {
    const out = mergePreservedParams(
      "/users-groups#pending",
      new URLSearchParams(),
    );
    expect(out).toBe("/users-groups#pending");
  });

  it("excludes redirect-only input without altering target fragment", () => {
    const out = mergePreservedParams(
      "/users-groups#pending",
      new URLSearchParams("redirect=/home"),
    );
    expect(out).toBe("/users-groups#pending");
  });
});

describe("mergePreservedParams edge cases", () => {
  it("handles an empty target string", () => {
    const out = mergePreservedParams(
      "",
      new URLSearchParams("utm_source=newsletter"),
    );
    const u = new URL(out, "http://localhost");
    expect(u.searchParams.get("utm_source")).toBe("newsletter");
  });

  it("handles a target that is only a fragment", () => {
    const out = mergePreservedParams(
      "#pending",
      new URLSearchParams("utm_source=newsletter"),
    );
    const u = new URL(out, "http://localhost");
    expect(u.searchParams.get("utm_source")).toBe("newsletter");
    expect(u.hash).toBe("#pending");
  });

  it("handles a target with an empty query string (trailing ?)", () => {
    const out = mergePreservedParams(
      "/users-groups?",
      new URLSearchParams("utm_source=newsletter"),
    );
    const u = new URL(out, "http://localhost");
    expect(u.searchParams.get("utm_source")).toBe("newsletter");
  });

  it("handles a registered-onboarding redirect target (query + fragment)", () => {
    const out = mergePreservedParams(
      "/org-invite?token=abc#step-2",
      new URLSearchParams("utm_source=newsletter&gclid=xyz&token=leak"),
    );
    const u = new URL(out, "http://localhost");
    expect(u.searchParams.get("utm_source")).toBe("newsletter");
    expect(u.searchParams.get("gclid")).toBe("xyz");
    expect(u.searchParams.get("token")).toBe("abc");
    expect(u.hash).toBe("#step-2");
  });

  it("accepts an encoded-fragment redirect target as produced by LoginForm/AuthGuard callers", () => {
    // Simulates redirect=%2Fusers-groups%23pending decoded by searchParams.get("redirect")
    const redirect = decodeURIComponent("/users-groups%23pending");
    const out = mergePreservedParams(
      redirect,
      new URLSearchParams("utm_source=newsletter"),
    );
    const u = new URL(out, "http://localhost");
    expect(u.searchParams.get("utm_source")).toBe("newsletter");
    expect(u.hash).toBe("#pending");
  });
});
