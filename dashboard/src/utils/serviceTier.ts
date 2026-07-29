export function getServiceTierLabel(tier: string | null | undefined): string {
  if (!tier) return "Unknown";

  const labels: Record<string, string> = {
    realtime: "Realtime",
    priority: "Priority",
    flex: "Flex",
    async: "Async",
    batch: "Batch",
    background: "Background",
  };

  return labels[tier] ?? tier.charAt(0).toUpperCase() + tier.slice(1);
}

export function getCompletionWindowLabel(
  completionWindow: string,
  asyncWindow = "1h",
): string {
  if (completionWindow === "background") return "Background";
  if (completionWindow === "0s") return "Realtime";
  if (completionWindow === asyncWindow) return "Async";
  if (completionWindow === "24h") return "Batch";
  return "Batch";
}
