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
          <h1 className="text-2xl font-bold">
            {org.display_name || org.username}
          </h1>
          <p className="text-muted-foreground">{org.email}</p>
          {org.display_name && (
            <p className="text-sm text-muted-foreground">
              Slug: {org.username}
            </p>
          )}
        </div>
        <Button variant="outline" onClick={() => setShowEditModal(true)}>
          Edit
        </Button>
      </div>

      <div className="grid grid-cols-3 gap-4">
        <div className="border rounded-lg p-4">
          <p className="text-sm text-muted-foreground">Members</p>
          <p className="text-2xl font-bold">{org.member_count ?? "—"}</p>
        </div>
        <div className="border rounded-lg p-4">
          <p className="text-sm text-muted-foreground">Balance</p>
          <p className="text-2xl font-bold">
            {org.credit_balance !== undefined
              ? `$${org.credit_balance.toFixed(2)}`
              : "—"}
          </p>
        </div>
        <div className="border rounded-lg p-4">
          <p className="text-sm text-muted-foreground">Created</p>
          <p className="text-2xl font-bold">
            {new Date(org.created_at).toLocaleDateString()}
          </p>
        </div>
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
