import { describe, it, expect } from "vitest";
import { splitDwImgTokens, dwImageUrl } from "./dwImage";

const SHA = "a".repeat(64);
const SHA2 = "b".repeat(64);

describe("splitDwImgTokens", () => {
  it("returns a single text segment when there are no tokens", () => {
    expect(splitDwImgTokens('{"x":1}')).toEqual([{ kind: "text", value: '{"x":1}' }]);
  });

  it("splits a token out of surrounding text", () => {
    expect(splitDwImgTokens(`"url": "dw-img://${SHA}"`)).toEqual([
      { kind: "text", value: '"url": "' },
      { kind: "token", raw: `dw-img://${SHA}`, sha256: SHA },
      { kind: "text", value: '"' },
    ]);
  });

  it("handles multiple tokens and preserves order", () => {
    const segs = splitDwImgTokens(`a dw-img://${SHA} b dw-img://${SHA2} c`);
    expect(segs).toEqual([
      { kind: "text", value: "a " },
      { kind: "token", raw: `dw-img://${SHA}`, sha256: SHA },
      { kind: "text", value: " b " },
      { kind: "token", raw: `dw-img://${SHA2}`, sha256: SHA2 },
      { kind: "text", value: " c" },
    ]);
  });

  it("handles a token at the very start and end", () => {
    expect(splitDwImgTokens(`dw-img://${SHA}`)).toEqual([
      { kind: "token", raw: `dw-img://${SHA}`, sha256: SHA },
    ]);
  });

  it("ignores malformed tokens (wrong hash length)", () => {
    const text = `dw-img://${"a".repeat(10)}`;
    expect(splitDwImgTokens(text)).toEqual([{ kind: "text", value: text }]);
  });

  it("lower-cases the captured hash", () => {
    const upper = "A".repeat(64);
    const segs = splitDwImgTokens(`dw-img://${upper}`);
    expect(segs).toEqual([{ kind: "token", raw: `dw-img://${upper}`, sha256: SHA }]);
  });
});

describe("dwImageUrl", () => {
  it("builds the management-API image path", () => {
    expect(dwImageUrl(SHA)).toBe(`/admin/api/v1/images/${SHA}`);
  });
});
