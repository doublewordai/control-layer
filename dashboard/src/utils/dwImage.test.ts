import { describe, it, expect } from "vitest";
import { extractDwImageShas, dwImageUrl } from "./dwImage";

const SHA = "a".repeat(64);
const SHA2 = "b".repeat(64);

describe("extractDwImageShas", () => {
  it("extracts a token from a chat-completions body object", () => {
    const body = {
      messages: [
        {
          role: "user",
          content: [
            { type: "text", text: "describe" },
            { type: "image_url", image_url: { url: `dw-img://${SHA}` } },
          ],
        },
      ],
    };
    expect(extractDwImageShas(body)).toEqual([SHA]);
  });

  it("extracts a token from the responses shape (bare image_url string)", () => {
    const body = {
      input: [{ content: [{ type: "input_image", image_url: `dw-img://${SHA}` }] }],
    };
    expect(extractDwImageShas(body)).toEqual([SHA]);
  });

  it("extracts a token from a raw JSON string body", () => {
    expect(extractDwImageShas(JSON.stringify({ url: `dw-img://${SHA}` }))).toEqual([SHA]);
  });

  it("collects multiple distinct tokens and de-duplicates repeats", () => {
    const body = { a: `dw-img://${SHA}`, b: `dw-img://${SHA}`, c: `dw-img://${SHA2}` };
    expect(extractDwImageShas(body).sort()).toEqual([SHA, SHA2].sort());
  });

  it("returns [] when no tokens are present", () => {
    expect(extractDwImageShas({ messages: [{ content: "no images here" }] })).toEqual([]);
  });

  it("ignores plain http and data: URLs", () => {
    const body = { a: "https://example.com/x.png", b: "data:image/png;base64,AAAA" };
    expect(extractDwImageShas(body)).toEqual([]);
  });

  it("ignores malformed tokens (wrong hash length)", () => {
    expect(extractDwImageShas(`dw-img://${"a".repeat(10)}`)).toEqual([]);
  });

  it("handles null and undefined", () => {
    expect(extractDwImageShas(null)).toEqual([]);
    expect(extractDwImageShas(undefined)).toEqual([]);
  });
});

describe("dwImageUrl", () => {
  it("builds the management-API image path", () => {
    expect(dwImageUrl(SHA)).toBe(`/admin/api/v1/images/${SHA}`);
  });
});
