export const DEFAULT_FALLBACK_STATUS_CODES = [
  429, 499, 500, 502, 503, 504,
];

export interface FallbackStatusFlags {
  originalStatuses: readonly number[];
  on429: boolean;
  on499: boolean;
  on404: boolean;
  on5xx: boolean;
}

const GROUPED_5XX_STATUS_CODES = [500, 502, 503, 504];

function is5xx(status: number): boolean {
  return status >= 500 && status < 600;
}

function reconcileStatus(
  statuses: number[],
  status: number,
  enabled: boolean,
): number[] {
  const wasEnabled = statuses.includes(status);
  if (wasEnabled === enabled) return statuses;
  return enabled
    ? [...statuses, status]
    : statuses.filter((candidate) => candidate !== status);
}

export function buildFallbackStatusCodes({
  originalStatuses,
  on429,
  on499,
  on404,
  on5xx,
}: FallbackStatusFlags): number[] {
  let statuses = [...originalStatuses];
  statuses = reconcileStatus(statuses, 429, on429);
  statuses = reconcileStatus(statuses, 499, on499);
  statuses = reconcileStatus(statuses, 404, on404);

  const originallyOn5xx = originalStatuses.some(is5xx);
  if (on5xx !== originallyOn5xx) {
    statuses = on5xx
      ? [
          ...statuses,
          ...GROUPED_5XX_STATUS_CODES.filter(
            (status) => !statuses.includes(status),
          ),
        ]
      : statuses.filter((status) => !is5xx(status));
  }

  return statuses;
}

export interface FallbackUpdateInput extends FallbackStatusFlags {
  fallbackEnabled: boolean;
  fallbackOnRateLimit: boolean;
  withReplacement: boolean;
  maxAttempts: number | null;
}

export interface FallbackUpdatePayload {
  fallback_enabled: boolean;
  fallback_on_rate_limit: boolean;
  fallback_on_status: number[];
  fallback_with_replacement: boolean;
  fallback_max_attempts: number | null;
}

export function buildFallbackUpdatePayload(
  input: FallbackUpdateInput,
): FallbackUpdatePayload {
  return {
    fallback_enabled: input.fallbackEnabled,
    fallback_on_rate_limit: input.fallbackEnabled
      ? input.fallbackOnRateLimit
      : false,
    fallback_on_status: buildFallbackStatusCodes(input),
    fallback_with_replacement: input.fallbackEnabled
      ? input.withReplacement
      : false,
    fallback_max_attempts: input.fallbackEnabled ? input.maxAttempts : null,
  };
}
