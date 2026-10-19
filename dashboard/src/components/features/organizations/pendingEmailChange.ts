import type { PendingEmailChange } from "@/api/control-layer/types";

/**
 * Addresses that still need to click their confirmation link before a
 * pending organization email change is applied. Order: current address
 * first, then the new one. A side whose timestamp is missing is treated as
 * outstanding, so an older backend that omits the fields reads as "both".
 */
export function waitingOn(
  pending: PendingEmailChange,
  currentEmail: string,
): string[] {
  const outstanding: string[] = [];
  if (!pending.old_email_confirmed_at) outstanding.push(currentEmail);
  if (!pending.new_email_confirmed_at) outstanding.push(pending.new_email);
  return outstanding;
}

/** Human-readable "waiting on …" clause for a pending email change. */
export function describeWaitingOn(
  pending: PendingEmailChange,
  currentEmail: string,
): string {
  const outstanding = waitingOn(pending, currentEmail);
  if (outstanding.length === 0) {
    return "both addresses have confirmed; applying";
  }
  if (outstanding.length === 1) {
    return `waiting on ${outstanding[0]} to confirm`;
  }
  return `waiting on both ${outstanding[0]} and ${outstanding[1]} to confirm`;
}
