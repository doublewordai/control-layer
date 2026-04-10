import { toast } from "sonner";

/**
 * Writes text to the system clipboard, handling the cases where the Clipboard
 * API is unavailable (e.g. non-secure contexts, some mobile browsers) or the
 * user has denied permission. On failure, a toast is shown and `false` is
 * returned so callers can skip any success UI.
 */
export async function copyToClipboard(
  text: string,
  options: { successMessage?: string; errorMessage?: string } = {},
): Promise<boolean> {
  const errorMessage = options.errorMessage ?? "Failed to copy to clipboard";

  if (typeof navigator === "undefined" || !navigator.clipboard) {
    toast.error(errorMessage);
    return false;
  }

  try {
    await navigator.clipboard.writeText(text);
    if (options.successMessage) {
      toast.success(options.successMessage);
    }
    return true;
  } catch (error) {
    console.error("Failed to copy to clipboard:", error);
    toast.error(errorMessage);
    return false;
  }
}
