/**
 * Format file size in bytes to human-readable string
 * @param bytes - File size in bytes
 * @returns Formatted file size (e.g., "1.5 MB", "500 KB")
 */
export function formatFileSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(2)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(2)} MB`;
}

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
 * Validate a file for upload
 * @param file - File to validate
 * @returns Object with isValid boolean and optional error message
 */
export function validateBatchFile(file: File): { isValid: boolean; error?: string } {
  if (!file.name.endsWith(".jsonl")) {
    return { isValid: false, error: "Please upload a .jsonl file" };
  }
  
  if (file.size > FILE_SIZE_LIMITS.MAX_FILE_SIZE_BYTES) {
    return { isValid: false, error: `File size exceeds ${FILE_SIZE_LIMITS.MAX_FILE_SIZE_MB}MB limit` };
  }
  
  return { isValid: true };
}