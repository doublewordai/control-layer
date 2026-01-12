/**
 * Batch-related utility functions
 */

export type BatchFileType = "input" | "output" | "error";

/**
 * Generates a standardized filename for batch file downloads
 * @param batchId - The batch ID
 * @param fileType - The type of file (input, output, or error)
 * @returns A standardized filename like "batch_abc123_output.jsonl"
 */
export function getBatchDownloadFilename(
  batchId: string,
  fileType: BatchFileType,
): string {
  return `batch_${batchId}_${fileType}.jsonl`;
}

/**
 * Downloads a file from the API and saves it with the specified filename
 * @param endpoint - The API endpoint to fetch the file from
 * @param filename - The filename to save the file as
 * @throws Error if the download fails
 */
export async function downloadFile(
  endpoint: string,
  filename: string,
): Promise<void> {
  const response = await fetch(endpoint, {
    headers: {
      Authorization: `Bearer ${localStorage.getItem("auth_token") || ""}`,
    },
  });

  if (!response.ok) {
    throw new Error(`Download failed: ${response.status}`);
  }

  const blob = await response.blob();
  const url = window.URL.createObjectURL(blob);
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  document.body.appendChild(a);
  a.click();
  window.URL.revokeObjectURL(url);
  document.body.removeChild(a);
}
