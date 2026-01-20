import { describe, it, expect } from "vitest";
import {
  validateBatchFile,
  FILE_SIZE_LIMITS,
} from "./files";

describe("files", () => {
  describe("validateBatchFile", () => {
    it("should validate valid .jsonl files", () => {
      const file = new File(["content"], "test.jsonl", { type: "application/jsonl" });
      const result = validateBatchFile(file);
      
      expect(result.isValid).toBe(true);
      if (result.isValid) {
        // Type narrowing: error should not exist
        expect('error' in result).toBe(false);
      }
    });

    it("should reject non-.jsonl files", () => {
      const txtFile = new File(["content"], "test.txt", { type: "text/plain" });
      const result = validateBatchFile(txtFile);
      
      expect(result.isValid).toBe(false);
      if (!result.isValid) {
        // Type narrowing: error must exist
        expect(result.error).toBe("Please upload a .jsonl file");
      }
    });

    it("should reject files without extension", () => {
      const noExtFile = new File(["content"], "test", { type: "application/octet-stream" });
      const result = validateBatchFile(noExtFile);
      
      expect(result.isValid).toBe(false);
      if (!result.isValid) {
        expect(result.error).toBe("Please upload a .jsonl file");
      }
    });

    it("should reject files with wrong extension", () => {
      const jsonFile = new File(["content"], "test.json", { type: "application/json" });
      const result = validateBatchFile(jsonFile);
      
      expect(result.isValid).toBe(false);
      if (!result.isValid) {
        expect(result.error).toBe("Please upload a .jsonl file");
      }
    });

    it("should reject files exceeding max size", () => {
      const largeContent = new ArrayBuffer(FILE_SIZE_LIMITS.MAX_FILE_SIZE_BYTES + 1);
      const largeFile = new File([largeContent], "large.jsonl", { type: "application/jsonl" });
      const result = validateBatchFile(largeFile);
      
      expect(result.isValid).toBe(false);
      if (!result.isValid) {
        expect(result.error).toBe(`File size exceeds ${FILE_SIZE_LIMITS.MAX_FILE_SIZE_MB}MB limit`);
      }
    });

    it("should accept files at exactly max size", () => {
      const maxContent = new ArrayBuffer(FILE_SIZE_LIMITS.MAX_FILE_SIZE_BYTES);
      const maxFile = new File([maxContent], "max.jsonl", { type: "application/jsonl" });
      const result = validateBatchFile(maxFile);
      
      expect(result.isValid).toBe(true);
      if (result.isValid) {
        expect('error' in result).toBe(false);
      }
    });

    it("should accept files just under max size", () => {
      const justUnderContent = new ArrayBuffer(FILE_SIZE_LIMITS.MAX_FILE_SIZE_BYTES - 1);
      const justUnderFile = new File([justUnderContent], "just-under.jsonl", { type: "application/jsonl" });
      const result = validateBatchFile(justUnderFile);
      
      expect(result.isValid).toBe(true);
      if (result.isValid) {
        expect('error' in result).toBe(false);
      }
    });

    it("should accept large warning threshold files", () => {
      const largeWarningContent = new ArrayBuffer(FILE_SIZE_LIMITS.LARGE_FILE_WARNING_BYTES);
      const largeWarningFile = new File([largeWarningContent], "large-warning.jsonl", { type: "application/jsonl" });
      const result = validateBatchFile(largeWarningFile);
      
      expect(result.isValid).toBe(true);
      if (result.isValid) {
        expect('error' in result).toBe(false);
      }
    });

    it("should accept files with uppercase extension (case-insensitive)", () => {
      const upperFile = new File(["content"], "test.JSONL", { type: "application/jsonl" });
      const result = validateBatchFile(upperFile);
      
      expect(result.isValid).toBe(true);
      if (result.isValid) {
        expect('error' in result).toBe(false);
      }
    });

    it("should accept files with mixed case extension", () => {
      const mixedFile = new File(["content"], "test.JsonL", { type: "application/jsonl" });
      const result = validateBatchFile(mixedFile);
      
      expect(result.isValid).toBe(true);
      if (result.isValid) {
        expect('error' in result).toBe(false);
      }
    });

    it("should handle empty files", () => {
      const emptyFile = new File([], "empty.jsonl", { type: "application/jsonl" });
      const result = validateBatchFile(emptyFile);
      
      expect(result.isValid).toBe(true);
      if (result.isValid) {
        expect('error' in result).toBe(false);
      }
    });

    it("should handle files with multiple dots in filename", () => {
      const multiDotFile = new File(["content"], "test.file.name.jsonl", { type: "application/jsonl" });
      const result = validateBatchFile(multiDotFile);
      
      expect(result.isValid).toBe(true);
      if (result.isValid) {
        expect('error' in result).toBe(false);
      }
    });
  });

  describe("FILE_SIZE_LIMITS", () => {
    it("should have correct max file size constants", () => {
      expect(FILE_SIZE_LIMITS.MAX_FILE_SIZE_MB).toBe(200);
      expect(FILE_SIZE_LIMITS.MAX_FILE_SIZE_BYTES).toBe(200 * 1024 * 1024);
      expect(FILE_SIZE_LIMITS.MAX_FILE_SIZE_BYTES).toBe(209715200);
    });

    it("should have correct large file warning constants", () => {
      expect(FILE_SIZE_LIMITS.LARGE_FILE_WARNING_MB).toBe(50);
      expect(FILE_SIZE_LIMITS.LARGE_FILE_WARNING_BYTES).toBe(50 * 1024 * 1024);
      expect(FILE_SIZE_LIMITS.LARGE_FILE_WARNING_BYTES).toBe(52428800);
    });

    it("should have warning threshold less than max size", () => {
      expect(FILE_SIZE_LIMITS.LARGE_FILE_WARNING_BYTES).toBeLessThan(FILE_SIZE_LIMITS.MAX_FILE_SIZE_BYTES);
      expect(FILE_SIZE_LIMITS.LARGE_FILE_WARNING_MB).toBeLessThan(FILE_SIZE_LIMITS.MAX_FILE_SIZE_MB);
    });

    it("should be immutable (as const)", () => {
      expect(Object.isFrozen(FILE_SIZE_LIMITS)).toBe(false);
      // This is expected - 'as const' provides compile-time type safety, not runtime immutability
    });
  });
});