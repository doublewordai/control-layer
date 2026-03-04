import { useState } from "react";
import { useCreateOrganization, useUsers } from "@/api/control-layer/hooks";
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
import { Combobox } from "@/components/ui/combobox";
import { useDebounce } from "@/hooks/useDebounce";
import { toast } from "sonner";

interface CreateOrganizationModalProps {
  isOpen: boolean;
  onClose: () => void;
  isPlatformManager?: boolean;
}

export function CreateOrganizationModal({
  isOpen,
  onClose,
  isPlatformManager = false,
}: CreateOrganizationModalProps) {
  const [name, setName] = useState("");
  const [email, setEmail] = useState("");
  const [displayName, setDisplayName] = useState("");
  const [ownerId, setOwnerId] = useState("");
  const [ownerSearch, setOwnerSearch] = useState("");
  const debouncedOwnerSearch = useDebounce(ownerSearch, 300);
  const createOrg = useCreateOrganization();

  const { data: usersData } = useUsers({
    search: debouncedOwnerSearch,
    limit: 20,
    enabled: isPlatformManager && isOpen,
  });

  const users = usersData?.data ?? [];

  const userOptions = users.map((user) => ({
    value: user.id,
    label: user.email,
  }));

  const handleOwnerChange = (value: string) => {
    setOwnerId(value);
    const selected = users.find((u) => u.id === value);
    if (selected) {
      setEmail(selected.email);
    } else {
      setEmail("");
    }
  };

  const handleSubmit = async (e: React.FormEvent) => {
    e.preventDefault();

    try {
      await createOrg.mutateAsync({
        name,
        email,
        display_name: displayName || undefined,
        owner_id: isPlatformManager && ownerId ? ownerId : undefined,
      });
      toast.success("Organization created successfully");
      handleClose();
    } catch (error) {
      toast.error(
        error instanceof Error ? error.message : "Failed to create organization",
      );
    }
  };

  const handleClose = () => {
    setName("");
    setEmail("");
    setDisplayName("");
    setOwnerId("");
    setOwnerSearch("");
    onClose();
  };

  return (
    <Dialog open={isOpen} onOpenChange={handleClose}>
      <DialogContent className="sm:max-w-[425px]">
        <DialogHeader>
          <DialogTitle>Create Organization</DialogTitle>
          <DialogDescription>
            Create a new organization. Members can be added after creation.
          </DialogDescription>
        </DialogHeader>
        <form onSubmit={handleSubmit}>
          <div className="grid gap-4 py-4">
            <div className="grid gap-2">
              <Label htmlFor="displayName">Organization Name</Label>
              <Input
                id="displayName"
                placeholder="Acme Corporation"
                value={displayName}
                onChange={(e) => setDisplayName(e.target.value)}
              />
            </div>
            <div className="grid gap-2">
              <Label htmlFor="name">Slug</Label>
              <Input
                id="name"
                placeholder="acme-corp"
                value={name}
                onChange={(e) => setName(e.target.value)}
                required
              />
              <p className="text-xs text-muted-foreground">
                Unique identifier (lowercase, hyphens allowed)
              </p>
            </div>
            {isPlatformManager && (
              <div className="grid gap-2">
                <Label>Owner</Label>
                <Combobox
                  options={userOptions}
                  value={ownerId}
                  onValueChange={handleOwnerChange}
                  onSearchChange={setOwnerSearch}
                  placeholder="Select owner..."
                  searchPlaceholder="Search users by email..."
                  emptyMessage="No users found."
                  className="w-full"
                  allowClear
                />
                <p className="text-xs text-muted-foreground">
                  User who will own this organization (defaults to you)
                </p>
              </div>
            )}
            <div className="grid gap-2">
              <Label htmlFor="email">Email</Label>
              <Input
                id="email"
                type="email"
                placeholder="admin@acme.com"
                value={email}
                onChange={(e) => setEmail(e.target.value)}
                required
              />
            </div>
          </div>
          <DialogFooter>
            <Button type="button" variant="outline" onClick={handleClose}>
              Cancel
            </Button>
            <Button type="submit" disabled={createOrg.isPending}>
              {createOrg.isPending ? "Creating..." : "Create"}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
