import { describe, it, expect } from "vitest";
import { cn } from "./common";

describe("common utils", () => {
  describe("cn", () => {
    it("should merge class names", () => {
      const result = cn("foo", "bar");
      expect(result).toBe("foo bar");
    });

    it("should handle conditional classes", () => {
      const shouldInclude = false;
      const result = cn("foo", shouldInclude && "bar", "baz");
      expect(result).toBe("foo baz");
    });

    it("should handle empty input", () => {
      const result = cn();
      expect(result).toBe("");
    });
  });
});
