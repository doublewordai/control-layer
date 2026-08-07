import { useState } from "react";
import { useNavigate } from "react-router-dom";
import { useMutation, useQueryClient } from "@tanstack/react-query";
import {
  useOrganizationMembers,
  useInviteMember,
  useCancelInvite,
  useRemoveMember,
  useLeaveOrganization,
  useUser,
} from "@/api/control-layer/hooks";
import { dwctlApi } from "@/api/control-layer/client";
import { queryKeys } from "@/api/control-layer/keys";
import { useOrganizationContext } from "@/contexts";
import type {
  InviteMemberRequest,
  OrgMemberRole,
  OrganizationMember,
  UpdateMemberRoleRequest,
} from "@/api/control-layer/types";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { Switch } from "@/components/ui/switch";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { UserAvatar } from "@/components/ui";
import { UserPlus, Trash2, Mail, X, LogOut, KeyRound } from "lucide-react";
import { toast } from "sonner";

const ROLE_OPTIONS: {
  value: OrgMemberRole;
  label: string;
  description: string;
}[] = [
  {
    value: "member",
    label: "Member",
    description:
      "Can create inference jobs and view their own usage. Cannot manage billing or other users.",
  },
  {
    value: "admin",
    label: "Admin",
    description:
      "Can invite members, manage API keys for all users, and view org-wide billing and workloads.",
  },
  {
    value: "owner",
    label: "Owner",
    description:
      "Full control over the organization, including members, billing, and settings.",
  },
];

interface RoleRadioCardsProps {
  idPrefix: string;
  value: OrgMemberRole;
  onValueChange: (role: OrgMemberRole) => void;
  includeOwner?: boolean;
  canManageKeys: boolean;
  onCanManageKeysChange: (value: boolean) => void;
}

function RoleRadioCards({
  idPrefix,
  value,
  onValueChange,
  includeOwner = false,
  canManageKeys,
  onCanManageKeysChange,
}: RoleRadioCardsProps) {
  const options = includeOwner
    ? ROLE_OPTIONS
    : ROLE_OPTIONS.filter((option) => option.value !== "owner");

  return (
    <RadioGroup
      value={value}
      onValueChange={(v) => onValueChange(v as OrgMemberRole)}
      aria-label="Role"
    >
      {options.map((option) => {
        const selected = value === option.value;
        return (
          <div
            key={option.value}
            className={`rounded-lg border p-3 ${
              selected
                ? "border-doubleword-accent-blue bg-blue-50/50"
                : "border-gray-200"
            }`}
          >
            <div className="flex items-start gap-3">
              <RadioGroupItem
                value={option.value}
                id={`${idPrefix}-role-${option.value}`}
                className="mt-0.5"
              />
              <div className="flex-1">
                <Label
                  htmlFor={`${idPrefix}-role-${option.value}`}
                  className="text-sm font-medium text-gray-900 cursor-pointer"
                >
                  {option.label}
                </Label>
                <p className="text-xs text-gray-500 mt-0.5">
                  {option.description}
                </p>
                {option.value === "member" && selected && (
                  <div className="flex items-center justify-between gap-2 mt-3 pt-3 border-t border-gray-200">
                    <Label
                      htmlFor={`${idPrefix}-can-manage-keys`}
                      className="text-sm text-gray-900 cursor-pointer"
                    >
                      Can generate API keys
                    </Label>
                    <Switch
                      id={`${idPrefix}-can-manage-keys`}
                      checked={canManageKeys}
                      onCheckedChange={onCanManageKeysChange}
                    />
                  </div>
                )}
              </div>
            </div>
          </div>
        );
      })}
    </RadioGroup>
  );
}

interface MemberManagementProps {
  organizationId: string;
  readOnly?: boolean;
}

export function MemberManagement({ organizationId, readOnly = false }: MemberManagementProps) {
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const { setActiveOrganization } = useOrganizationContext();
  const { data: members = [], isLoading } =
    useOrganizationMembers(organizationId);
  const { data: currentUser } = useUser("current");
  const inviteMember = useInviteMember();
  const cancelInvite = useCancelInvite();
  const removeMember = useRemoveMember();
  const leaveOrg = useLeaveOrganization();

  // Local mutation (rather than useUpdateMemberRole) so the request can carry
  // the optional can_manage_keys flag alongside the role.
  const updateMemberRole = useMutation({
    mutationKey: ["organizations", "updateMemberRole"],
    mutationFn: ({
      userId,
      data,
    }: {
      userId: string;
      data: UpdateMemberRoleRequest;
    }) => dwctlApi.organizations.updateMemberRole(organizationId, userId, data),
    onSettled: () => {
      queryClient.invalidateQueries({
        queryKey: queryKeys.organizations.members(organizationId),
      });
    },
  });

  // Invite modal state
  const [inviteOpen, setInviteOpen] = useState(false);
  const [inviteEmail, setInviteEmail] = useState("");
  const [inviteRole, setInviteRole] = useState<OrgMemberRole>("member");
  const [inviteCanManageKeys, setInviteCanManageKeys] = useState(false);

  // Role modal state
  const [roleMember, setRoleMember] = useState<OrganizationMember | null>(null);
  const [roleValue, setRoleValue] = useState<OrgMemberRole>("member");
  const [roleCanManageKeys, setRoleCanManageKeys] = useState(false);

  const [memberToRemove, setMemberToRemove] =
    useState<OrganizationMember | null>(null);
  const [showLeaveConfirm, setShowLeaveConfirm] = useState(false);

  const activeMembers = members.filter((m) => m.status === "active");
  const pendingInvites = members.filter((m) => m.status === "pending");

  const resetInviteForm = () => {
    setInviteEmail("");
    setInviteRole("member");
    setInviteCanManageKeys(false);
  };

  const handleInviteMember = async () => {
    if (!inviteEmail) return;

    const data: InviteMemberRequest = { email: inviteEmail, role: inviteRole };
    // can_manage_keys only applies to the member role (owners/admins are
    // implicitly allowed to manage keys).
    if (inviteRole === "member") {
      data.can_manage_keys = inviteCanManageKeys;
    }

    try {
      await inviteMember.mutateAsync({ orgId: organizationId, data });
      toast.success(`Invite sent to ${inviteEmail}`);
      setInviteOpen(false);
      resetInviteForm();
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

  const openRoleModal = (member: OrganizationMember) => {
    setRoleValue(member.role as OrgMemberRole);
    setRoleCanManageKeys(member.can_manage_keys);
    setRoleMember(member);
  };

  const handleSaveRole = async () => {
    if (!roleMember?.user) return;

    const data: UpdateMemberRoleRequest = { role: roleValue };
    if (roleValue === "member") {
      data.can_manage_keys = roleCanManageKeys;
    }

    try {
      await updateMemberRole.mutateAsync({
        userId: roleMember.user.id,
        data,
      });
      toast.success("Role updated");
      setRoleMember(null);
    } catch (error) {
      toast.error(
        error instanceof Error ? error.message : "Failed to update role",
      );
    }
  };

  const handleLeave = async () => {
    try {
      await leaveOrg.mutateAsync(organizationId);
      await setActiveOrganization(null);
      toast.success("You have left the organization");
      setShowLeaveConfirm(false);
      navigate("/");
    } catch (error) {
      toast.error(
        error instanceof Error ? error.message : "Failed to leave organization",
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
      <div className="bg-white rounded-lg shadow-sm border border-gray-200 p-6">
        <div className="flex items-center justify-between mb-4">
          <h4 className="text-lg font-medium text-gray-900">
            Members ({activeMembers.length})
          </h4>
          {!readOnly && (
            <Button
              variant="outline"
              size="sm"
              onClick={() => setInviteOpen(true)}
            >
              <UserPlus className="h-4 w-4 mr-2" />
              Invite Member
            </Button>
          )}
        </div>

        {/* Active Members */}
        <div className="divide-y divide-gray-200">
          {activeMembers.map((member) =>
            member.user && (
              <div
                key={member.id}
                className="flex items-center justify-between py-3"
              >
                <div className="flex items-center gap-3">
                  <UserAvatar user={member.user} size="md" />
                  <div>
                    <p className="text-sm font-medium text-gray-900">
                      {member.user.display_name || member.user.username}
                    </p>
                    <p className="text-xs text-gray-500">
                      {member.user.email}
                    </p>
                  </div>
                </div>
                <div className="flex items-center gap-2">
                  {member.can_manage_keys && (
                    <KeyRound
                      className="h-4 w-4 text-gray-400"
                      role="img"
                      aria-label="Can create API keys"
                    />
                  )}
                  {member.user?.id === currentUser?.id ? (
                    <>
                      <span className="text-xs text-muted-foreground capitalize px-2 py-1 bg-muted rounded">
                        {member.role}
                      </span>
                      <button
                        onClick={() => setShowLeaveConfirm(true)}
                        className="h-8 px-2 rounded text-red-600 hover:text-red-700 hover:bg-red-50 transition-all flex items-center gap-1 text-xs"
                        title="Leave organization"
                      >
                        <LogOut className="h-3.5 w-3.5" />
                        Leave
                      </button>
                    </>
                  ) : readOnly ? (
                    <span className="text-xs text-muted-foreground capitalize px-2 py-1 bg-muted rounded">
                      {member.role}
                    </span>
                  ) : (
                    <>
                      <Button
                        variant="outline"
                        size="sm"
                        className="h-8 text-xs capitalize"
                        onClick={() => openRoleModal(member)}
                        aria-label={`Change role for ${
                          member.user.display_name || member.user.username
                        }`}
                      >
                        {member.role}
                      </Button>
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
            <div className="py-8 text-center text-gray-500">
              No members yet
            </div>
          )}
        </div>

        {/* Pending Invites */}
        {pendingInvites.length > 0 && (
          <>
            <h5 className="text-sm font-medium text-gray-500 uppercase tracking-wide mt-6 mb-3">
              Pending Invites ({pendingInvites.length})
            </h5>
            <div className="divide-y divide-gray-200">
              {pendingInvites.map((member) => (
                <div
                  key={member.id}
                  className="flex items-center justify-between py-3"
                >
                  <div className="flex items-center gap-3">
                    <div className="h-8 w-8 rounded-full bg-gray-100 flex items-center justify-center">
                      <Mail className="h-4 w-4 text-gray-500" />
                    </div>
                    <div>
                      <p className="text-sm font-medium text-gray-900">
                        {member.invite_email}
                      </p>
                      <span className="text-xs bg-amber-100 text-amber-800 dark:bg-amber-900/30 dark:text-amber-400 px-1.5 py-0.5 rounded">
                        Pending
                      </span>
                    </div>
                  </div>
                  <div className="flex items-center gap-2">
                    <span className="text-xs text-gray-500 capitalize">
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
          </>
        )}
      </div>

      <Dialog
        open={inviteOpen}
        onOpenChange={(open) => {
          setInviteOpen(open);
          if (!open) resetInviteForm();
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Invite member</DialogTitle>
            <DialogDescription>
              Send an invitation to join this organization.
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-4">
            <div className="grid gap-2">
              <Label htmlFor="invite-email">Email</Label>
              <Input
                id="invite-email"
                type="email"
                placeholder="Enter email address..."
                value={inviteEmail}
                onChange={(e) => setInviteEmail(e.target.value)}
              />
            </div>
            <RoleRadioCards
              idPrefix="invite"
              value={inviteRole}
              onValueChange={setInviteRole}
              canManageKeys={inviteCanManageKeys}
              onCanManageKeysChange={setInviteCanManageKeys}
            />
          </div>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => {
                setInviteOpen(false);
                resetInviteForm();
              }}
            >
              Cancel
            </Button>
            <Button
              onClick={handleInviteMember}
              disabled={!inviteEmail || inviteMember.isPending}
            >
              {inviteMember.isPending ? "Sending..." : "Send Invite"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <Dialog
        open={!!roleMember}
        onOpenChange={(open) => !open && setRoleMember(null)}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Select Role</DialogTitle>
            <DialogDescription>
              Choose a role for{" "}
              <strong>
                {roleMember?.user?.display_name || roleMember?.user?.username}
              </strong>
              .
            </DialogDescription>
          </DialogHeader>
          <RoleRadioCards
            idPrefix="member-role"
            value={roleValue}
            onValueChange={setRoleValue}
            includeOwner
            canManageKeys={roleCanManageKeys}
            onCanManageKeysChange={setRoleCanManageKeys}
          />
          <DialogFooter>
            <Button variant="outline" onClick={() => setRoleMember(null)}>
              Cancel
            </Button>
            <Button
              onClick={handleSaveRole}
              disabled={updateMemberRole.isPending}
            >
              {updateMemberRole.isPending ? "Saving..." : "Save"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

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

      <Dialog
        open={showLeaveConfirm}
        onOpenChange={(open) => !open && setShowLeaveConfirm(false)}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Leave Organization</DialogTitle>
            <DialogDescription>
              Are you sure you want to leave this organization? Your
              organization API keys will be deleted.
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => setShowLeaveConfirm(false)}
            >
              Cancel
            </Button>
            <Button
              variant="destructive"
              onClick={handleLeave}
              disabled={leaveOrg.isPending}
            >
              {leaveOrg.isPending ? "Leaving..." : "Leave"}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
