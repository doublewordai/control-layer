export const DEFAULT_FALLBACK_STATUS_CODES = [
  429, 499, 500, 502, 503, 504,
];

export interface FallbackStatusFlags {
  on429: boolean;
  on499: boolean;
  on404: boolean;
  on5xx: boolean;
}

export function buildFallbackStatusCodes({
  on429,
  on499,
  on404,
  on5xx,
}: FallbackStatusFlags): number[] {
  const statuses: number[] = [];
  if (on429) statuses.push(429);
  if (on499) statuses.push(499);
  if (on404) statuses.push(404);
  if (on5xx) statuses.push(500, 502, 503, 504);
  return statuses;
}
