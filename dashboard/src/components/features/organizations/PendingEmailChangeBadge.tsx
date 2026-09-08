import { MailWarning } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import type { PendingEmailChange } from "@/api/control-layer/types";
import { describeWaitingOn } from "./pendingEmailChange";

interface PendingEmailChangeBadgeProps {
  pending?: PendingEmailChange | null;
  /** The organization's current contact email (the "old" side). */
  currentEmail?: string | null;
}

/**
 * Flags an organization whose contact email change is still waiting on the
 * double opt-in verification, naming the address(es) that have yet to
 * confirm. Renders nothing when there is no pending change.
 */
export function PendingEmailChangeBadge({
  pending,
  currentEmail,
}: PendingEmailChangeBadgeProps) {
  if (!pending) return null;

  const expires = new Date(pending.expires_at).toLocaleString();
  const waiting = describeWaitingOn(pending, currentEmail ?? "current address");
  return (
    <Badge
      variant="outline"
      className="border-yellow-300 bg-yellow-50 text-yellow-800 whitespace-normal"
      title={`Both the current and the new address must confirm before the email changes. Links expire ${expires}.`}
    >
      <MailWarning />
      Pending change to {pending.new_email} · {waiting}
    </Badge>
  );
}
