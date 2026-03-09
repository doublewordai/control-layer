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
import { toast } from "sonner";

interface EditOrganizationModalProps {
  isOpen: boolean;
  onClose: () => void;
  organization: Organization | null;
}

export function EditOrganizationModal({
  isOpen,
  onClose,
  organization,
}: EditOrganizationModalProps) {
  const [email, setEmail] = useState("");
  const [displayName, setDisplayName] = useState("");
  const updateOrg = useUpdateOrganization();

  useEffect(() => {
    if (organization) {
      setEmail(organization.email || "");
      setDisplayName(organization.display_name || "");
    }
  }, [organization]);

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!organization) return;

    try {
      await updateOrg.mutateAsync({
        id: organization.id,
        data: {
          display_name: displayName || undefined,
          email: email || undefined,
        },
      });
      toast.success("Organization updated successfully");
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
            </div>
            <div className="grid gap-2">
              <Label htmlFor="edit-displayName">Display Name</Label>
              <Input
                id="edit-displayName"
                value={displayName}
                onChange={(e) => setDisplayName(e.target.value)}
              />
            </div>
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
