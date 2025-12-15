export function formatMemory(memoryGb: number): string {
  // Guard against invalid numbers
  if (!isFinite(memoryGb) || isNaN(memoryGb)) return "N/A";
  if (memoryGb < 0) return "0GB";

  if (memoryGb >= 1000) {
    return `${(memoryGb / 1000).toFixed(1)}TB`;
  }
  return `${memoryGb}GB`;
}

export function formatNumber(num: number): string {
  // Guard against invalid numbers
  if (!isFinite(num) || isNaN(num)) return "N/A";
  if (num < 0) return "0";

  if (num >= 1_000_000_000) {
    return `${(num / 1_000_000_000).toFixed(1)}B`;
  }
  if (num >= 1_000_000) {
    return `${(num / 1_000_000).toFixed(1)}M`;
  }
  if (num >= 1_000) {
    return `${(num / 1_000).toFixed(1)}K`;
  }
  return num.toString();
}

export function formatTokens(tokens: number): string {
  // Guard against invalid numbers
  if (!isFinite(tokens) || isNaN(tokens)) return "N/A";
  if (tokens < 0) return "0";

  if (tokens >= 1_000_000) return `${(tokens / 1_000_000).toFixed(1)}M`;
  if (tokens >= 1_000) return `${(tokens / 1_000).toFixed(1)}K`;
  return tokens.toString();
}

/**
 * Format duration from milliseconds to human-readable string
 * Good for short durations (< 1 minute)
 */
export function formatDuration(ms: number): string {
  // Guard against invalid numbers
  if (!isFinite(ms) || isNaN(ms)) return "N/A";
  if (ms < 0) return "0ms";

  if (ms < 1000) return `${Math.round(ms)}ms`;
  if (ms < 60000) return `${(ms / 1000).toFixed(1)}s`;
  return `${Math.floor(ms / 60000)}m ${Math.floor((ms % 60000) / 1000)}s`;
}

/**
 * Format duration from milliseconds to human-readable string
 * Optimized for longer durations (minutes, hours, days)
 */
export function formatLongDuration(ms: number): string {
  // Guard against invalid numbers
  if (!isFinite(ms) || isNaN(ms)) return "N/A";
  if (ms < 0) return "0s";
  if (ms < 1000) return "< 1s";

  const seconds = Math.floor(ms / 1000);
  const minutes = Math.floor(seconds / 60);
  const hours = Math.floor(minutes / 60);
  const days = Math.floor(hours / 24);

  if (days > 0) {
    const remainingHours = hours % 24;
    if (remainingHours > 0) {
      return `${days}d ${remainingHours}h`;
    }
    return `${days}d`;
  }

  if (hours > 0) {
    const remainingMinutes = minutes % 60;
    if (remainingMinutes > 0) {
      return `${hours}h ${remainingMinutes}m`;
    }
    return `${hours}h`;
  }

  if (minutes > 0) {
    const remainingSeconds = seconds % 60;
    if (remainingSeconds > 0) {
      return `${minutes}m ${remainingSeconds}s`;
    }
    return `${minutes}m`;
  }

  return `${seconds}s`;
}

/**
 * Format timestamp to human-readable string
 */
export function formatTimestamp(timestamp: string): string {
  const date = new Date(timestamp);
  // Guard against invalid dates
  if (isNaN(date.getTime())) return "Invalid date";
  return date.toLocaleString();
}

export function formatBytes(bytes: number): string {
  // Guard against invalid numbers
  if (!isFinite(bytes) || isNaN(bytes)) return "N/A";
  if (bytes < 0) return "0 Bytes";
  if (bytes === 0) return "0 Bytes";

  const k = 1000;
  const sizes = ["Bytes", "KB", "MB", "GB", "TB"];
  const i = Math.floor(Math.log(bytes) / Math.log(k));

  // Guard against index out of bounds
  const sizeIndex = Math.min(i, sizes.length - 1);

  return (
    Math.round((bytes / Math.pow(k, sizeIndex)) * 100) / 100 +
    " " +
    sizes[sizeIndex]
  );
}

/**
 * Format latency in milliseconds to human-readable string
 */
export function formatLatency(ms?: number): string {
  if (!ms) return "N/A";
  if (ms >= 1000) {
    return `${(ms / 1000).toFixed(1)}s`;
  }
  return `${Math.round(ms)}ms`;
}

/**
 * Format a date string to relative time (e.g., "5m ago", "2h ago")
 */
export function formatRelativeTime(dateString?: string): string {
  if (!dateString) return "Never";

  const date = new Date(dateString);
  const now = new Date();
  const diffMs = now.getTime() - date.getTime();

  const diffMinutes = Math.floor(diffMs / (1000 * 60));
  const diffHours = Math.floor(diffMs / (1000 * 60 * 60));
  const diffDays = Math.floor(diffMs / (1000 * 60 * 60 * 24));

  if (diffMinutes < 1) return "Just now";
  if (diffMinutes < 60) return `${diffMinutes}m ago`;
  if (diffHours < 24) return `${diffHours}h ago`;
  if (diffDays < 7) return `${diffDays}d ago`;

  return date.toLocaleDateString();
}

/**
 * Format a single tariff price (stored as price per token) for display as price per million tokens
 * @param pricePerToken - Price per token as a decimal string (e.g., "0.000001")
 * @returns Formatted price per million tokens (e.g., "$1.00")
 */
export function formatTariffPrice(pricePerToken: string | number | null | undefined): string {
  if (!pricePerToken || pricePerToken === "0" || pricePerToken === 0) {
    return "$0";
  }

  // Convert from per-token to per-million tokens for display
  const pricePerMillion = parseFloat(String(pricePerToken)) * 1000000;

  // Format with 2 decimal places
  return `$${pricePerMillion.toFixed(2)}`;
}

/**
 * Format tariff pricing with both input and output prices per million tokens
 * @param inputPrice - Input price per token as a decimal string
 * @param outputPrice - Output price per token as a decimal string
 * @returns Formatted pricing string (e.g., "$1.50 / $2.00")
 */
export function formatTariffPricing(
  inputPrice: string | number | null | undefined,
  outputPrice: string | number | null | undefined
): string {
  return `${formatTariffPrice(inputPrice)} / ${formatTariffPrice(outputPrice)}`;
}
