import { useSearchParams, useNavigate, Navigate, Link } from "react-router-dom";
import { useAuth } from "@/contexts/auth";
import {
  useInviteDetails,
  useAcceptInvite,
  useDeclineInvite,
} from "@/api/control-layer/hooks";
import { Button } from "@/components/ui/button";
import { toast } from "sonner";
import { Building, Loader2 } from "lucide-react";

export function AcceptInvite() {
  const [searchParams] = useSearchParams();
  const navigate = useNavigate();
  const token = searchParams.get("token") || "";
  const { isAuthenticated, isLoading: authLoading } = useAuth();

  const {
    data: invite,
    isLoading: inviteLoading,
    error: inviteError,
  } = useInviteDetails(token, { enabled: isAuthenticated });
  const acceptInvite = useAcceptInvite();
  const declineInvite = useDeclineInvite();

  // Loading auth state
  if (authLoading) {
    return (
      <div className="min-h-screen flex items-center justify-center bg-doubleword-background-secondary">
        <Loader2 className="h-8 w-8 animate-spin text-muted-foreground" />
      </div>
    );
  }

  // Not authenticated -- redirect to login with return path
  if (!isAuthenticated) {
    return (
      <Navigate
        to={`/login?redirect=${encodeURIComponent(`/org-invite?token=${token}`)}`}
        replace
      />
    );
  }

  // No token provided
  if (!token) {
    return (
      <div className="min-h-screen flex items-center justify-center bg-doubleword-background-secondary p-4">
        <div className="w-full max-w-md border rounded-lg bg-white dark:bg-doubleword-background-dark p-6 shadow-sm space-y-4 text-center">
          <h1 className="text-xl font-semibold">Invalid Invite</h1>
          <p className="text-sm text-muted-foreground">
            No invite token was provided. Please check the link and try again.
          </p>
          <Button variant="outline" asChild>
            <Link to="/">Go Home</Link>
          </Button>
        </div>
      </div>
    );
  }

  // Loading invite details
  if (inviteLoading) {
    return (
      <div className="min-h-screen flex items-center justify-center bg-doubleword-background-secondary">
        <Loader2 className="h-8 w-8 animate-spin text-muted-foreground" />
      </div>
    );
  }

  // Error fetching invite (expired, invalid, etc.)
  if (inviteError || !invite) {
    return (
      <div className="min-h-screen flex items-center justify-center bg-doubleword-background-secondary p-4">
        <div className="w-full max-w-md border rounded-lg bg-white dark:bg-doubleword-background-dark p-6 shadow-sm space-y-4 text-center">
          <h1 className="text-xl font-semibold">Invite Not Found</h1>
          <p className="text-sm text-muted-foreground">
            This invite may have expired or is no longer valid. Please contact
            the organization administrator for a new invite.
          </p>
          <Button variant="outline" asChild>
            <Link to="/">Go Home</Link>
          </Button>
        </div>
      </div>
    );
  }

  const handleAccept = async () => {
    try {
      await acceptInvite.mutateAsync(token);
      toast.success(`You've joined ${invite.org_name}`);
      navigate("/organizations");
    } catch (error) {
      toast.error(
        error instanceof Error ? error.message : "Failed to accept invite",
      );
    }
  };

  const handleDecline = async () => {
    try {
      await declineInvite.mutateAsync(token);
      toast.success("Invite declined");
      navigate("/");
    } catch (error) {
      toast.error(
        error instanceof Error ? error.message : "Failed to decline invite",
      );
    }
  };

  return (
    <div className="min-h-screen flex items-center justify-center bg-doubleword-background-secondary p-4">
      <div className="w-full max-w-md border rounded-lg bg-white dark:bg-doubleword-background-dark p-6 shadow-sm space-y-4 text-center">
        <div className="mx-auto h-12 w-12 rounded-full bg-muted flex items-center justify-center">
          <Building className="h-6 w-6 text-muted-foreground" />
        </div>
        <h1 className="text-xl font-semibold">Organization Invite</h1>
        <p className="text-sm text-muted-foreground">
          You've been invited to join{" "}
          <strong className="text-foreground">{invite.org_name}</strong> as a{" "}
          <strong className="text-foreground">{invite.role}</strong>.
        </p>
        {invite.inviter_name && (
          <p className="text-xs text-muted-foreground">
            Invited by {invite.inviter_name}
          </p>
        )}
        <div className="flex flex-col gap-2 pt-2">
          <Button
            onClick={handleAccept}
            disabled={acceptInvite.isPending || declineInvite.isPending}
          >
            {acceptInvite.isPending ? "Accepting..." : "Accept Invite"}
          </Button>
          <Button
            variant="ghost"
            onClick={handleDecline}
            disabled={acceptInvite.isPending || declineInvite.isPending}
          >
            {declineInvite.isPending ? "Declining..." : "Decline"}
          </Button>
        </div>
      </div>
    </div>
  );
}
