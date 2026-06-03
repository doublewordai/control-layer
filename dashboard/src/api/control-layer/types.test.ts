import { describe, it, expect } from "vitest";
import { providerIconUrl } from "./types";

describe("providerIconUrl", () => {
  it("routes https:// URLs through the icon proxy", () => {
    expect(
      providerIconUrl(
        "openai",
        "https://registry.npmmirror.com/@lobehub/icons-static-svg/latest/files/icons/openai.svg",
      ),
    ).toBe("/admin/api/v1/provider-display-configs/openai/icon");
  });

  it("URL-encodes the provider key", () => {
    expect(providerIconUrl("meta llama", "https://example.com/x.svg")).toBe(
      "/admin/api/v1/provider-display-configs/meta%20llama/icon",
    );
  });

  it("passes root-relative paths through unchanged", () => {
    expect(providerIconUrl("anthropic", "/endpoints/anthropic.svg")).toBe(
      "/endpoints/anthropic.svg",
    );
  });

  it("passes registry-key strings through so CatalogIcon's local registry resolves them", () => {
    expect(providerIconUrl("anthropic", "anthropic")).toBe("anthropic");
    expect(providerIconUrl("openai", "openai")).toBe("openai");
  });

  it("returns undefined for null / empty / whitespace icon values", () => {
    expect(providerIconUrl("openai", null)).toBeUndefined();
    expect(providerIconUrl("openai", undefined)).toBeUndefined();
    expect(providerIconUrl("openai", "")).toBeUndefined();
    expect(providerIconUrl("openai", "   ")).toBeUndefined();
  });

  it("trims whitespace before deciding on the path", () => {
    expect(providerIconUrl("openai", "  https://example.com/x.svg  ")).toBe(
      "/admin/api/v1/provider-display-configs/openai/icon",
    );
  });
});
