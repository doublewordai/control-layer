import { useState } from "react";
import {
  useOrganizationMembers,
  useInviteMember,
  useCancelInvite,
  useUpdateMemberRole,
  useRemoveMember,
} from "@/api/control-layer/hooks";
import type { OrgMemberRole, OrganizationMember } from "@/api/control-layer/types";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { UserAvatar } from "@/components/ui";
import { UserPlus, Trash2, Mail, X } from "lucide-react";
import { toast } from "sonner";

interface MemberManagementProps {
  organizationId: string;
  readOnly?: boolean;
}

export function MemberManagement({ organizationId, readOnly = false }: MemberManagementProps) {
  const { data: members = [], isLoading } =
    useOrganizationMembers(organizationId);
  const inviteMember = useInviteMember();
  const cancelInvite = useCancelInvite();
  const updateRole = useUpdateMemberRole();
  const removeMember = useRemoveMember();

  const [showAddForm, setShowAddForm] = useState(false);
  const [email, setEmail] = useState("");
  const [selectedRole, setSelectedRole] = useState<OrgMemberRole>("member");
  const [memberToRemove, setMemberToRemove] =
    useState<OrganizationMember | null>(null);

  const activeMembers = members.filter((m) => m.status === "active");
  const pendingInvites = members.filter((m) => m.status === "pending");

  const handleInviteMember = async () => {
    if (!email) return;

    try {
      await inviteMember.mutateAsync({
        orgId: organizationId,
        data: { email, role: selectedRole },
      });
      toast.success(`Invite sent to ${email}`);
      setShowAddForm(false);
      setEmail("");
      setSelectedRole("member");
    } catch (error) {
      toast.error(
        error instanceof Error ? error.message : "Failed to send invite",
      );
    }
  };

  const handleCancelInvite = async (member: OrganizationMember) => {
    try {
      await cancelInvite.mutateAsync({
        orgId: organizationId,
        inviteId: member.id,
      });
      toast.success("Invite cancelled");
    } catch (error) {
      toast.error(
        error instanceof Error ? error.message : "Failed to cancel invite",
      );
    }
  };

  const handleRoleChange = async (
    userId: string,
    newRole: OrgMemberRole,
  ) => {
    try {
      await updateRole.mutateAsync({
        orgId: organizationId,
        userId,
        role: newRole,
      });
      toast.success("Role updated");
    } catch (error) {
      toast.error(
        error instanceof Error ? error.message : "Failed to update role",
      );
    }
  };

  const handleRemoveMember = async () => {
    if (!memberToRemove || !memberToRemove.user) return;

    try {
      await removeMember.mutateAsync({
        orgId: organizationId,
        userId: memberToRemove.user.id,
      });
      toast.success("Member removed");
      setMemberToRemove(null);
    } catch (error) {
      toast.error(
        error instanceof Error ? error.message : "Failed to remove member",
      );
    }
  };

  if (isLoading) {
    return (
      <div className="flex items-center justify-center py-8">
        <div className="animate-spin rounded-full h-8 w-8 border-b-2 border-doubleword-accent-blue" />
      </div>
    );
  }

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between">
        <h3 className="text-lg font-semibold">
          Members ({activeMembers.length})
        </h3>
        {!readOnly && (
          <Button
            variant="outline"
            size="sm"
            onClick={() => setShowAddForm(!showAddForm)}
          >
            <UserPlus className="h-4 w-4 mr-2" />
            Invite Member
          </Button>
        )}
      </div>

      {showAddForm && (
        <div className="border rounded-lg p-4 space-y-3 bg-muted/30">
          <div className="grid gap-2">
            <Input
              type="email"
              placeholder="Enter email address..."
              value={email}
              onChange={(e) => setEmail(e.target.value)}
            />
          </div>
          <div className="flex items-center gap-2">
            <Select
              value={selectedRole}
              onValueChange={(v) => setSelectedRole(v as OrgMemberRole)}
            >
              <SelectTrigger className="w-32">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                <SelectItem value="member">Member</SelectItem>
                <SelectItem value="admin">Admin</SelectItem>
                <SelectItem value="owner">Owner</SelectItem>
              </SelectContent>
            </Select>
            <Button
              size="sm"
              onClick={handleInviteMember}
              disabled={!email || inviteMember.isPending}
            >
              {inviteMember.isPending ? "Sending..." : "Send Invite"}
            </Button>
            <Button
              size="sm"
              variant="ghost"
              onClick={() => {
                setShowAddForm(false);
                setEmail("");
                setSelectedRole("member");
              }}
            >
              Cancel
            </Button>
          </div>
        </div>
      )}

      {/* Active Members */}
      <div className="border rounded-lg divide-y">
        {activeMembers.map((member) =>
          member.user && (
            <div
              key={member.id}
              className="flex items-center justify-between px-4 py-3"
            >
              <div className="flex items-center gap-3">
                <UserAvatar user={member.user} size="md" />
                <div>
                  <p className="text-sm font-medium">
                    {member.user.display_name || member.user.username}
                  </p>
                  <p className="text-xs text-muted-foreground">
                    {member.user.email}
                  </p>
                </div>
              </div>
              <div className="flex items-center gap-2">
                {readOnly ? (
                  <span className="text-xs text-muted-foreground capitalize px-2 py-1 bg-muted rounded">
                    {member.role}
                  </span>
                ) : (
                  <>
                    <Select
                      value={member.role}
                      onValueChange={(v) =>
                        handleRoleChange(member.user!.id, v as OrgMemberRole)
                      }
                    >
                      <SelectTrigger className="w-28 h-8 text-xs">
                        <SelectValue />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectItem value="member">Member</SelectItem>
                        <SelectItem value="admin">Admin</SelectItem>
                        <SelectItem value="owner">Owner</SelectItem>
                      </SelectContent>
                    </Select>
                    <button
                      onClick={() => setMemberToRemove(member)}
                      className="h-8 w-8 p-0 rounded text-red-600 hover:text-red-700 hover:bg-red-50 transition-all flex items-center justify-center"
                      title="Remove member"
                    >
                      <Trash2 className="h-4 w-4" />
                    </button>
                  </>
                )}
              </div>
            </div>
          ),
        )}
        {activeMembers.length === 0 && (
          <div className="px-4 py-8 text-center text-muted-foreground">
            No members yet
          </div>
        )}
      </div>

      {/* Pending Invites */}
      {pendingInvites.length > 0 && (
        <div className="space-y-2">
          <h4 className="text-sm font-medium text-muted-foreground">
            Pending Invites ({pendingInvites.length})
          </h4>
          <div className="border rounded-lg divide-y">
            {pendingInvites.map((member) => (
              <div
                key={member.id}
                className="flex items-center justify-between px-4 py-3"
              >
                <div className="flex items-center gap-3">
                  <div className="h-8 w-8 rounded-full bg-muted flex items-center justify-center">
                    <Mail className="h-4 w-4 text-muted-foreground" />
                  </div>
                  <div>
                    <p className="text-sm font-medium">
                      {member.invite_email}
                    </p>
                    <span className="text-xs bg-amber-100 text-amber-800 dark:bg-amber-900/30 dark:text-amber-400 px-1.5 py-0.5 rounded">
                      Pending
                    </span>
                  </div>
                </div>
                <div className="flex items-center gap-2">
                  <span className="text-xs text-muted-foreground capitalize">
                    {member.role}
                  </span>
                  {!readOnly && (
                    <button
                      onClick={() => handleCancelInvite(member)}
                      className="h-8 w-8 p-0 rounded text-red-600 hover:text-red-700 hover:bg-red-50 transition-all flex items-center justify-center"
                      title="Cancel invite"
                      disabled={cancelInvite.isPending}
                    >
                      <X className="h-4 w-4" />
                    </button>
                  )}
                </div>
              </div>
            ))}
          </div>
        </div>
      )}

      <Dialog
        open={!!memberToRemove}
        onOpenChange={(open) => !open && setMemberToRemove(null)}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Remove Member</DialogTitle>
            <DialogDescription>
              Are you sure you want to remove{" "}
              <strong>
                {memberToRemove?.user?.display_name ||
                  memberToRemove?.user?.username}
              </strong>{" "}
              from this organization?
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setMemberToRemove(null)}>
              Cancel
            </Button>
            <Button
              variant="destructive"
              onClick={handleRemoveMember}
              disabled={removeMember.isPending}
            >
              {removeMember.isPending ? "Removing..." : "Remove"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
