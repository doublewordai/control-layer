import { useState } from "react";
import { useParams, useNavigate } from "react-router-dom";
import { useOrganization } from "@/api/control-layer/hooks";
import { Button } from "@/components/ui/button";
import { ArrowLeft } from "lucide-react";
import { MemberManagement } from "./MemberManagement";
import { EditOrganizationModal } from "./EditOrganizationModal";

export function OrganizationDetail() {
  const { organizationId } = useParams<{ organizationId: string }>();
  const navigate = useNavigate();
  const { data: org, isLoading } = useOrganization(organizationId!);
  const [showEditModal, setShowEditModal] = useState(false);

  if (isLoading) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="animate-spin rounded-full h-12 w-12 border-b-2 border-doubleword-accent-blue" />
      </div>
    );
  }

  if (!org) {
    return (
      <div className="p-6">
        <p className="text-muted-foreground">Organization not found.</p>
      </div>
    );
  }

  return (
    <div className="p-6 space-y-6">
      <div className="flex items-center gap-4">
        <Button
          variant="ghost"
          size="sm"
          onClick={() => navigate("/organizations")}
        >
          <ArrowLeft className="h-4 w-4 mr-2" />
          Back
        </Button>
      </div>

      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-bold tracking-tight">
            {org.display_name || org.username}
          </h1>
          <div className="flex items-center gap-3 mt-1 text-sm text-muted-foreground flex-wrap">
            <span>{org.email}</span>
            <span>·</span>
            <span>{org.member_count ?? 0} members</span>
            {org.credit_balance !== undefined && (
              <>
                <span>·</span>
                <span className="font-mono tabular-nums">${org.credit_balance.toFixed(2)}</span>
              </>
            )}
            <span>·</span>
            <span>Created {new Date(org.created_at).toLocaleDateString()}</span>
          </div>
        </div>
        <Button variant="outline" onClick={() => setShowEditModal(true)}>
          Edit
        </Button>
      </div>

      <MemberManagement organizationId={organizationId!} />

      <EditOrganizationModal
        isOpen={showEditModal}
        onClose={() => setShowEditModal(false)}
        organization={org}
      />
    </div>
  );
}
