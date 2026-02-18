/**
 * File size constants
 */
export const FILE_SIZE_LIMITS = {
  MAX_FILE_SIZE_MB: 200,
  MAX_FILE_SIZE_BYTES: 200 * 1024 * 1024,
  LARGE_FILE_WARNING_MB: 50,
  LARGE_FILE_WARNING_BYTES: 50 * 1024 * 1024,
} as const;

/**
 * Result type for file validation using discriminated union
 */
export type FileValidationResult =
  | { isValid: true }
  | { isValid: false; error: string };

/**
 * Validate a file for upload
 * @param file - File to validate
 * @returns Discriminated union indicating validation success or failure with error
 */
export function validateBatchFile(file: File): FileValidationResult {
  if (!file.name.toLowerCase().endsWith(".jsonl")) {
    return { isValid: false, error: "Please upload a .jsonl file" };
  }

  if (file.size > FILE_SIZE_LIMITS.MAX_FILE_SIZE_BYTES) {
    return {
      isValid: false,
      error: `File size exceeds ${FILE_SIZE_LIMITS.MAX_FILE_SIZE_MB}MB limit`,
    };
  }

  return { isValid: true };
}
