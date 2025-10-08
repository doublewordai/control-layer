export function formatMemory(memoryGb: number): string {
  if (memoryGb >= 1000) {
    return `${(memoryGb / 1000).toFixed(1)}TB`;
  }
  return `${memoryGb}GB`;
}

export function formatNumber(num: number): string {
  if (num >= 1000000) return `${(num / 1000000).toFixed(1)}M`;
  if (num >= 1000) return `${(num / 1000).toFixed(0)}k`;
  return num.toString();
}

export function formatTokens(tokens: number): string {
  if (tokens >= 1000000) return `${(tokens / 1000000).toFixed(0)}M`;
  if (tokens >= 1000) return `${(tokens / 1000).toFixed(0)}k`;
  return tokens.toString();
}

/**
 * Format duration from milliseconds to human-readable string
 */
export function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60000) return `${(ms / 1000).toFixed(1)}s`;
  return `${Math.floor(ms / 60000)}m ${Math.floor((ms % 60000) / 1000)}s`;
}

/**
 * Format timestamp to human-readable string
 */
export function formatTimestamp(timestamp: string): string {
  return new Date(timestamp).toLocaleString();
}
