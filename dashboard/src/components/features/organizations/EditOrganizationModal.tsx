import { useState, useEffect } from "react";
import { useUpdateOrganization } from "@/api/control-layer/hooks";
import type { Organization } from "@/api/control-layer/types";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { AlertBox } from "@/components/ui/alert-box";
import { describeWaitingOn } from "./pendingEmailChange";
import { toast } from "sonner";

interface EditOrganizationModalProps {
  isOpen: boolean;
  onClose: () => void;
  organization: Organization | null;
  /** Whether the current user may toggle zero data retention (admins only). */
  canEditZdr?: boolean;
}

export function EditOrganizationModal({
  isOpen,
  onClose,
  organization,
  canEditZdr = false,
}: EditOrganizationModalProps) {
  const [email, setEmail] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [zeroDataRetention, setZeroDataRetention] = useState(false);
  const updateOrg = useUpdateOrganization();

  useEffect(() => {
    if (organization) {
      setEmail(organization.email || "");
      setDisplayName(organization.display_name || "");
      setZeroDataRetention(organization.zero_data_retention ?? false);
    }
  }, [organization]);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!organization) return;

    const requestedEmail = email.trim();
    const emailChangeRequested =
      requestedEmail !== "" &&
      requestedEmail.toLowerCase() !==
        (organization.email ?? "").toLowerCase();

    try {
      const updated = await updateOrg.mutateAsync({
        id: organization.id,
        data: {
          display_name: displayName || undefined,
          email: requestedEmail || undefined,
          ...(canEditZdr
            ? { zero_data_retention: zeroDataRetention }
            : {}),
        },
      });
      if (emailChangeRequested && updated.pending_email_change) {
        // The backend never applies an email change directly: both the
        // current and the new mailbox must click a verification link first.
        // Saying "updated" here would hide that nothing has changed yet.
        const { new_email, expires_at } = updated.pending_email_change;
        toast.info("Email change pending verification", {
          description: `Confirmation links were sent to ${organization.email} and ${new_email}. The contact email will only update once both are confirmed (links expire ${new Date(expires_at).toLocaleString()}).`,
          duration: 12000,
        });
      } else {
        toast.success("Organization updated successfully");
      }
      onClose();
    } catch (error) {
      toast.error(
        error instanceof Error ? error.message : "Failed to update organization",
      );
    }
  };

  return (
    <Dialog open={isOpen} onOpenChange={onClose}>
      <DialogContent className="sm:max-w-[425px]">
        <DialogHeader>
          <DialogTitle>Edit Organization</DialogTitle>
          <DialogDescription>
            Update organization details for {organization?.username}.
          </DialogDescription>
        </DialogHeader>
        <form onSubmit={handleSubmit}>
          <div className="grid gap-4 py-4">
            <div className="grid gap-2">
              <Label htmlFor="edit-email">Email</Label>
              <Input
                id="edit-email"
                type="email"
                value={email}
                onChange={(e) => setEmail(e.target.value)}
              />
              <p className="text-xs text-muted-foreground">
                Changing the email sends a verification link to both the
                current and the new address. The change only takes effect once
                both are confirmed.
              </p>
            </div>
            {organization?.pending_email_change && (
              <AlertBox variant="warning">
                A change to{" "}
                <strong>{organization.pending_email_change.new_email}</strong>{" "}
                is pending —{" "}
                {describeWaitingOn(
                  organization.pending_email_change,
                  organization.email,
                )}{" "}
                (expires{" "}
                {new Date(
                  organization.pending_email_change.expires_at,
                ).toLocaleString()}
                ). Saving a different email restarts the verification.
              </AlertBox>
            )}
            <div className="grid gap-2">
              <Label htmlFor="edit-displayName">Display Name</Label>
              <Input
                id="edit-displayName"
                value={displayName}
                onChange={(e) => setDisplayName(e.target.value)}
              />
            </div>
            {canEditZdr && (
              <div className="flex items-center justify-between gap-4">
                <div className="grid gap-1">
                  <Label htmlFor="edit-zdr">Zero Data Retention</Label>
                  <p className="text-sm text-muted-foreground">
                    Applies to every API key owned by the organization.
                  </p>
                </div>
                <Switch
                  id="edit-zdr"
                  checked={zeroDataRetention}
                  onCheckedChange={setZeroDataRetention}
                  aria-label="Toggle zero data retention"
                />
              </div>
            )}
          </div>
          <DialogFooter>
            <Button type="button" variant="outline" onClick={onClose}>
              Cancel
            </Button>
            <Button type="submit" disabled={updateOrg.isPending}>
              {updateOrg.isPending ? "Saving..." : "Save"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
